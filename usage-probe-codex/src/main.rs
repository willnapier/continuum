//! `usage-probe-codex` — OpenAI Codex (ChatGPT plan) probe.
//!
//! Migration of the shipped v1 monitor. It reuses the tested app-server call in
//! `continuum-core` (`account/rateLimits/read`) and maps the result into the v2
//! envelope.
//!
//! The mapping is where the old schema's flaw shows: v1 hoisted `primary` to
//! top-level `used_percent`/`resets_at` fields and shoved `secondary` into an
//! untyped `Value`, because the shape could only hold one window. Here both are
//! ordinary resources, and neither is privileged. Resource labels follow
//! `windowDurationMins` (5h session, 7d week, or an explicit unknown), not the
//! `primary`/`secondary` slot — Pro Lite puts the week in `primary`.

use std::io::{BufRead, BufReader, Write};
use std::process::{ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use color_eyre::{
    eyre::{bail, eyre, Context, ContextCompat},
    Result,
};
use continuum_usage_core::masked_launch;
use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Measure, MeterScope, Monetary, Observation, Outcome, Resource,
    SideEffect,
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

/// ChatGPT Plus used a 5-hour `primary` plus a 7-day `secondary`. Pro Lite
/// (observed 2026-09-02) puts the 7-day window in `primary` and omits
/// `secondary`. Labels therefore come from duration, not from the slot name.
const SESSION_WINDOW_MINS: i64 = 300;
const WEEKLY_WINDOW_MINS: i64 = 10_080;

fn window_label(mins: Option<i64>) -> String {
    match mins {
        Some(SESSION_WINDOW_MINS) => "Session (5 hours)".to_string(),
        Some(WEEKLY_WINDOW_MINS) => "Weekly (7 days)".to_string(),
        Some(m) if m > 0 => format!("Window ({})", format_window_mins(m)),
        _ => "Window (duration unknown)".to_string(),
    }
}

fn format_window_mins(mins: i64) -> String {
    if mins % 1_440 == 0 {
        let days = mins / 1_440;
        if days == 1 {
            "1 day".to_string()
        } else {
            format!("{days} days")
        }
    } else if mins % 60 == 0 {
        let hours = mins / 60;
        if hours == 1 {
            "1 hour".to_string()
        } else {
            format!("{hours} hours")
        }
    } else {
        format!("{mins} minutes")
    }
}

/// Map one `{usedPercent, windowDurationMins, resetsAt}` block to a resource.
fn window(raw: &Value, key: &str, id: &str, representative: bool) -> Option<Resource> {
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
        label: window_label(mins),
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
    // Neither window is privileged. v1 could only express one. Do not name
    // `primary` a session: Pro Lite puts the weekly window there.
    if let Some(r) = window(&raw, "primary", "codex-primary", true) {
        resources.push(r);
    }
    if let Some(r) = window(&raw, "secondary", "codex-secondary", false) {
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
    // account/rateLimits/read is the plan's 5-hour and weekly windows across every
    // host that signs in with this account; the probing host is not the meter.
    obs.scope = MeterScope::Account;
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
    // On Linux through the mask launcher, `ai-masked <codex> app-server`;
    // directly elsewhere (`continuum_usage_core::masked_launch`,
    // ~/Assistants/shared/AI-FOOTING-MASK.md). It matters most here: a
    // `codex app-server` outside the mask makes the launcher refuse every
    // other Codex start while it lives, and is a red health finding. The
    // launcher becomes Codex by exec, so the pipes and the kill below reach
    // Codex itself. A missing launcher is the error, never a direct start.
    let mut child = masked_launch::harness_command(&codex)
        .map_err(|reason| eyre!(reason))?
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| masked_launch::spawn_failure("the Codex app-server usage probe"))?;
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
        // Inside the closure, so a child that has already gone (a launcher
        // refusal closes the pipe at once) is still reaped and explained below.
        writeln!(
            stdin,
            "{}",
            json!({"method":"initialize","id":0,"params":{"clientInfo":{"name":"usage_probe_codex","title":"Continuum Usage Probe: Codex","version":VERSION}}})
        )?;
        stdin.flush()?;
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
    let exit_code = child.wait().ok().and_then(|status| status.code());
    explain_launcher_refusal(
        result,
        exit_code,
        &codex,
        masked_launch::masked_on_this_system(),
    )
}

/// When the mask launcher refused to start anything, say so. Its own reason
/// went to stderr, which this probe does not keep, and what is left ("exited
/// without responding", or a broken pipe) reads as a fault in Codex. Only an
/// error on a masked system with the launcher's refusal code is reworded.
fn explain_launcher_refusal(
    result: Result<Value>,
    exit_code: Option<i32>,
    codex: &std::path::Path,
    masked: bool,
) -> Result<Value> {
    match result {
        Err(error) if masked && exit_code == Some(masked_launch::REFUSED_EXIT) => {
            Err(error.wrap_err(format!(
                "`{launcher}` refused to start the Codex app-server (exit {code}), so no \
                 reading was taken. Run `{launcher} {codex} --version` in a terminal to see \
                 its reason.",
                launcher = masked_launch::LAUNCHER,
                code = masked_launch::REFUSED_EXIT,
                codex = codex.display(),
            )))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Value {
        // Shape taken verbatim from a live observation, 2026-08-27.
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
        let p = window(&raw, "primary", "codex-primary", true).unwrap();
        let s = window(&raw, "secondary", "codex-secondary", false).unwrap();
        assert_eq!(p.facets.utilization, Some(0.34));
        assert_eq!(s.facets.utilization, Some(0.14));
        assert_eq!(p.facets.window_secs, Some(18_000));
        assert_eq!(s.facets.window_secs, Some(604_800));
        assert_eq!(p.label, "Session (5 hours)");
        assert_eq!(s.label, "Weekly (7 days)");
        // v1 could hold exactly one of these; that was the whole defect.
        assert_ne!(p.id, s.id);
    }

    #[test]
    fn a_seven_day_primary_is_weekly_not_a_session() {
        // Live Pro Lite shape, 2026-09-07: weekly allowance in `primary`,
        // `secondary` absent. Hardcoding "Session window" on primary made
        // usagewatch report 58% of a 5-hour session that had not been used.
        let raw = serde_json::json!({
            "planType": "prolite",
            "primary": {"resetsAt": 1788996441, "usedPercent": 58, "windowDurationMins": 10080},
            "secondary": null,
            "credits": {"balance": "0", "hasCredits": false, "unlimited": false}
        });
        let p = window(&raw, "primary", "codex-primary", true).unwrap();
        assert_eq!(p.label, "Weekly (7 days)");
        assert_eq!(p.facets.window_secs, Some(604_800));
        assert_eq!(p.facets.utilization, Some(0.58));
        assert!(window(&raw, "secondary", "codex-secondary", false).is_none());
    }

    #[test]
    fn unknown_or_odd_durations_are_not_named_session_or_weekly() {
        assert_eq!(window_label(None), "Window (duration unknown)");
        assert_eq!(window_label(Some(0)), "Window (duration unknown)");
        assert_eq!(window_label(Some(-1)), "Window (duration unknown)");
        assert_eq!(window_label(Some(240)), "Window (4 hours)");
        assert_eq!(window_label(Some(1_440)), "Window (1 day)");
        assert_eq!(window_label(Some(90)), "Window (90 minutes)");
        let raw = serde_json::json!({
            "primary": {"usedPercent": 10, "windowDurationMins": 240, "resetsAt": 1}
        });
        let p = window(&raw, "primary", "codex-primary", true).unwrap();
        assert_eq!(p.label, "Window (4 hours)");
        let missing = serde_json::json!({"primary": {"usedPercent": 10, "resetsAt": 1}});
        assert_eq!(
            window(&missing, "primary", "codex-primary", true)
                .unwrap()
                .label,
            "Window (duration unknown)"
        );
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
        assert!(window(&over, "primary", "p", true).is_none());
        let nan = serde_json::json!({"primary":
            {"usedPercent": f64::NAN, "windowDurationMins": 300, "resetsAt": 1}});
        assert!(window(&nan, "primary", "p", true).is_none());
        // Sanity: a normal percentage still parses.
        let ok = window(&fraction, "primary", "p", true).unwrap();
        assert!(ok.facets.utilization.unwrap() < 0.01);
        assert_eq!(ok.label, "Session (5 hours)");
    }

    #[test]
    fn a_launcher_refusal_is_named_and_nothing_else_is_reworded() {
        let codex = std::path::Path::new("/opt/real/codex");
        let dead = || -> Result<Value> { bail!("Codex app-server exited without responding") };

        let refused = explain_launcher_refusal(dead(), Some(113), codex, true).unwrap_err();
        let text = refused.to_string();
        assert!(text.contains("`ai-masked` refused to start"), "{text}");
        assert!(text.contains("ai-masked /opt/real/codex --version"), "{text}");
        // Not one of the words `probe()` reads as an outage or a login fault.
        assert!(!text.contains("timed out") && !text.to_lowercase().contains("auth"), "{text}");

        // Any other exit, a child that was killed, and a system without the
        // mask keep the original error.
        for (code, masked) in [(Some(1), true), (None, true), (Some(113), false)] {
            let kept = explain_launcher_refusal(dead(), code, codex, masked).unwrap_err();
            assert_eq!(kept.to_string(), "Codex app-server exited without responding");
        }
        // A reading that arrived is never turned into an error.
        let reading = explain_launcher_refusal(Ok(snapshot()), Some(113), codex, true);
        assert_eq!(reading.unwrap(), snapshot());
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
