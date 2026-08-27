//! `usage-probe-grok` — xAI / Grok Build probe.
//!
//! Two resources, because xAI meters this account in two different ways and
//! only one of them is readable.
//!
//! **1. Monthly credit allowance — measurable.** `GET /v1/billing` on
//! `cli-chat-proxy.grok.com`, authenticated with the OIDC access token the CLI
//! stores in `~/.grok/auth.json`, returns `monthlyLimit`, `used`, `onDemandCap`
//! and the billing period. That is real remaining, and it is what the TUI's
//! `/usage` modal renders.
//!
//! **2. The shorter rate pool — still not measurable.** On 2026-08-26 this
//! account returned `402 Payment Required: Grok Build usage balance exhausted`
//! mid-forum-round while the *monthly* meter still had headroom (it cannot have
//! fallen since: the period runs 1 Aug to 1 Sep with no reset between). So a
//! second, shorter ceiling exists and no endpoint found exposes it — probing
//! `/v1/{rate_limits,limits,quota,subscription,entitlements,credits}` all
//! return 404.
//!
//! Consumption for that pool is derivable from `turn_completed.usage` records
//! in `~/.grok/sessions/**/updates.jsonl`, so the probe reports `consumed` with
//! no ceiling and core renders scarcity as **not assessable** — not
//! "inapplicable", and emphatically not "healthy". A ceiling demonstrably
//! exists; only our sight of it is missing.
//!
//! The one time that pool *is* legible is the moment it bites, and a 402 is
//! reported as `QuotaDenied` rather than swallowed as a crash.

use std::path::PathBuf;
use std::process::ExitCode;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Measure, Observation, Outcome, Resource, SideEffect,
};

const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing";

const PROBE: &str = "grok";
const PROVIDER: &str = "xai";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let obs = probe();
    println!("{}", serde_json::to_string(&obs).expect("envelope serialises"));
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn fail(kind: FailureKind, msg: impl Into<String>) -> Observation {
    Observation::failure(PROBE, VERSION, PROVIDER, kind, msg)
}

#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cached: u64,
    turns: u64,
    sessions: u64,
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
            // Cheap pre-filter: a file untouched since the cutoff holds nothing new.
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH) {
                        if (age.as_secs() as i64) < since_unix {
                            continue;
                        }
                    }
                }
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut counted_session = false;
            for line in text.lines() {
                if !line.contains("turn_completed") {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let Some(usage) = v.pointer("/turn_completed/usage") else {
                    continue;
                };
                t.turns += 1;
                if !counted_session {
                    t.sessions += 1;
                    counted_session = true;
                }
                let n = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                t.input += n("input_tokens");
                t.output += n("output_tokens");
                t.cached += n("cached_read_tokens") + n("cache_read_input_tokens");
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
    chrono::DateTime::parse_from_rfc3339(value).ok().map(|d| d.timestamp())
}

/// The monthly included allowance, from the billing endpoint.
fn monthly_resource(billing: &serde_json::Value) -> Option<Resource> {
    let c = billing.get("config")?;
    let val = |k: &str| c.get(k).and_then(|v| v.get("val")).and_then(|v| v.as_f64());
    let limit = val("monthlyLimit")?;
    let used = val("used")?;
    let start = c.get("billingPeriodStart").and_then(|v| v.as_str()).and_then(parse_rfc3339);
    let end = c.get("billingPeriodEnd").and_then(|v| v.as_str()).and_then(parse_rfc3339);

    Some(Resource {
        id: "grok-monthly-credits".to_string(),
        label: "Monthly allowance".to_string(),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization: if limit > 0.0 { Some((used / limit).clamp(0.0, 1.0)) } else { None },
            consumed: Some(Measure::new(used, "credits")),
            remaining: Some(Measure::new((limit - used).max(0.0), "credits")),
            limit: Some(Measure::new(limit, "credits")),
            resets_at: end,
            window_secs: match (start, end) { (Some(s), Some(e)) => Some(e - s), _ => None },
            // An included monthly allowance does not roll over. With
            // `onDemandCap` at 0 there are no purchased credits behind it
            // either, so unspent allowance is simply lost at the period end —
            // which is exactly what makes "might as well" meaningful here.
            expires_unused: Some(true),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: true,
    })
}

fn probe() -> Observation {
    let Ok(home) = std::env::var("HOME") else {
        return fail(FailureKind::Unknown, "HOME is not set");
    };
    let root = PathBuf::from(&home).join(".grok/sessions");
    if !root.exists() {
        return fail(
            FailureKind::Unknown,
            format!("{} does not exist; Grok Build is not installed here", root.display()),
        );
    }

    // ISO weeks are a *proxy* for the vendor's reset, which may fall on another
    // weekday. Labelled as such rather than presented as the real window.
    let now = chrono::Utc::now();
    let week_start = now
        .date_naive()
        .week(chrono::Weekday::Mon)
        .first_day()
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp())
        .unwrap_or(0);

    let t = scan(&root, week_start);

    // -- the readable half: monthly credits ---------------------------------
    let mut resources = vec![];
    let mut billing_note = serde_json::Value::Null;
    match read_token() {
        Err(e) => billing_note = serde_json::json!({ "billing_unavailable": e }),
        Ok(token) => {
            let req = ureq::get(BILLING_URL)
                .set("authorization", &format!("Bearer {token}"))
                .timeout(std::time::Duration::from_secs(15));
            match req.call() {
                Ok(resp) => match resp.into_string().map_err(|e| e.to_string()).and_then(
                    |t| serde_json::from_str::<serde_json::Value>(&t).map_err(|e| e.to_string()),
                ) {
                    Ok(billing) => {
                        if let Some(r) = monthly_resource(&billing) {
                            resources.push(r);
                        }
                        billing_note = billing;
                    }
                    Err(e) => billing_note = serde_json::json!({ "billing_unparseable": e }),
                },
                // The OIDC token is short-lived (about an hour). Expiry is the
                // ordinary case, not a fault in the account — say so plainly.
                Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
                    return fail(
                        FailureKind::InvalidCredentials,
                        "grok OIDC token expired or rejected — run `grok` once to refresh it",
                    )
                }
                Err(ureq::Error::Status(402, _)) => {
                    return fail(
                        FailureKind::QuotaDenied,
                        "402 Payment Required — Grok Build usage balance exhausted",
                    )
                }
                Err(ureq::Error::Status(code, _)) if (500..600).contains(&code) => {
                    billing_note = serde_json::json!({ "billing_outage": code })
                }
                Err(e) => billing_note = serde_json::json!({ "billing_error": e.to_string() }),
            }
        }
    }

    // -- the unreadable half: the shorter rate pool --------------------------
    let resource = Resource {
        id: "grok-build-week".to_string(),
        label: "Grok Build (ISO week)".to_string(),
        // Consumption is observable; capacity is not. The hint is what tells
        // core to say "cannot measure" instead of inventing a percentage.
        kind_hint: KindHint::Consumption,
        facets: Facets {
            consumed: Some(Measure::new((t.input + t.output) as f64, "tokens")),
            // remaining, limit, utilization: deliberately absent. The vendor
            // exposes no ceiling locally, so we assert none.
            //
            // expires_unused stays None rather than false: the weekly pool
            // almost certainly does perish, we simply cannot see it. `false`
            // would be a claim we have not earned.
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: false,
    };

    resources.push(resource);

    let mut obs = Observation::ok(
        PROBE,
        VERSION,
        PROVIDER,
        // One HTTP GET for billing plus local file reads. The GET costs a
        // request but no metered allowance, so it is not `quota-consuming`.
        SideEffect::RequestConsuming,
        resources,
    );
    obs.assistant = Some("grok-build".to_string());
    if let Outcome::Ok { raw, .. } = &mut obs.outcome {
        *raw = Some(serde_json::json!({
            "window": "iso-week-proxy",
            "window_start_unix": week_start,
            "note": "ISO week is a proxy; the SuperGrok pool may reset on another weekday. \
                     Remaining balance is not exposed locally — read /usage in the TUI.",
            "turns": t.turns,
            "sessions": t.sessions,
            "input_tokens": t.input,
            "output_tokens": t.output,
            "cached_read_tokens": t.cached,
            "billing": billing_note,
        }));
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuum_usage_core::policy::{assess, AxisState, Policy};

    fn live_shape() -> serde_json::Value {
        // Verbatim shape from GET /v1/billing on nimbini, 2026-08-27.
        serde_json::json!({"config":{
            "monthlyLimit":{"val":15500},
            "used":{"val":11558},
            "onDemandCap":{"val":0},
            "billingPeriodStart":"2026-08-01T00:00:00+00:00",
            "billingPeriodEnd":"2026-09-01T00:00:00+00:00",
            "history":[]}})
    }

    #[test]
    fn monthly_allowance_yields_real_remaining() {
        let r = monthly_resource(&live_shape()).expect("parsed");
        let u = r.facets.utilization.expect("utilization");
        assert!((u - 11558.0 / 15500.0).abs() < 1e-9, "got {u}");
        assert_eq!(r.facets.remaining.as_ref().unwrap().value, 15500.0 - 11558.0);
        assert_eq!(r.facets.limit.as_ref().unwrap().value, 15500.0);
        // The period boundaries survive as a real window.
        assert_eq!(r.facets.resets_at, Some(1788220800));
        assert_eq!(r.facets.window_secs, Some(31 * 86_400));
    }

    #[test]
    fn monthly_allowance_perishes_so_might_as_well_applies() {
        let r = monthly_resource(&live_shape()).expect("parsed");
        assert_eq!(r.facets.expires_unused, Some(true));

        // Two days before the period ends, still under 80% used: surplus that
        // will be lost. This is precisely the "might as well" case.
        let two_days_before = r.facets.resets_at.unwrap() - 2 * 86_400;
        let a = assess(&r, &Policy::default(), two_days_before, 0);
        assert_eq!(a.perishability, AxisState::Opportunity);
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
        // The 402 of 2026-08-26 proves a ceiling exists; claiming the axis does
        // not apply would be the lie this design is built to prevent.
        assert_ne!(a.scarcity, AxisState::Inapplicable);
        assert_ne!(a.scarcity, AxisState::Healthy);
    }
}
