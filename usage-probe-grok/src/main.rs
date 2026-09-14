//! `usage-probe-grok` — xAI Grok **account** probe.
//!
//! The weekly pool is one subscription, not one host. Chat, Voice, Imagine and
//! Build share it across every laptop, grok.com, and the phone apps.
//!
//! **1. Shared weekly pool** from `GET /v1/billing?format=credits`
//! (`creditUsagePercent`, `currentPeriod`). Product rows are *shares of that
//! pool*, not per-product quotas.
//!
//! **2. Extra Usage Credits** (`prepaidBalance`, cents → USD) and **Auto Top Up**.
//! Remaining dollars are remaining dollars. They are not remaining sessions.
//!
//! **3. This host's Build mix** from `~/.grok/sessions`, labelled as a sample.
//! The phone is invisible here and already counted in Chat/Voice/Imagine.
//!
//! Unformatted `GET /v1/billing` is fallback only. A 402 is `QuotaDenied`.

use std::path::PathBuf;
use std::process::ExitCode;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Measure, MeterScope, Monetary, Observation, Outcome, Resource,
    SideEffect,
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
    fn tokens(&self) -> u64 {
        self.input + self.output
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

fn grok_week_label(window_secs: Option<i64>) -> String {
    match window_secs {
        Some(s) if (6 * 86_400..8 * 86_400).contains(&s) => "Grok (shared weekly)".to_string(),
        Some(s) if s >= 86_400 => format!("Grok ({} days)", s / 86_400),
        Some(s) if s >= 3_600 => format!("Grok ({} hours)", s / 3_600),
        _ => "Grok (shared window)".to_string(),
    }
}

fn product_short_name(name: &str) -> String {
    match name {
        "GrokBuild" => "Build".into(),
        "GrokChat" => "Chat".into(),
        "GrokVoice" => "Voice".into(),
        "GrokImagine" => "Imagine".into(),
        other => other.trim_start_matches("Grok").to_string(),
    }
}

fn usd_from_cents(cents: f64) -> f64 {
    cents.abs() / 100.0
}

fn format_usd(dollars: f64) -> String {
    if (dollars - dollars.round()).abs() < 1e-9 {
        format!("${:.0}", dollars)
    } else {
        format!("${:.2}", dollars)
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M tokens", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k tokens", n / 1_000)
    } else {
        format!("{n} tokens")
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

/// The auto top-up rule, when one is configured.
///
/// Amounts arrive negative (they are charges) in cents. An earlier version
/// synthesised an "Included (before top-up)" row by treating
/// `minBeforeHittingSl` as a threshold on the *monthly allowance*. That was
/// withdrawn: the field watches the prepaid wallet, not the weekly pool.
struct TopupRule {
    enabled: bool,
    #[allow(dead_code)]
    trigger_at_remaining: f64,
    amount: f64,
    max_per_month: f64,
}

fn parse_topup(v: &serde_json::Value) -> Option<TopupRule> {
    let r = v.get("rule")?;
    let val = |k: &str| r.get(k).and_then(|x| x.get("val")).and_then(|x| x.as_f64());
    Some(TopupRule {
        enabled: r.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false),
        trigger_at_remaining: val("minBeforeHittingSl")?.abs(),
        amount: val("topupAmount").unwrap_or(0.0).abs(),
        max_per_month: val("maxAmountPerMonth").unwrap_or(0.0).abs(),
    })
}

/// The shared weekly pool across Chat, Voice, Imagine, and Build — every
/// laptop, grok.com, and the phone apps. This is the account meter.
fn shared_week(config: &serde_json::Value) -> Option<Resource> {
    let (_start, end, window_secs) = period_bounds(config);
    let utilization = config
        .get("creditUsagePercent")
        .and_then(|v| v.as_f64())
        .and_then(percent_as_utilization)?;

    Some(Resource {
        id: "grok-week".to_string(),
        label: grok_week_label(window_secs),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization: Some(utilization),
            resets_at: end,
            window_secs,
            expires_unused: Some(true),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: true,
    })
}

/// Per-product *share of the shared week*, not a product quota.
///
/// GrokBuild 71 + GrokChat 3 + GrokVoice 1 = creditUsagePercent 75. Putting
/// 71% through scarcity would read as "Build is 71% full". It is not: it is
/// 71 points of the one weekly pool, including phone Chat/Voice.
fn product_shares(config: &serde_json::Value) -> Vec<Resource> {
    let Some(products) = config.get("productUsage").and_then(|v| v.as_array()) else {
        return vec![];
    };
    products
        .iter()
        .filter_map(|product| {
            let name = product.get("product").and_then(|v| v.as_str())?;
            let short = product_short_name(name);
            let status = match product.get("usagePercent").and_then(|v| v.as_f64()) {
                Some(p) => format!("{p:.0}% of this week's shared pool"),
                None => "in this week's shared pool (no percent reported)".to_string(),
            };
            Some(Resource {
                id: format!("grok-share-{}", kebab_product(name)),
                label: format!("{short} · week share"),
                kind_hint: KindHint::Opaque,
                facets: Facets::default(),
                vendor_status: Some(status),
                vendor_representative: false,
            })
        })
        .collect()
}

fn prepaid_wallet(config: &serde_json::Value) -> Option<Resource> {
    let cents = wrapped_val(config, "prepaidBalance")?;
    let usd = usd_from_cents(cents.max(0.0));
    Some(Resource {
        id: "grok-monthly-credits".to_string(),
        label: "Extra Usage Credits".to_string(),
        kind_hint: KindHint::Consumption,
        facets: Facets {
            remaining: Some(Measure::new(usd, "USD")),
            monetary: Some(Monetary {
                currency: "USD".to_string(),
                spent: None,
                cap: None,
            }),
            expires_unused: Some(false),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: false,
    })
}

fn auto_topup_row(topup: &serde_json::Value) -> Option<Resource> {
    let rule = parse_topup(topup)?;
    let status = if rule.enabled {
        format!(
            "on · {} blocks · {}/month cap",
            format_usd(usd_from_cents(rule.amount)),
            format_usd(usd_from_cents(rule.max_per_month))
        )
    } else {
        "off".to_string()
    };
    Some(Resource {
        id: "grok-auto-topup".to_string(),
        label: "Auto Top Up".to_string(),
        kind_hint: KindHint::Opaque,
        facets: Facets {
            expires_unused: Some(false),
            ..Default::default()
        },
        vendor_status: Some(status),
        vendor_representative: false,
    })
}

/// Local CLI sessions on *this host*. Never remaining capacity: the phone,
/// the other machine, and grok.com are invisible here.
fn host_mix(totals: &Totals) -> Option<Resource> {
    if totals.sessions == 0 {
        return None;
    }
    let status = format!(
        "{} sessions · {} on this host this week — sample, not remaining capacity",
        totals.sessions,
        format_tokens(totals.tokens())
    );
    Some(Resource {
        id: "grok-host-mix".to_string(),
        label: "This host (Build mix)".to_string(),
        kind_hint: KindHint::Consumption,
        facets: Facets {
            consumed: Some(Measure::new(totals.sessions as f64, "sessions")),
            expires_unused: Some(false),
            ..Default::default()
        },
        vendor_status: Some(status),
        vendor_representative: false,
    })
}

/// Legacy unformatted `/v1/billing` shape (`monthlyLimit` / `used`).
///
/// After 2026-09-01 this account reported `used: 0` with `monthlyLimit` equal
/// to prepaid remaining, so callers must not treat that as 0% utilization
/// unless the credits-format payload is absent entirely.
fn monthly_resource(billing: &serde_json::Value) -> Option<Resource> {
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
            expires_unused: None,
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: true,
    })
}

fn account_label(config: &serde_json::Value) -> Option<String> {
    if config
        .get("isUnifiedBillingUser")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Some("unified".to_string());
    }
    Some("xai".to_string())
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
                    "402 Payment Required — Grok usage balance exhausted",
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
    let t = if root.exists() {
        scan(&root, since_unix)
    } else {
        Totals::default()
    };

    let mut resources = vec![];
    if let Some(b) = billing.as_ref() {
        if let Some(c) = b.get("config") {
            if let Some(r) = shared_week(c) {
                resources.push(r);
            }
            resources.extend(product_shares(c));
            if let Some(r) = prepaid_wallet(c) {
                resources.push(r);
            }
        }
        if let Some(r) = monthly_resource(b) {
            resources.push(r);
        }
    }
    if let Some(r) = auto_topup_row(&topup_note) {
        resources.push(r);
    }
    if let Some(r) = host_mix(&t) {
        resources.push(r);
    }

    if resources.is_empty() {
        return fail(
            FailureKind::Unknown,
            "no Grok account meter and no local Build sessions on this host",
        );
    }

    let mut obs = Observation::ok(
        PROBE,
        VERSION,
        PROVIDER,
        SideEffect::RequestConsuming,
        resources,
    );
    obs.assistant = Some("grok".to_string());
    obs.account = account.or_else(|| Some("xai".to_string()));
    obs.scope = MeterScope::Account;
    if let Outcome::Ok { raw, .. } = &mut obs.outcome {
        *raw = Some(serde_json::json!({
            "window": window_kind,
            "window_start_unix": since_unix,
            "note": "Account meter is /v1/billing?format=credits (the TUI /usage call): \
                     creditUsagePercent is the shared weekly pool across Chat, Voice, Imagine \
                     and Build on every device. Local ~/.grok/sessions is this host's Build \
                     mix only. Unformatted /v1/billing is fallback only.",
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

    /// Verbatim from GET /v1/billing?format=credits, 2026-09-14.
    /// Shared week is 75%; Build is 71 points of that pool, not a Build quota.
    fn live_credits_shape() -> serde_json::Value {
        serde_json::json!({"config":{
            "currentPeriod":{
                "type":"USAGE_PERIOD_TYPE_WEEKLY",
                "start":"2026-09-08T06:24:29.556273+00:00",
                "end":"2026-09-15T06:24:29.556273+00:00"
            },
            "creditUsagePercent":75.0,
            "onDemandCap":{"val":0},
            "onDemandUsed":{"val":0},
            "productUsage":[
                {"product":"GrokBuild","usagePercent":71.0},
                {"product":"GrokChat","usagePercent":3.0},
                {"product":"GrokVoice","usagePercent":1.0},
                {"product":"GrokImagine"}
            ],
            "isUnifiedBillingUser":true,
            "prepaidBalance":{"val":8542},
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
    fn a_disabled_rule_is_an_off_flag_not_silence() {
        let off = serde_json::json!({"rule":{"enabled": false, "minBeforeHittingSl":{"val":5000}}});
        let r = parse_topup(&off).expect("rule present");
        assert!(!r.enabled);
        let row = auto_topup_row(&off).expect("flag");
        assert_eq!(row.vendor_status.as_deref(), Some("off"));
        assert!(parse_topup(&serde_json::json!({})).is_none());
    }

    #[test]
    fn prepaid_wallet_is_dollars_with_no_session_estimate() {
        let c = live_credits_shape();
        let r = prepaid_wallet(c.get("config").unwrap()).expect("parsed");
        assert!(r.facets.work_units.is_empty(), "must not invent remaining sessions");
        assert_eq!(r.facets.remaining.as_ref().unwrap().unit, "USD");
        assert!((r.facets.remaining.as_ref().unwrap().value - 85.42).abs() < 1e-9);
        assert_eq!(r.label, "Extra Usage Credits");
        assert_eq!(r.facets.expires_unused, Some(false));
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
    fn credits_payload_yields_the_shared_week_not_build_alone() {
        let c = live_credits_shape();
        let r = shared_week(c.get("config").unwrap()).expect("parsed");
        let u = r.facets.utilization.expect("utilization");
        assert!(
            (u - 0.75).abs() < 1e-9,
            "got {u} — shared week is 75, not Build's 71"
        );
        assert_eq!(r.id, "grok-week");
        assert_eq!(r.label, "Grok (shared weekly)");
        assert_eq!(r.kind_hint, KindHint::ResetWindow);
        assert!(r.vendor_representative);
        assert!(r.facets.work_units.is_empty());
        assert_eq!(r.facets.expires_unused, Some(true));
        assert_eq!(r.facets.resets_at, Some(1_789_453_469));
        assert_eq!(r.facets.window_secs, Some(7 * 86_400));
    }

    #[test]
    fn product_shares_are_breakdown_not_quotas() {
        let c = live_credits_shape();
        let shares = product_shares(c.get("config").unwrap());
        assert_eq!(shares.len(), 4);
        assert_eq!(shares[0].id, "grok-share-grok-build");
        assert_eq!(shares[0].label, "Build · week share");
        assert_eq!(
            shares[0].vendor_status.as_deref(),
            Some("71% of this week's shared pool")
        );
        assert_eq!(shares[0].facets.utilization, None);
        assert_eq!(shares[0].kind_hint, KindHint::Opaque);
        assert_eq!(shares[1].label, "Chat · week share");
        assert_eq!(shares[2].label, "Voice · week share");
        assert_eq!(
            shares[3].vendor_status.as_deref(),
            Some("in this week's shared pool (no percent reported)")
        );
        let a = assess(&shares[0], &Policy::default(), 1_000_000, 0);
        assert_ne!(a.scarcity, AxisState::Approaching);
        assert_ne!(a.perishability, AxisState::Opportunity);
    }

    #[test]
    fn auto_topup_flag_names_the_dollar_blocks() {
        let row = auto_topup_row(&live_rule()).expect("flag");
        assert_eq!(row.id, "grok-auto-topup");
        assert_eq!(
            row.vendor_status.as_deref(),
            Some("on · $50 blocks · $100/month cap")
        );
    }

    #[test]
    fn host_mix_is_a_sample_not_remaining_capacity() {
        let mut t = Totals::default();
        t.sessions = 15;
        t.input = 1_152_502;
        t.output = 145_791;
        let r = host_mix(&t).expect("mix");
        assert_eq!(r.id, "grok-host-mix");
        assert!(r.facets.work_units.is_empty());
        let status = r.vendor_status.expect("status");
        assert!(status.contains("15 sessions"), "{status}");
        assert!(status.contains("this host"), "{status}");
        assert!(status.contains("not remaining capacity"), "{status}");
        assert!(host_mix(&Totals::default()).is_none());
    }

    #[test]
    fn credits_payload_does_not_paint_prepaid_as_zero_percent() {
        let c = live_credits_shape();
        let r = prepaid_wallet(c.get("config").unwrap()).expect("parsed");
        assert_eq!(r.facets.utilization, None, "remaining is not a 0% cap");
        assert_eq!(r.facets.limit, None);
        assert!(!r.vendor_representative);
    }

    #[test]
    fn credits_shape_is_not_parsed_as_the_legacy_monthly_row() {
        assert!(monthly_resource(&live_credits_shape()).is_none());
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
        let r = shared_week(c.get("config").unwrap()).expect("parsed");
        let now = r.facets.resets_at.unwrap() - 6 * 86_400;
        let a = assess(&r, &Policy::default(), now, 0);
        assert_eq!(a.scarcity, AxisState::Approaching, "75% is the approaching line");
        assert_eq!(a.perishability, AxisState::Healthy);
        assert_ne!(a.scarcity, AxisState::NotAssessable);
    }

    #[test]
    fn account_label_is_the_unified_account_not_build() {
        let c = live_credits_shape();
        assert_eq!(
            account_label(c.get("config").unwrap()).as_deref(),
            Some("unified")
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
        let r = monthly_resource(&live_monthly_shape()).expect("parsed");
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
        let r = monthly_resource(&live_monthly_shape()).expect("parsed");
        assert_eq!(r.facets.expires_unused, None);

        let two_days_before = r.facets.resets_at.unwrap() - 2 * 86_400;
        let a = assess(&r, &Policy::default(), two_days_before, 0);
        assert_ne!(a.perishability, AxisState::Opportunity);
        assert_eq!(a.perishability, AxisState::NotAssessable);
    }

    #[test]
    fn a_billing_payload_without_a_limit_is_not_invented() {
        let bad = serde_json::json!({"config":{"used":{"val":10}}});
        assert!(monthly_resource(&bad).is_none());
        assert!(monthly_resource(&serde_json::json!({})).is_none());
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
