//! `usage-probe-claude` — Anthropic subscription (Claude Max) probe.
//!
//! Reads remaining quota by making one minimal request and inspecting the
//! `anthropic-ratelimit-unified-*` response headers. Nothing on disk carries
//! this: `~/.claude/stats-cache.json` is activity counts and `policy-limits.json`
//! is unrelated, so it must be probed.
//!
//! **This probe spends the allowance it measures** — one Haiku token per
//! observation — so it declares `quota-consuming` and core holds it to a long
//! minimum interval unless the user explicitly refreshes.
//!
//! The account returns three concurrent windows plus a `representative-claim`.
//! The vendor declines to reduce them to one number, and so do we.

use std::process::ExitCode;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Monetary, Observation, ObservationCost, Outcome, Resource,
    SideEffect,
};

const PROBE: &str = "claude";
const PROVIDER: &str = "anthropic";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const API: &str = "https://api.anthropic.com/v1/messages";
/// Smallest possible real request. One token.
const PROBE_MODEL: &str = "claude-haiku-4-5-20251001";

fn main() -> ExitCode {
    let obs = probe();
    // Contract: exactly one JSON object on stdout, success or failure.
    println!("{}", serde_json::to_string(&obs).expect("envelope serialises"));
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn fail(kind: FailureKind, msg: impl Into<String>) -> Observation {
    Observation::failure(PROBE, VERSION, PROVIDER, kind, msg)
}

struct Credentials {
    access_token: String,
    subscription: Option<String>,
}

fn read_credentials() -> Result<Credentials, Observation> {
    let home = std::env::var("HOME")
        .map_err(|_| fail(FailureKind::Unknown, "HOME is not set"))?;
    let path = format!("{home}/.claude/.credentials.json");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        fail(
            FailureKind::InvalidCredentials,
            format!("cannot read {path}: {e}"),
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| fail(FailureKind::InvalidCredentials, format!("{path} is not JSON: {e}")))?;
    let oauth = value.get("claudeAiOauth").ok_or_else(|| {
        fail(
            FailureKind::InvalidCredentials,
            "credentials file has no claudeAiOauth block",
        )
    })?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| fail(FailureKind::InvalidCredentials, "no accessToken present"))?
        .to_string();
    Ok(Credentials {
        access_token,
        // Not a secret and not identifying: "max", "pro". Used as the account
        // pseudonym so history can be attributed without storing anything.
        subscription: oauth
            .get("subscriptionType")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// Pull one `anthropic-ratelimit-unified-<window>-<field>` header.
fn header<'a>(get: &'a dyn Fn(&str) -> Option<String>, window: &str, field: &str) -> Option<String> {
    get(&format!("anthropic-ratelimit-unified-{window}-{field}"))
}

fn window_resource(
    get: &dyn Fn(&str) -> Option<String>,
    window: &str,
    label: &str,
    window_secs: i64,
    representative: bool,
) -> Option<Resource> {
    let utilization = header(&get, window, "utilization")?.parse::<f64>().ok();
    let resets_at = header(&get, window, "reset").and_then(|v| v.parse::<i64>().ok());
    let status = header(&get, window, "status");

    Some(Resource {
        id: format!("unified-{window}"),
        label: label.to_string(),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization,
            resets_at,
            window_secs: Some(window_secs),
            // Unused allowance in a subscription window is lost at the reset.
            // This is what makes "might as well" meaningful here.
            expires_unused: Some(true),
            ..Default::default()
        },
        vendor_status: status,
        vendor_representative: representative,
    })
}

/// The overage meter: credit spend, inside a window-shaped plan.
///
/// This single row is why the round-1 tagged union was rejected. It carries a
/// reset *and* spend semantics. `expires_unused: false` is the load-bearing
/// field — without it core would read the reset as perishable and cheerfully
/// advise burning credit.
fn overage_resource(get: &dyn Fn(&str) -> Option<String>) -> Option<Resource> {
    let utilization = header(&get, "overage", "utilization")?.parse::<f64>().ok();
    let resets_at = header(&get, "overage", "reset").and_then(|v| v.parse::<i64>().ok());
    Some(Resource {
        id: "unified-overage".to_string(),
        label: "Overage (credit spend)".to_string(),
        kind_hint: KindHint::Continuous,
        facets: Facets {
            utilization,
            resets_at,
            expires_unused: Some(false),
            monetary: Some(Monetary {
                currency: "USD".to_string(),
                spent: None,
                cap: None,
            }),
            ..Default::default()
        },
        vendor_status: header(&get, "overage", "status"),
        vendor_representative: false,
    })
}

fn probe() -> Observation {
    let creds = match read_credentials() {
        Ok(c) => c,
        Err(obs) => return obs,
    };

    let body = serde_json::json!({
        "model": PROBE_MODEL,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();

    let request = ureq::post(API)
        .set("authorization", &format!("Bearer {}", creds.access_token))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("content-type", "application/json")
        .timeout(std::time::Duration::from_secs(20));

    // A 429 still carries the rate-limit headers, and those headers are the
    // whole point — so a throttle is a *reading*, not an error to discard.
    let (response, throttled) = match request.send_string(&body) {
        Ok(r) => (r, false),
        Err(ureq::Error::Status(429, r)) => (r, true),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return fail(
                FailureKind::InvalidCredentials,
                "Anthropic rejected the OAuth token (401/403); re-authenticate Claude Code",
            )
        }
        Err(ureq::Error::Status(402, r)) => {
            return fail(
                FailureKind::QuotaDenied,
                format!(
                    "402 Payment Required: {}",
                    r.into_string().unwrap_or_default().chars().take(300).collect::<String>()
                ),
            )
        }
        Err(ureq::Error::Status(code, _)) if (500..600).contains(&code) => {
            return fail(FailureKind::ProviderOutage, format!("Anthropic returned {code}"))
        }
        Err(ureq::Error::Status(code, _)) => {
            return fail(FailureKind::MalformedResponse, format!("unexpected status {code}"))
        }
        Err(ureq::Error::Transport(t)) => {
            return fail(FailureKind::NetworkFailure, format!("transport error: {t}"))
        }
    };

    let headers: Vec<(String, String)> = response
        .headers_names()
        .into_iter()
        .filter_map(|n| response.header(&n).map(|v| (n.to_lowercase(), v.to_string())))
        .collect();
    let get = move |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    // Which window the vendor considers representative of the account. Carried
    // as a display hint; core still renders every resource.
    let representative = get("anthropic-ratelimit-unified-representative-claim");
    let rep_id = match representative.as_deref() {
        Some("five_hour") => "5h",
        Some("seven_day") => "7d",
        _ => "",
    };

    let mut resources = vec![];
    if let Some(r) = window_resource(&get, "5h", "Session (5 hours)", 5 * 3600, rep_id == "5h") {
        resources.push(r);
    }
    if let Some(r) = window_resource(&get, "7d", "Weekly (7 days)", 7 * 86_400, rep_id == "7d") {
        resources.push(r);
    }
    if let Some(r) = overage_resource(&get) {
        resources.push(r);
    }

    if resources.is_empty() {
        return fail(
            FailureKind::MalformedResponse,
            "response carried no anthropic-ratelimit-unified-* headers",
        );
    }

    let mut obs = Observation::ok(
        PROBE,
        VERSION,
        PROVIDER,
        SideEffect::QuotaConsuming,
        resources,
    );
    obs.assistant = Some("claude-code".to_string());
    obs.account = creds.subscription;
    if let Outcome::Ok { cost, raw, .. } = &mut obs.outcome {
        *cost = Some(ObservationCost {
            requests: Some(1),
            tokens: Some(1),
            note: Some("one minimal Haiku request; spends the quota it measures".into()),
        });
        *raw = Some(serde_json::json!({
            "unified_status": get("anthropic-ratelimit-unified-status"),
            "representative_claim": representative,
            "fallback_percentage": get("anthropic-ratelimit-unified-fallback-percentage"),
            "throttled": throttled,
        }));
    }
    obs
}
