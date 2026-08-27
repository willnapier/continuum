//! `usage-probe-anthropic-api` — the metered Anthropic API key (PracticeForge DPA).
//!
//! The fourth ontology, and the discriminating one. Everything else in the
//! observatory so far is a subscription with reset windows. This is:
//!
//! * **rolling**, not fixed-reset — buckets recover continuously
//! * **monetary** — usage is billed, not allowanced
//! * **perishability structurally absent** — spending faster is never an
//!   opportunity, so the "might as well" axis must render as *absent*
//!
//! It shipped as a checked-in fixture in v1 precisely so the envelope could be
//! proved against this shape before the probe existed. This is that probe; it
//! required no schema change, which is the acceptance evidence the forum asked
//! for.
//!
//! **This key is the PracticeForge clinical billing stream.** The probe sends a
//! single fixed token ("hi") and reads response headers. It transmits no
//! patient data and reads none — but it does spend a fraction of a cent, so it
//! declares `chargeable` and core holds it to the costly cadence.

use std::process::ExitCode;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Measure, Monetary, Observation, ObservationCost, Outcome,
    Resource, SideEffect,
};

const PROBE: &str = "anthropic-api";
const PROVIDER: &str = "anthropic-api";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const API: &str = "https://api.anthropic.com/v1/messages";
const PROBE_MODEL: &str = "claude-haiku-4-5-20251001";

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

/// The DPA key lives in PracticeForge's secrets file under `[ai] api_key`.
/// Only the probe process ever holds it; core never sees a credential.
fn read_key() -> Result<String, Observation> {
    let home = std::env::var("HOME").map_err(|_| fail(FailureKind::Unknown, "HOME is not set"))?;
    let path = format!("{home}/.config/practiceforge/secrets.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        fail(
            FailureKind::InvalidCredentials,
            format!("cannot read {path}: {e}"),
        )
    })?;
    let doc: toml::Value = text.parse().map_err(|e| {
        fail(
            FailureKind::InvalidCredentials,
            format!("{path} is not valid TOML: {e}"),
        )
    })?;
    doc.get("ai")
        .and_then(|ai| ai.get("api_key"))
        .and_then(|k| k.as_str())
        .filter(|k| !k.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| fail(FailureKind::InvalidCredentials, "[ai] api_key is absent or empty"))
}

/// Build a rolling-bucket resource from the `-limit` / `-remaining` / `-reset`
/// header triple Anthropic sends for each metered dimension.
fn bucket(
    get: &dyn Fn(&str) -> Option<String>,
    header_stem: &str,
    id: &str,
    label: &str,
    unit: &str,
) -> Option<Resource> {
    let limit = get(&format!("anthropic-ratelimit-{header_stem}-limit"))?
        .parse::<f64>()
        .ok()?;
    let remaining = get(&format!("anthropic-ratelimit-{header_stem}-remaining"))
        .and_then(|v| v.parse::<f64>().ok());
    // Anthropic sends an RFC3339 instant here, not a unix epoch.
    let resets_at = get(&format!("anthropic-ratelimit-{header_stem}-reset"))
        .and_then(|v| chrono_parse(&v));

    Some(Resource {
        id: id.to_string(),
        label: label.to_string(),
        kind_hint: KindHint::RollingRecovery,
        facets: Facets {
            remaining: remaining.map(|r| Measure::new(r, unit)),
            limit: Some(Measure::new(limit, unit)),
            resets_at,
            // A rolling bucket refills whether or not you drew on it. There is
            // no surplus to "use up before it expires", so perishability is
            // structurally absent — not merely unknown.
            expires_unused: Some(false),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: id == "input-tokens-per-minute",
    })
}

/// Minimal RFC3339 -> unix seconds, without pulling chrono into this crate.
fn chrono_parse(value: &str) -> Option<i64> {
    // Format: 2026-08-27T15:04:05Z
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |a: usize, b: usize| value.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);

    // Days since the Unix epoch (civil-from-days, Howard Hinnant's algorithm).
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

fn probe() -> Observation {
    let key = match read_key() {
        Ok(k) => k,
        Err(obs) => return obs,
    };

    let body = serde_json::json!({
        "model": PROBE_MODEL,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();

    let request = ureq::post(API)
        .set("x-api-key", &key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .timeout(std::time::Duration::from_secs(20));

    let (response, throttled) = match request.send_string(&body) {
        Ok(r) => (r, false),
        // A 429 still carries the headers, and the headers are the point.
        Err(ureq::Error::Status(429, r)) => (r, true),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return fail(
                FailureKind::InvalidCredentials,
                "Anthropic rejected the API key (401/403)",
            )
        }
        Err(ureq::Error::Status(402, _)) => {
            return fail(FailureKind::QuotaDenied, "402 Payment Required — credit exhausted")
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
        headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    };

    let mut resources = vec![];
    for (stem, id, label, unit) in [
        ("requests", "requests-per-minute", "Requests / minute", "requests"),
        ("input-tokens", "input-tokens-per-minute", "Input tokens / minute", "tokens"),
        ("output-tokens", "output-tokens-per-minute", "Output tokens / minute", "tokens"),
        ("tokens", "tokens-per-minute", "Tokens / minute", "tokens"),
    ] {
        if let Some(r) = bucket(&get, stem, id, label, unit) {
            resources.push(r);
        }
    }

    if resources.is_empty() {
        return fail(
            FailureKind::MalformedResponse,
            "response carried no anthropic-ratelimit-* bucket headers",
        );
    }

    let mut obs = Observation::ok(PROBE, VERSION, PROVIDER, SideEffect::Chargeable, resources);
    obs.assistant = Some("practiceforge".to_string());
    // A label, not the key and not an org id.
    obs.account = Some("dpa-metered".to_string());
    if let Outcome::Ok { cost, raw, .. } = &mut obs.outcome {
        *cost = Some(ObservationCost {
            requests: Some(1),
            tokens: Some(1),
            note: Some("billed at list price; fractions of a cent per observation".into()),
        });
        *raw = Some(serde_json::json!({ "throttled": throttled }));
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuum_usage_core::policy::{assess, AxisState, Policy};

    #[test]
    fn rfc3339_parses_to_unix_seconds() {
        // 2026-08-27T00:00:00Z
        assert_eq!(chrono_parse("2026-08-27T00:00:00Z"), Some(1_787_788_800));
        // Epoch itself, as a sanity anchor.
        assert_eq!(chrono_parse("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(chrono_parse("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(chrono_parse("nonsense"), None);
    }

    fn headers(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn a_metered_bucket_has_scarcity_but_no_perishability() {
        let get = headers(&[
            ("anthropic-ratelimit-input-tokens-limit", "20000"),
            ("anthropic-ratelimit-input-tokens-remaining", "2000"),
            ("anthropic-ratelimit-input-tokens-reset", "2026-08-27T15:05:00Z"),
        ]);
        let r = bucket(&get, "input-tokens", "input-tokens-per-minute", "Input", "tokens").unwrap();

        let a = assess(&r, &Policy::default(), 1_787_832_000, 0);
        // 2000 of 20000 left => 90% used.
        assert_eq!(a.scarcity, AxisState::Critical);
        // Money. Burning it faster is never an opportunity.
        assert_eq!(a.perishability, AxisState::Inapplicable);
        assert_ne!(a.perishability, AxisState::Opportunity);
    }

    #[test]
    fn a_bucket_without_a_limit_header_is_not_invented() {
        let get = headers(&[("anthropic-ratelimit-input-tokens-remaining", "2000")]);
        assert!(
            bucket(&get, "input-tokens", "x", "X", "tokens").is_none(),
            "no limit header means no resource, not a guessed one"
        );
    }
}
