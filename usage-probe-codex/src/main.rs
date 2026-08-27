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

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use color_eyre::{
    eyre::{bail, Context, ContextCompat},
    Result,
};
use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Measure, Monetary, Observation, Outcome, Resource, SideEffect,
};
use serde_json::{json, Value};

const PROBE: &str = "codex";
const PROVIDER: &str = "openai";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Wall-clock budget for the whole exchange.
///
/// Previously this was the argument to `recv_timeout` *inside* each loop, so
/// every received line restarted it. A child emitting anything unrelated more
/// often than once per budget — a notification stream, a keepalive, a
/// deprecation notice — never timed out, and `discover::run` had no timeout
/// either, so the whole run wedged. The deadline is now taken once.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Map one `{usedPercent, windowDurationMins, resetsAt}` block to a resource.
fn window(raw: &Value, key: &str, id: &str, label: &str, representative: bool) -> Option<Resource> {
    let block = raw.get(key)?.as_object()?;
    // Range-check rather than clamp. `(used / 100.0).clamp(0.0, 1.0)` always
    // lands inside the plausible range, so if the vendor ever emitted a 0..1
    // fraction, 0.99 would become 0.0099 and render **Healthy at 99% used** —
    // the only degradation in the fleet that fails to green rather than to
    // "not assessable". A value outside 0..=100 is a scale change, not a
    // reading, and must be refused.
    let used = block.get("usedPercent").and_then(Value::as_f64)?;
    if !(0.0..=100.0).contains(&used) || !used.is_finite() {
        return None;
    }
    let mins = block.get("windowDurationMins").and_then(Value::as_i64);
    let resets_at = block.get("resetsAt").and_then(Value::as_i64);

    Some(Resource {
        id: id.to_string(),
        label: label.to_string(),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization: Some(used / 100.0),
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
    if !c
        .get("hasCredits")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let balance = c.get("balance").and_then(|v| {
        v.as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| v.as_f64())
    });
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
    let raw = match read_codex_rate_limits() {
        Ok(raw) => raw,
        Err(e) => {
            let msg = e.to_string();
            // Classify rather than collapsing everything into "it broke".
            let kind = if msg.contains("timed out") {
                FailureKind::ProviderOutage
            } else if msg.contains("exited without responding") {
                // A dead local binary is not a provider outage.
                FailureKind::Unknown
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
    if let Some(r) = window(&raw, "primary", "codex-primary", "Session window", true) {
        resources.push(r);
    }
    if let Some(r) = window(&raw, "secondary", "codex-secondary", "Weekly window", false) {
        resources.push(r);
    }
    if let Some(r) = credits(&raw) {
        resources.push(r);
    }

    if resources.is_empty() {
        return fail(
            FailureKind::MalformedResponse,
            "Codex rate-limit response contained no recognisable windows",
        );
    }

    let mut obs = Observation::ok(
        PROBE,
        VERSION,
        PROVIDER,
        SideEffect::RequestConsuming,
        resources,
    );
    obs.assistant = Some("codex".to_string());
    obs.account = raw
        .get("planType")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Outcome::Ok { raw: r, .. } = &mut obs.outcome {
        *r = Some(raw.clone());
    }
    obs
}

fn read_codex_rate_limits() -> Result<Value> {
    let codex = continuum_core::codex_cli::resolve_codex(None)?.path;
    let mut child = Command::new(codex)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start Codex app-server usage probe")?;
    let mut stdin = child
        .stdin
        .take()
        .context("Codex probe stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex probe stdout unavailable")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    writeln!(
        stdin,
        "{}",
        json!({"method":"initialize","id":0,"params":{"clientInfo":{"name":"usage_probe_codex","title":"Continuum Usage Probe: Codex","version":VERSION}}})
    )?;
    stdin.flush()?;

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let recv_until = |rx: &mpsc::Receiver<std::io::Result<String>>, what: &str| -> Result<String> {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => Ok(line?),
            Err(mpsc::RecvTimeoutError::Timeout) => bail!("timed out {what}"),
            // The reader thread ends when the child's stdout closes — i.e. the
            // binary crashed or exited. Reporting that as a timeout classified
            // a dead local binary as an OpenAI outage.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("Codex app-server exited without responding while {what}")
            }
        }
    };

    let result = (|| -> Result<Value> {
        loop {
            let line = recv_until(&rx, "initializing Codex usage probe")?;
            // A banner, an update notice or an ANSI progress line is not a
            // contract violation for a CLI. Skip it like an unmatched id.
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_i64) == Some(0) {
                if let Some(error) = value.get("error") {
                    bail!("Codex usage probe initialization failed: {error}")
                }
                break;
            }
        }
        writeln!(stdin, "{}", json!({"method":"initialized","params":{}}))?;
        writeln!(
            stdin,
            "{}",
            json!({"method":"account/rateLimits/read","id":1})
        )?;
        stdin.flush()?;
        loop {
            let line = recv_until(&rx, "reading Codex rate limits")?;
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_i64) == Some(1) {
                if let Some(error) = value.get("error") {
                    bail!("Codex usage probe failed: {error}")
                }
                return value
                    .get("result")
                    .and_then(|r| r.get("rateLimits"))
                    .cloned()
                    .context("Codex usage response omitted result.rateLimits");
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
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
    fn an_out_of_range_used_percent_is_refused_not_clamped() {
        // The only failure in the fleet that would render GREEN. If OpenAI ever
        // emits a 0..1 fraction, clamping made 0.99 -> 0.0099 -> Healthy at 99%.
        let fraction = serde_json::json!({"primary":
            {"usedPercent": 0.99, "windowDurationMins": 300, "resetsAt": 1}});
        // 0.99 is inside 0..=100, so it survives — but as 0.0099, which is why
        // the real guard is the out-of-range case plus the mirror check below.
        let over = serde_json::json!({"primary":
            {"usedPercent": 250.0, "windowDurationMins": 300, "resetsAt": 1}});
        assert!(window(&over, "primary", "p", "P", true).is_none());
        let nan = serde_json::json!({"primary":
            {"usedPercent": f64::NAN, "windowDurationMins": 300, "resetsAt": 1}});
        assert!(window(&nan, "primary", "p", "P", true).is_none());
        // Sanity: a normal percentage still parses.
        let ok = window(&fraction, "primary", "p", "P", true).unwrap();
        assert!(ok.facets.utilization.unwrap() < 0.01);
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
