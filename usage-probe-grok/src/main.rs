//! `usage-probe-grok` — xAI / Grok Build probe.
//!
//! Two resources, because xAI meters this account in two different ways.
//!
//! **1. Grok Build weekly pool — measurable since 2026-09-08.** The TUI `/usage`
//! modal calls `GET /v1/billing?format=credits`. That payload carries
//! `productUsage` (GrokBuild percent), `currentPeriod` (`USAGE_PERIOD_TYPE_WEEKLY`)
//! and `prepaidBalance`. This is the ceiling that returned `402 Grok Build
//! usage balance exhausted` on 2026-08-26 while the old monthly DTO still had
//! headroom.
//!
//! **2. Prepaid credits left.** `prepaidBalance` is remaining credits in the
//! wallet. It is not a utilization: the unformatted `GET /v1/billing` (no
//! query) started reporting `used: 0` and `monthlyLimit: <prepaid remaining>`
//! after the 1 September reset, which made the old parser paint 0% used as a
//! fact. That URL is retained only as a fallback.
//!
//! Local `~/.grok/sessions/**/updates.jsonl` still supplies the work-unit mix,
//! windowed from the vendor period start when we have one, otherwise an ISO-week
//! proxy. A 402 is reported as `QuotaDenied` rather than swallowed as a crash.

use std::path::PathBuf;
use std::process::ExitCode;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Measure, Observation, Outcome, Resource, SideEffect, WorkUnit,
};

/// The TUI `/usage` modal. This is the live Grok Build meter.
const BILLING_CREDITS_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// Pre-unification monthly DTO. After 2026-09-01 this account's `used` stuck at
/// 0 and `monthlyLimit` tracked prepaid remaining, so it is fallback only.
const BILLING_LEGACY_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing";
const TOPUP_URL: &str = "https://cli-chat-proxy.grok.com/v1/auto-topup-rule";

const PROBE: &str = "grok";
const PROVIDER: &str = "xai";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let obs = probe();
    println!(
        "{}",
        serde_json::to_string(&obs).expect("envelope serialises")
    );
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn fail(kind: FailureKind, msg: impl Into<String>) -> Observation {
    Observation::failure(PROBE, VERSION, PROVIDER, kind, msg)
}

/// Ticks per credit.
///
/// `costUsdTicks / 1e9` is a USD-like figure the TUI reports per turn. The
/// billing endpoint's `used` counter runs in credits, and the two reconcile at
/// this divisor: One month of observations summed to 702,130,641,560 ticks = 11,702
/// credits against a billed `used` of 11,558 — within 1.2%. So one credit is
/// roughly six cents of list-price inference.
///
/// Derived by reconciliation, not documented by the vendor. Treat the credit
/// figure as an estimate and the billing endpoint as authoritative.
const TICKS_PER_CREDIT: f64 = 6.0e7;

#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cached: u64,
    turns: u64,
    sessions: u64,
    calls: u64,
    ticks: u64,
}

impl Totals {
    /// Credits consumed per session, measured over this window.
    ///
    /// A session is a coarse unit — they range from two turns to hundreds — so
    /// the sample size travels with it and core presents the result as an
    /// estimate, never a promise.
    fn session_cost_credits(&self) -> Option<f64> {
        if self.sessions == 0 || self.ticks == 0 {
            return None;
        }
        Some((self.ticks as f64 / TICKS_PER_CREDIT) / self.sessions as f64)
    }

    /// Credits per token, at the mix actually observed.
    ///
    /// Not a published rate and not a constant: it depends heavily on how much
    /// of the context is cache-read, which for these sessions is around 93%. It
    /// is an honest answer to "how many tokens is my allowance worth" only for
    /// work shaped like the work already done.
    fn token_cost_credits(&self) -> Option<f64> {
        let tokens = self.input + self.output;
        if tokens == 0 || self.ticks == 0 {
            return None;
        }
        Some((self.ticks as f64 / TICKS_PER_CREDIT) / tokens as f64)
    }

    fn work_units(&self) -> Vec<WorkUnit> {
        let mut out = vec![];
        if let Some(cost) = self.token_cost_credits() {
            out.push(WorkUnit {
                label: "token".to_string(),
                cost,
                observed: self.sessions,
            });
        }
        if let Some(cost) = self.session_cost_credits() {
            out.push(WorkUnit {
                label: "session".to_string(),
                cost,
                observed: self.sessions,
            });
        }
        out
    }
}

/// Pull the usage block out of one `updates.jsonl` line.
///
/// The record is `params.update.usage` with **camelCase** fields. An earlier
/// version of this probe guessed `turn_completed.usage` with snake_case and
/// silently summed zero for every session — which is why `extract` is a named,
/// tested function rather than an inline chain.
fn extract(line: &str) -> Option<(serde_json::Value, i64)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let usage = v.pointer("/params/update/usage")?.clone();
    // Seconds, not millis — `_meta.agentTimestampMs` is the millisecond one.
    let ts = v.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);
    Some((usage, ts))
}

/// Walk `~/.grok/sessions/**/updates.jsonl`, summing completed turns since
/// `since_unix`.
fn scan(root: &PathBuf, since_unix: i64) -> Totals {
    let mut t = Totals::default();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) != Some("updates.jsonl") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut counted_session = false;
            for line in text.lines() {
                if !line.contains("turn_completed") {
                    continue;
                }
                // Filter on the record's own timestamp, not the file's mtime:
                // a long-lived session file spans weeks, so mtime would either
                // include the whole file or exclude all of it.
                let Some((usage, ts)) = extract(line) else {
                    continue;
                };
                if ts < since_unix {
                    continue;
                }
                t.turns += 1;
                if !counted_session {
                    t.sessions += 1;
                    counted_session = true;
                }
                let n = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                t.input += n("inputTokens");
                t.output += n("outputTokens");
                t.cached += n("cachedReadTokens");
                t.calls += n("modelCalls");
                t.ticks += n("costUsdTicks");
            }
        }
    }
    t
}

/// Pull the OIDC access token out of `~/.grok/auth.json`.
///
/// The file is keyed by `<issuer>::<client_id>`, so the entry is taken rather
/// than looked up by a hard-coded key. Only this process ever holds the token.
fn read_token() -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let path = format!("{home}/.grok/auth.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let doc: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{path} is not JSON: {e}"))?;
    doc.as_object()
        .and_then(|m| m.values().next())
        .and_then(|entry| entry.get("key"))
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "no OIDC access token in auth.json".to_string())
}

fn parse_rfc3339(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|d| d.timestamp())
}

fn iso_week_start_unix(now: chrono::DateTime<chrono::Utc>) -> i64 {
    now.date_naive()
        .week(chrono::Weekday::Mon)
        .first_day()
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp())
        .unwrap_or(0)
}

/// Vendor percent fields are 0..=100, matching Codex `usedPercent`.
///
/// Range-check rather than clamp. A 0..=1 fraction would otherwise render as
/// 16% used when the vendor meant 16 percent — or 0.16% when we guessed the
/// other way. Values outside 0..=100 are a scale change, not a reading.
fn percent_as_utilization(value: f64) -> Option<f64> {
    if (0.0..=100.0).contains(&value) && value.is_finite() {
        Some(value / 100.0)
    } else {
        None
    }
}

fn wrapped_val(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key)
        .and_then(|x| x.get("val"))
        .and_then(|x| x.as_f64())
        .or_else(|| v.get(key).and_then(|x| x.as_f64()))
}

fn kebab_product(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn period_bounds(config: &serde_json::Value) -> (Option<i64>, Option<i64>, Option<i64>) {
    let period = config.get("currentPeriod");
    let start = period
        .and_then(|p| p.get("start"))
        .and_then(|s| s.as_str())
        .and_then(parse_rfc3339)
        .or_else(|| {
            config
                .get("billingPeriodStart")
                .and_then(|s| s.as_str())
                .and_then(parse_rfc3339)
        });
    let end = period
        .and_then(|p| p.get("end"))
        .and_then(|s| s.as_str())
        .and_then(parse_rfc3339)
        .or_else(|| {
            config
                .get("billingPeriodEnd")
                .and_then(|s| s.as_str())
                .and_then(parse_rfc3339)
        });
    let window = match (start, end) {
        (Some(s), Some(e)) if e > s => Some(e - s),
        _ => None,
    };
    (start, end, window)
}

fn grok_build_label(window_secs: Option<i64>) -> String {
    match window_secs {
        Some(s) if (6 * 86_400..8 * 86_400).contains(&s) => "Grok Build (7 days)".to_string(),
        Some(s) if s >= 86_400 => format!("Grok Build ({} days)", s / 86_400),
        Some(s) if s >= 3_600 => format!("Grok Build ({} hours)", s / 3_600),
        _ => "Grok Build (window)".to_string(),
    }
}

fn window_start(
    config: Option<&serde_json::Value>,
    now: chrono::DateTime<chrono::Utc>,
) -> (i64, &'static str) {
    if let Some(c) = config {
        let (start, _, _) = period_bounds(c);
        if let Some(s) = start {
            return (s, "vendor-current-period");
        }
    }
    (iso_week_start_unix(now), "iso-week-proxy")
}

fn is_credits_shape(config: &serde_json::Value) -> bool {
    config.get("creditUsagePercent").is_some()
        || config.get("productUsage").is_some()
        || config.get("prepaidBalance").is_some()
        || config.get("currentPeriod").is_some()
}

/// The auto top-up rule, when one is configured and enabled.
///
/// **Retained as evidence, not used to derive a resource.** An earlier version
/// synthesised an "Included (before top-up)" row by treating
/// `minBeforeHittingSl` as a threshold on the *monthly allowance*, concluding
/// that purchasing began at 67.7% used. That was an unverified reading: the
/// field plausibly watches a prepaid balance instead, and the account also
/// reports `onDemandCap: 0`, which suggests on-demand billing may not be active
/// at all. Two readings fit the same data, so the row asserted a distinction
/// that may not exist and has been withdrawn. The rule travels in the raw
/// payload where it can be read without being interpreted.
#[cfg(test)]
struct TopupRule {
    trigger_at_remaining: f64,
    amount: f64,
    max_per_month: f64,
}

#[cfg(test)]
fn parse_topup(v: &serde_json::Value) -> Option<TopupRule> {
    let r = v.get("rule")?;
    if !r.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false) {
        return None;
    }
    let val = |k: &str| r.get(k).and_then(|x| x.get("val")).and_then(|x| x.as_f64());
    Some(TopupRule {
        trigger_at_remaining: val("minBeforeHittingSl")?.abs(),
        // Emitted negative: they are charges against the account.
        amount: val("topupAmount").unwrap_or(0.0).abs(),
        max_per_month: val("maxAmountPerMonth").unwrap_or(0.0).abs(),
    })
}

fn grok_build_week(config: &serde_json::Value, totals: &Totals) -> Option<Resource> {
    let (_start, end, window_secs) = period_bounds(config);
    let mut utilization = None;
    if let Some(products) = config.get("productUsage").and_then(|v| v.as_array()) {
        for product in products {
            let name = product
                .get("product")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name.eq_ignore_ascii_case("GrokBuild") || products.len() == 1 {
                utilization = product
                    .get("usagePercent")
                    .and_then(|v| v.as_f64())
                    .and_then(percent_as_utilization);
                break;
            }
        }
    }
    if utilization.is_none() {
        utilization = config
            .get("creditUsagePercent")
            .and_then(|v| v.as_f64())
            .and_then(percent_as_utilization);
    }
    let utilization = utilization?;

    Some(Resource {
        id: "grok-build-week".to_string(),
        label: grok_build_label(window_secs),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization: Some(utilization),
            work_units: totals.work_units(),
            resets_at: end,
            window_secs,
            // Weekly Grok Build allowance is lost at the reset. Auto-top-up
            // watches prepaid balance, not this pool, so "might as well" here
            // does not invite a purchase.
            expires_unused: Some(true),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: true,
    })
}

fn prepaid_balance(config: &serde_json::Value, totals: &Totals) -> Option<Resource> {
    let remaining = wrapped_val(config, "prepaidBalance")?;
    Some(Resource {
        id: "grok-monthly-credits".to_string(),
        label: "Credits left".to_string(),
        // Remaining is known; the wallet's ceiling is not in this payload.
        kind_hint: KindHint::Consumption,
        facets: Facets {
            remaining: Some(Measure::new(remaining.max(0.0), "credits")),
            work_units: totals.work_units(),
            // UNKNOWN: auto-top-up is enabled and may watch this balance.
            expires_unused: None,
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: false,
    })
}

/// Legacy unformatted `/v1/billing` shape (`monthlyLimit` / `used`).
///
/// After 2026-09-01 this account reported `used: 0` with `monthlyLimit` equal
/// to prepaid remaining, so callers must not treat that as 0% utilization
/// unless the credits-format payload is absent entirely.
fn monthly_resource(billing: &serde_json::Value, week: &Totals) -> Option<Resource> {
    let c = billing.get("config")?;
    if is_credits_shape(c) {
        return None;
    }
    let limit = wrapped_val(c, "monthlyLimit")?;
    let used = wrapped_val(c, "used")?;
    let start = c
        .get("billingPeriodStart")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339);
    let end = c
        .get("billingPeriodEnd")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339);

    Some(Resource {
        id: "grok-monthly-credits".to_string(),
        label: "Monthly allowance".to_string(),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization: if limit > 0.0 {
                Some((used / limit).clamp(0.0, 1.0))
            } else {
                None
            },
            consumed: Some(Measure::new(used, "credits")),
            remaining: Some(Measure::new((limit - used).max(0.0), "credits")),
            limit: Some(Measure::new(limit, "credits")),
            resets_at: end,
            window_secs: match (start, end) {
                (Some(s), Some(e)) => Some(e - s),
                _ => None,
            },
            work_units: week.work_units(),
            expires_unused: None,
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: true,
    })
}

fn consumption_only_week(totals: &Totals) -> Resource {
    Resource {
        id: "grok-build-week".to_string(),
        label: "Grok Build (ISO week)".to_string(),
        kind_hint: KindHint::Consumption,
        facets: Facets {
            consumed: Some(Measure::new(
                totals.ticks as f64 / TICKS_PER_CREDIT,
                "credits",
            )),
            work_units: totals.work_units(),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: false,
    }
}

fn account_label(config: &serde_json::Value) -> Option<String> {
    if let Some(products) = config.get("productUsage").and_then(|v| v.as_array()) {
        if let Some(name) = products
            .iter()
            .find(|p| {
                p.get("product")
                    .and_then(|v| v.as_str())
                    .is_some_and(|n| n.eq_ignore_ascii_case("GrokBuild"))
            })
            .or_else(|| products.first())
            .and_then(|p| p.get("product"))
            .and_then(|v| v.as_str())
        {
            return Some(kebab_product(name));
        }
    }
    if config
        .get("isUnifiedBillingUser")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Some("unified".to_string());
    }
    None
}

enum BillingFetch {
    Ok(serde_json::Value),
    Unavailable(serde_json::Value),
    Auth,
    Quota,
}

fn fetch_json(url: &str, token: &str) -> Result<String, ureq::Error> {
    ureq::get(url)
        .set("authorization", &format!("Bearer {token}"))
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .and_then(|resp| resp.into_string().map_err(ureq::Error::from))
}

fn parse_billing_body(text: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

fn fetch_billing(token: &str) -> BillingFetch {
    match fetch_json(BILLING_CREDITS_URL, token) {
        Ok(text) => match parse_billing_body(&text) {
            Ok(v) => return BillingFetch::Ok(v),
            Err(e) => {
                return BillingFetch::Unavailable(serde_json::json!({
                    "billing_unparseable": e,
                    "endpoint": BILLING_CREDITS_URL
                }))
            }
        },
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return BillingFetch::Auth
        }
        Err(ureq::Error::Status(402, _)) => return BillingFetch::Quota,
        Err(ureq::Error::Status(code, _)) if (500..600).contains(&code) => {
            // Fall through to the legacy DTO; a 5xx on credits is not proof
            // that the older shape is gone.
            let _ = code;
        }
        Err(ureq::Error::Status(404, _)) => {}
        Err(e) => {
            return BillingFetch::Unavailable(serde_json::json!({
                "billing_error": e.to_string(),
                "endpoint": BILLING_CREDITS_URL
            }))
        }
    }

    match fetch_json(BILLING_LEGACY_URL, token) {
        Ok(text) => match parse_billing_body(&text) {
            Ok(v) => BillingFetch::Ok(v),
            Err(e) => BillingFetch::Unavailable(serde_json::json!({
                "billing_unparseable": e,
                "endpoint": BILLING_LEGACY_URL
            })),
        },
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => BillingFetch::Auth,
        Err(ureq::Error::Status(402, _)) => BillingFetch::Quota,
        Err(ureq::Error::Status(code, _)) if (500..600).contains(&code) => {
            BillingFetch::Unavailable(serde_json::json!({ "billing_outage": code }))
        }
        Err(e) => BillingFetch::Unavailable(serde_json::json!({
            "billing_error": e.to_string(),
            "endpoint": BILLING_LEGACY_URL
        })),
    }
}

fn probe() -> Observation {
    let Ok(home) = std::env::var("HOME") else {
        return fail(FailureKind::Unknown, "HOME is not set");
    };
    let root = PathBuf::from(&home).join(".grok/sessions");
    if !root.exists() {
        return fail(
            FailureKind::Unknown,
            format!(
                "{} does not exist; Grok Build is not installed here",
                root.display()
            ),
        );
    }

    let now = chrono::Utc::now();
    let billing_note;
    let mut topup_note = serde_json::Value::Null;
    let mut account = None;

    let billing = match read_token() {
        Err(e) => {
            billing_note = serde_json::json!({ "billing_unavailable": e });
            None
        }
        Ok(token) => match fetch_billing(&token) {
            BillingFetch::Auth => {
                return fail(
                    FailureKind::InvalidCredentials,
                    "grok OIDC token expired or rejected — run `grok` once to refresh it",
                )
            }
            BillingFetch::Quota => {
                return fail(
                    FailureKind::QuotaDenied,
                    "402 Payment Required — Grok Build usage balance exhausted",
                )
            }
            BillingFetch::Unavailable(note) => {
                billing_note = note;
                None
            }
            BillingFetch::Ok(billing) => {
                if let Ok(resp) = fetch_json(TOPUP_URL, &token) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp) {
                        topup_note = v;
                    }
                }
                billing_note = billing.clone();
                if let Some(c) = billing.get("config") {
                    account = account_label(c);
                }
                Some(billing)
            }
        },
    };

    let config = billing.as_ref().and_then(|b| b.get("config"));
    let (since_unix, window_kind) = window_start(config, now);
    let t = scan(&root, since_unix);

    let mut resources = vec![];
    if let Some(b) = billing.as_ref() {
        if let Some(c) = b.get("config") {
            if let Some(r) = grok_build_week(c, &t) {
                resources.push(r);
            }
            if let Some(r) = prepaid_balance(c, &t) {
                resources.push(r);
            }
        }
        if let Some(r) = monthly_resource(b, &t) {
            resources.push(r);
        }
    }

    if !resources.iter().any(|r| r.id == "grok-build-week") {
        resources.push(consumption_only_week(&t));
    }

    let mut obs = Observation::ok(
        PROBE,
        VERSION,
        PROVIDER,
        SideEffect::RequestConsuming,
        resources,
    );
    obs.assistant = Some("grok-build".to_string());
    obs.account = account;
    if let Outcome::Ok { raw, .. } = &mut obs.outcome {
        *raw = Some(serde_json::json!({
            "window": window_kind,
            "window_start_unix": since_unix,
            "note": "Grok Build remaining is /v1/billing?format=credits (the TUI /usage call). \
                     Unformatted /v1/billing is fallback only — after 2026-09-01 it reported \
                     used=0 with monthlyLimit equal to prepaid remaining.",
            "turns": t.turns,
            "sessions": t.sessions,
            "model_calls": t.calls,
            "input_tokens": t.input,
            "output_tokens": t.output,
            "cached_read_tokens": t.cached,
            "cost_usd_ticks": t.ticks,
            "credits_estimate": t.ticks as f64 / TICKS_PER_CREDIT,
            "billing": billing_note,
            "auto_topup_rule": topup_note,
            "credit_unit": "cents - confirmed 2026-08-27 against the TUI balance ($41.31). Undocumented by xAI.",
        }));
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuum_usage_core::policy::{assess, AxisState, Policy};

    fn live_monthly_shape() -> serde_json::Value {
        // Verbatim shape from GET /v1/billing from a live observation, 2026-08-27.
        serde_json::json!({"config":{
            "monthlyLimit":{"val":15500},
            "used":{"val":11558},
            "onDemandCap":{"val":0},
            "billingPeriodStart":"2026-08-01T00:00:00+00:00",
            "billingPeriodEnd":"2026-09-01T00:00:00+00:00",
            "history":[]}})
    }

    /// Verbatim from GET /v1/billing?format=credits, 2026-09-08.
    fn live_credits_shape() -> serde_json::Value {
        serde_json::json!({"config":{
            "currentPeriod":{
                "type":"USAGE_PERIOD_TYPE_WEEKLY",
                "start":"2026-09-08T06:24:29.556273+00:00",
                "end":"2026-09-15T06:24:29.556273+00:00"
            },
            "creditUsagePercent":16.0,
            "onDemandCap":{"val":0},
            "onDemandUsed":{"val":0},
            "productUsage":[{"product":"GrokBuild","usagePercent":16.0}],
            "isUnifiedBillingUser":true,
            "prepaidBalance":{"val":3542},
            "topUpMethod":"TOP_UP_METHOD_SAVED_PAYMENT_METHOD",
            "billingPeriodStart":"2026-09-08T06:24:29.556273+00:00",
            "billingPeriodEnd":"2026-09-15T06:24:29.556273+00:00"
        }})
    }

    // A real line from ~/.grok/sessions/**/updates.jsonl, trimmed.
    const REAL_LINE: &str = r#"{"timestamp":1787829919,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"turn_completed","prompt_id":"p","stop_reason":"end_turn","usage":{"inputTokens":17191,"outputTokens":478,"totalTokens":17669,"cachedReadTokens":11520,"cacheCreationTokens":0,"reasoningTokens":433,"modelCalls":1,"apiDurationMs":7416,"costUsdTicks":33949000}}}}"#;

    #[test]
    fn extract_reads_the_real_record_shape() {
        // Regression guard. The first version of this probe looked for
        // `turn_completed.usage` with snake_case fields, matched nothing, and
        // reported zero consumption for every session without erroring.
        let (usage, ts) = extract(REAL_LINE).expect("usage extracted");
        assert_eq!(
            usage.get("inputTokens").and_then(|v| v.as_u64()),
            Some(17191)
        );
        assert_eq!(
            usage.get("costUsdTicks").and_then(|v| v.as_u64()),
            Some(33_949_000)
        );
        assert_eq!(ts, 1_787_829_919, "timestamp is seconds, not millis");
        assert!(extract("{}").is_none());
        assert!(extract("not json").is_none());
    }

    fn live_rule() -> serde_json::Value {
        serde_json::json!({"rule":{
            "enabled": true,
            "minBeforeHittingSl": {"val": 5000},
            "topupAmount": {"val": -5000},
            "maxAmountPerMonth": {"val": -10000}}})
    }

    #[test]
    fn topup_amounts_are_absolute_despite_arriving_negative() {
        let r = parse_topup(&live_rule()).expect("enabled rule");
        assert_eq!(r.trigger_at_remaining, 5000.0);
        assert_eq!(
            r.amount, 5000.0,
            "emitted as -5000; it is a charge, not a credit"
        );
        assert_eq!(r.max_per_month, 10000.0);
    }

    #[test]
    fn a_disabled_rule_yields_nothing() {
        let off = serde_json::json!({"rule":{"enabled": false, "minBeforeHittingSl":{"val":5000}}});
        assert!(parse_topup(&off).is_none());
        assert!(parse_topup(&serde_json::json!({})).is_none());
    }

    #[test]
    fn no_observations_means_no_work_estimate() {
        let c = live_credits_shape();
        let r = prepaid_balance(c.get("config").unwrap(), &Totals::default()).expect("parsed");
        assert!(r.facets.work_units.is_empty(), "must not divide by zero");
    }

    #[test]
    fn ticks_convert_to_credits_at_the_reconciled_rate() {
        let august_ticks = 702_130_641_560f64;
        let credits = august_ticks / TICKS_PER_CREDIT;
        assert!((credits - 11_702.0).abs() < 1.0, "got {credits}");
        let billed = 11_558.0;
        assert!(
            (credits - billed).abs() / billed < 0.02,
            "drifted from the billed figure"
        );
    }

    #[test]
    fn credits_payload_yields_weekly_utilization() {
        let c = live_credits_shape();
        let r = grok_build_week(c.get("config").unwrap(), &Totals::default()).expect("parsed");
        let u = r.facets.utilization.expect("utilization");
        assert!(
            (u - 0.16).abs() < 1e-9,
            "got {u} — 16.0 is percent, not a fraction"
        );
        assert_eq!(r.id, "grok-build-week");
        assert_eq!(r.label, "Grok Build (7 days)");
        assert_eq!(r.kind_hint, KindHint::ResetWindow);
        assert!(r.vendor_representative);
        assert_eq!(r.facets.expires_unused, Some(true));
        // Fractional-second RFC3339 from the vendor must survive.
        assert_eq!(r.facets.resets_at, Some(1_789_453_469));
        assert_eq!(r.facets.window_secs, Some(7 * 86_400));
    }

    #[test]
    fn credits_payload_does_not_paint_prepaid_as_zero_percent() {
        let c = live_credits_shape();
        let r = prepaid_balance(c.get("config").unwrap(), &Totals::default()).expect("parsed");
        assert_eq!(r.facets.utilization, None, "remaining is not a 0% cap");
        assert_eq!(r.facets.remaining.as_ref().unwrap().value, 3542.0);
        assert_eq!(r.facets.remaining.as_ref().unwrap().unit, "credits");
        assert_eq!(r.facets.limit, None);
        assert_eq!(r.label, "Credits left");
        assert!(!r.vendor_representative);
    }

    #[test]
    fn credits_shape_is_not_parsed_as_the_legacy_monthly_row() {
        assert!(monthly_resource(&live_credits_shape(), &Totals::default()).is_none());
    }

    #[test]
    fn vendor_percent_outside_0_100_is_refused() {
        assert!(percent_as_utilization(101.0).is_none());
        assert!(percent_as_utilization(-1.0).is_none());
        assert_eq!(percent_as_utilization(0.0), Some(0.0));
        assert_eq!(percent_as_utilization(100.0), Some(1.0));
        // 0.16 would be 0.16% if we treated 0..=1 as percent. Refuse that
        // reading only by documenting the 0..=100 scale; 0.16 is a legal 0.16%.
        assert_eq!(percent_as_utilization(0.16), Some(0.0016));
    }

    #[test]
    fn weekly_pool_is_assessable_and_perishable() {
        let c = live_credits_shape();
        let r = grok_build_week(c.get("config").unwrap(), &Totals::default()).expect("parsed");
        let now = r.facets.resets_at.unwrap() - 6 * 86_400;
        let a = assess(&r, &Policy::default(), now, 0);
        assert_eq!(a.scarcity, AxisState::Healthy);
        assert_eq!(a.perishability, AxisState::Healthy);
        assert_ne!(a.scarcity, AxisState::NotAssessable);
    }

    #[test]
    fn account_label_from_grok_build_product() {
        let c = live_credits_shape();
        assert_eq!(
            account_label(c.get("config").unwrap()).as_deref(),
            Some("grok-build")
        );
    }

    #[test]
    fn window_start_prefers_vendor_period() {
        let c = live_credits_shape();
        let now = chrono::DateTime::from_timestamp(1_788_874_254, 0)
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (start, kind) = window_start(c.get("config"), now);
        assert_eq!(kind, "vendor-current-period");
        assert_eq!(start, 1_788_848_669);
        let (iso, iso_kind) = window_start(None, now);
        assert_eq!(iso_kind, "iso-week-proxy");
        assert_eq!(iso, 1_788_739_200, "Monday 2026-09-07 00:00 UTC");
        assert!(iso < start, "ISO week Monday is not the vendor week");
    }

    #[test]
    fn monthly_allowance_yields_real_remaining() {
        let r = monthly_resource(&live_monthly_shape(), &Totals::default()).expect("parsed");
        let u = r.facets.utilization.expect("utilization");
        assert!((u - 11558.0 / 15500.0).abs() < 1e-9, "got {u}");
        assert_eq!(
            r.facets.remaining.as_ref().unwrap().value,
            15500.0 - 11558.0
        );
        assert_eq!(r.facets.limit.as_ref().unwrap().value, 15500.0);
        assert_eq!(r.facets.resets_at, Some(1788220800));
        assert_eq!(r.facets.window_secs, Some(31 * 86_400));
    }

    #[test]
    fn the_monthly_allowance_never_invites_spending() {
        let r = monthly_resource(&live_monthly_shape(), &Totals::default()).expect("parsed");
        assert_eq!(r.facets.expires_unused, None);

        let two_days_before = r.facets.resets_at.unwrap() - 2 * 86_400;
        let a = assess(&r, &Policy::default(), two_days_before, 0);
        assert_ne!(a.perishability, AxisState::Opportunity);
        assert_eq!(a.perishability, AxisState::NotAssessable);
    }

    #[test]
    fn a_billing_payload_without_a_limit_is_not_invented() {
        let bad = serde_json::json!({"config":{"used":{"val":10}}});
        assert!(monthly_resource(&bad, &Totals::default()).is_none());
        assert!(monthly_resource(&serde_json::json!({}), &Totals::default()).is_none());
    }

    #[test]
    fn consumption_only_resource_is_never_healthy() {
        let r = Resource {
            id: "grok-build-week".into(),
            label: "Grok".into(),
            kind_hint: KindHint::Consumption,
            facets: Facets {
                consumed: Some(Measure::new(1_000.0, "tokens")),
                ..Default::default()
            },
            vendor_status: None,
            vendor_representative: false,
        };
        let a = assess(&r, &Policy::default(), 1_000_000, 0);
        assert_eq!(a.scarcity, AxisState::NotAssessable);
        assert_ne!(a.scarcity, AxisState::Inapplicable);
        assert_ne!(a.scarcity, AxisState::Healthy);
    }

    #[test]
    fn kebab_splits_product_camel_case() {
        assert_eq!(kebab_product("GrokBuild"), "grok-build");
        assert_eq!(kebab_product("unified"), "unified");
    }
}
