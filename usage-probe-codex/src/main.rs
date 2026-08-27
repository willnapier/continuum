//! `usage-probe-codex` — OpenAI Codex (ChatGPT plan) probe.
//!
//! Migration of the shipped v1 monitor. It reuses the tested app-server call in
//! `continuum-core` (`account/rateLimits/read`) and maps the result into the v2
//! envelope.
//!
//! The mapping is where the old schema's flaw shows: v1 hoisted `primary` to
//! top-level `used_percent`/`resets_at` fields and shoved `secondary` into an
//! untyped `Value`, because the shape could only hold one window. Here both are
//! ordinary resources, and neither is privileged.

use std::process::ExitCode;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Measure, Monetary, Observation, Outcome, Resource, SideEffect,
};
use serde_json::Value;

const PROBE: &str = "codex";
const PROVIDER: &str = "openai";
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

/// Map one `{usedPercent, windowDurationMins, resetsAt}` block to a resource.
fn window(raw: &Value, key: &str, id: &str, label: &str, representative: bool) -> Option<Resource> {
    let block = raw.get(key)?.as_object()?;
    let used = block.get("usedPercent").and_then(Value::as_f64)?;
    let mins = block.get("windowDurationMins").and_then(Value::as_i64);
    let resets_at = block.get("resetsAt").and_then(Value::as_i64);

    Some(Resource {
        id: id.to_string(),
        label: label.to_string(),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization: Some((used / 100.0).clamp(0.0, 1.0)),
            resets_at,
            window_secs: mins.map(|m| m * 60),
            // Plan allowance not spent before the reset is simply gone.
            expires_unused: Some(true),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: representative,
    })
}

/// Purchased credits, when the account holds any. Money, so nothing perishes:
/// spending a credit balance faster is never an opportunity.
fn credits(raw: &Value) -> Option<Resource> {
    let c = raw.get("credits")?.as_object()?;
    if !c.get("hasCredits").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let balance = c
        .get("balance")
        .and_then(|v| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64()));
    Some(Resource {
        id: "codex-credits".to_string(),
        label: "Purchased credits".to_string(),
        kind_hint: KindHint::Continuous,
        facets: Facets {
            remaining: balance.map(|b| Measure::new(b, "USD")),
            expires_unused: Some(false),
            monetary: Some(Monetary {
                currency: "USD".to_string(),
                spent: None,
                cap: None,
            }),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: false,
    })
}

fn probe() -> Observation {
    let v1 = match continuum_core::usage::refresh_codex_usage("usage-probe-codex") {
        Ok(o) => o,
        Err(e) => {
            let msg = e.to_string();
            // Classify rather than collapsing everything into "it broke".
            let kind = if msg.contains("timed out") {
                FailureKind::ProviderOutage
            } else if msg.to_lowercase().contains("auth") || msg.contains("401") {
                FailureKind::InvalidCredentials
            } else if msg.contains("omitted") {
                FailureKind::MalformedResponse
            } else {
                FailureKind::Unknown
            };
            return fail(kind, msg);
        }
    };

    let raw = &v1.vendor.raw_snapshot;

    // The vendor reports a throttle explicitly; that is a reading, not a fault.
    if let Some(reached) = raw.get("rateLimitReachedType").and_then(Value::as_str) {
        if !reached.is_empty() {
            let mut obs = fail(
                FailureKind::QuotaDenied,
                format!("Codex reports rate limit reached: {reached}"),
            );
            if let Outcome::Failure { raw: r, .. } = &mut obs.outcome {
                *r = Some(raw.clone());
            }
            return obs;
        }
    }

    let mut resources = vec![];
    // Neither window is privileged. v1 could only express one.
    if let Some(r) = window(raw, "primary", "codex-primary", "Session window", true) {
        resources.push(r);
    }
    if let Some(r) = window(raw, "secondary", "codex-secondary", "Weekly window", false) {
        resources.push(r);
    }
    if let Some(r) = credits(raw) {
        resources.push(r);
    }

    if resources.is_empty() {
        return fail(
            FailureKind::MalformedResponse,
            "Codex rate-limit response contained no recognisable windows",
        );
    }

    let mut obs = Observation::ok(PROBE, VERSION, PROVIDER, SideEffect::RequestConsuming, resources);
    obs.assistant = Some("codex".to_string());
    obs.account = v1.vendor.plan_type.clone();
    if let Outcome::Ok { raw: r, .. } = &mut obs.outcome {
        *r = Some(raw.clone());
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Value {
        // Shape taken verbatim from a live nimbini observation, 2026-08-27.
        serde_json::json!({
            "limitId": "codex",
            "planType": "plus",
            "primary": {"resetsAt": 1787842437, "usedPercent": 34, "windowDurationMins": 300},
            "secondary": {"resetsAt": 1788272245, "usedPercent": 14, "windowDurationMins": 10080},
            "credits": {"balance": "0", "hasCredits": false, "unlimited": false},
            "rateLimitReachedType": null
        })
    }

    #[test]
    fn both_windows_survive_the_migration() {
        let raw = snapshot();
        let p = window(&raw, "primary", "codex-primary", "Session", true).unwrap();
        let s = window(&raw, "secondary", "codex-secondary", "Weekly", false).unwrap();
        assert_eq!(p.facets.utilization, Some(0.34));
        assert_eq!(s.facets.utilization, Some(0.14));
        assert_eq!(p.facets.window_secs, Some(18_000));
        assert_eq!(s.facets.window_secs, Some(604_800));
        // v1 could hold exactly one of these; that was the whole defect.
        assert_ne!(p.id, s.id);
    }

    #[test]
    fn no_credits_resource_when_the_account_holds_none() {
        assert!(credits(&snapshot()).is_none());
    }

    #[test]
    fn credit_balance_is_monetary_and_does_not_perish() {
        let raw = serde_json::json!({
            "credits": {"balance": "12.50", "hasCredits": true, "unlimited": false}
        });
        let r = credits(&raw).unwrap();
        assert_eq!(r.facets.expires_unused, Some(false));
        assert!(r.facets.monetary.is_some());
    }
}
