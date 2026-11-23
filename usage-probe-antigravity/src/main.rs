//! `usage-probe-antigravity` — Google AI Ultra limits, read through the
//! Antigravity CLI (`agy`, Gemini 4 Neon).
//!
//! `agy -p /usage --output-format json` answers the `/usage` slash command in
//! print mode without a model turn: `num_turns` 0, every token count 0, about
//! 3 s (verified 23 Nov 2026 on agy 1.5.15). The reply carries the plan's
//! limit groups, each with a weekly and a 5-hour bucket:
//!
//! ```text
//! command.data.groups[] {name, buckets[] {id, window, remaining_fraction, reset_time}}
//! ```
//!
//! `agy` makes the vendor call itself (`retrieveUserQuotaSummary` on
//! `cloudcode-pa.googleapis.com`) with its own sign-in from the desktop
//! keyring. **This probe never reads that credential**: it runs `agy` and reads
//! what `agy` prints, so its blast radius is the CLI's, not a token's.
//!
//! The groups are account meters: the Ultra plan's limits wherever the account
//! signs in. Within a group the models share both limits ("Quota is consumed
//! proportionally to the cost of the tokens"), so Gemini 4 Neon draws on the
//! Gemini group with Carbon, Argon, Hydrogen, the Orchestrators and Flash.
//!
//! Two guards:
//!
//! - **`/usage` must not reach the model.** If a later `agy` sent it as a
//!   prompt instead, the reply would have no `command` block and the run would
//!   have spent quota. That is reported as a malformed reading naming the
//!   tokens spent, never parsed out of the model's prose.
//! - **A fraction outside 0..=1 is refused**, not clamped: a scale change must
//!   read as "not assessable", never as healthy.
//!
//! On Linux `agy` starts through the mask launcher `ai-masked`
//! (`continuum_usage_core::masked_launch`, `~/Assistants/shared/AI-FOOTING-MASK.md`),
//! from an empty scratch folder, in plan mode. A missing launcher is a failed
//! reading, never a direct start.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, MeterScope, Observation, Outcome, Resource, SideEffect,
};
use continuum_usage_core::masked_launch;
use continuum_usage_core::smoke::{self, tail, Run};
use serde_json::{json, Value};

const PROBE: &str = "antigravity";
const PROVIDER: &str = "google";
const ASSISTANT: &str = "antigravity";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Points the probe at another `agy`; also the verified error path.
const BIN_OVERRIDE: &str = "CONTINUUM_AGY_BIN";
/// `agy`'s own limit on print mode, inside the probe's hard deadline.
const PRINT_TIMEOUT: &str = "30s";
const DEADLINE: Duration = Duration::from_secs(40);
/// The bucket the status header marks: the Gemini weekly limit is the one
/// Neon's overnight work is bounded by.
const REPRESENTATIVE_BUCKET: &str = "gemini-weekly";

fn main() -> ExitCode {
    let obs = probe();
    println!("{}", serde_json::to_string(&obs).expect("envelope serialises"));
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn fail(kind: FailureKind, msg: impl Into<String>) -> Observation {
    let mut obs = Observation::failure(PROBE, VERSION, PROVIDER, kind, msg);
    obs.assistant = Some(ASSISTANT.to_string());
    obs.scope = MeterScope::Account;
    obs
}

fn with_raw(mut obs: Observation, raw: Value) -> Observation {
    match &mut obs.outcome {
        Outcome::Ok { raw: slot, .. } | Outcome::Failure { raw: slot, .. } => *slot = Some(raw),
    }
    obs
}

fn probe() -> Observation {
    let agy = match smoke::resolve_bin(BIN_OVERRIDE, "agy") {
        Ok(p) => p,
        Err(e) => return fail(FailureKind::NetworkFailure, format!("no agy binary: {e}")),
    };
    let run = match run_agy(&agy) {
        Ok(run) => run,
        Err(e) => return fail(FailureKind::NetworkFailure, e),
    };
    observe(&run)
}

/// `agy -p /usage` in plan mode, from an empty scratch folder, so nothing in
/// the caller's folder is offered to it as a workspace.
fn run_agy(agy: &Path) -> Result<Run, String> {
    let workdir = std::env::temp_dir().join("usage-probe-antigravity");
    std::fs::create_dir_all(&workdir).map_err(|e| format!("scratch dir: {e}"))?;
    let mut cmd = masked_launch::harness_command(agy)?;
    cmd.current_dir(&workdir).args([
        "-p",
        "/usage",
        "--mode",
        "plan",
        "--output-format",
        "json",
        "--print-timeout",
        PRINT_TIMEOUT,
    ]);
    smoke::run_with_deadline(cmd, DEADLINE)
}

/// The whole reading from one finished run.
fn observe(run: &Run) -> Observation {
    let reply = last_json_object(&run.stdout);
    let Some(reply) = reply else {
        let source = if run.stderr.trim().is_empty() { &run.stdout } else { &run.stderr };
        let message = format!("no JSON reply (exit {:?}): {}", run.exit_code, tail(source, 300).trim());
        return with_raw(
            fail(refusal_kind(&message), message),
            json!({"exit_code": run.exit_code, "stderr_tail": tail(&run.stderr, 600)}),
        );
    };

    let status = reply.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "SUCCESS" {
        let said = reply
            .get("response")
            .or_else(|| reply.get("error"))
            .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
            .unwrap_or_default();
        let message = format!("agy /usage status {status:?}: {}", tail(said.trim(), 300));
        return with_raw(fail(refusal_kind(&message), message), reply);
    }

    match resources_from(&reply) {
        Ok(resources) => {
            let mut obs =
                Observation::ok(PROBE, VERSION, PROVIDER, SideEffect::RequestConsuming, resources);
            obs.assistant = Some(ASSISTANT.to_string());
            // The plan's limits wherever the account signs in; the probing host
            // is only where `agy` ran.
            obs.scope = MeterScope::Account;
            // The descriptions and groups, as the vendor sent them.
            with_raw(obs, reply.pointer("/command/data").cloned().unwrap_or(Value::Null))
        }
        Err(message) => with_raw(fail(FailureKind::MalformedResponse, message), reply),
    }
}

/// `agy` prints one JSON object; take the last line that parses as one, so a
/// banner or update notice before it is not fatal.
fn last_json_object(stdout: &str) -> Option<Value> {
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(stdout.trim()) {
        return Some(v);
    }
    stdout
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(Value::is_object)
}

/// `agy`'s own words for a lost sign-in ("You are not logged into
/// Antigravity", "Eligibility check failed … Verify your account to continue")
/// are credentials, not a refusal; the rest is the shared table.
fn refusal_kind(message: &str) -> FailureKind {
    let m = message.to_ascii_lowercase();
    if ["not logged in", "logged into", "verify your account", "eligibility check"]
        .iter()
        .any(|w| m.contains(w))
    {
        FailureKind::InvalidCredentials
    } else {
        smoke::kind_for(None, message)
    }
}

/// One resource per bucket. Errors name what was wrong, for the envelope.
fn resources_from(reply: &Value) -> Result<Vec<Resource>, String> {
    let Some(command) = reply.get("command") else {
        let tokens = reply.pointer("/usage/total_tokens").and_then(Value::as_u64);
        return Err(format!(
            "agy answered /usage without its command block: the slash command reached the model \
             ({} turns, {} tokens spent). Check `agy -p /usage --output-format json` by hand.",
            reply.get("num_turns").and_then(Value::as_u64).map_or("?".into(), |n| n.to_string()),
            tokens.map_or("?".into(), |n| n.to_string()),
        ));
    };
    if command.get("name").and_then(Value::as_str) != Some("usage") {
        return Err(format!("agy answered another command: {}", command.get("name").unwrap_or(&Value::Null)));
    }
    let groups = command
        .pointer("/data/groups")
        .and_then(Value::as_array)
        .ok_or("agy /usage reply has no data.groups")?;

    let mut resources = vec![];
    let mut skipped = vec![];
    for group in groups {
        let group_name = group.get("name").and_then(Value::as_str).unwrap_or("Unnamed group");
        for bucket in group.get("buckets").and_then(Value::as_array).into_iter().flatten() {
            match bucket_resource(group_name, bucket) {
                Some(r) => resources.push(r),
                None => skipped.push(bucket.get("id").and_then(Value::as_str).unwrap_or("?").to_string()),
            }
        }
    }
    if resources.is_empty() {
        return Err(format!(
            "agy /usage reply held no readable bucket (unreadable: {})",
            if skipped.is_empty() { "none sent".to_string() } else { skipped.join(", ") }
        ));
    }
    Ok(resources)
}

fn bucket_resource(group_name: &str, bucket: &Value) -> Option<Resource> {
    let id = bucket.get("id").and_then(Value::as_str)?;
    let remaining = bucket.get("remaining_fraction").and_then(Value::as_f64)?;
    // Refuse, do not clamp: a percentage arriving here would read 97.1 as
    // "used −9610%" clamped to healthy.
    if !remaining.is_finite() || !(0.0..=1.0).contains(&remaining) {
        return None;
    }
    let window = bucket.get("window").and_then(Value::as_str);
    let window_secs = window.and_then(window_secs);
    let resets_at = bucket
        .get("reset_time")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp());
    Some(Resource {
        id: format!("antigravity-{id}"),
        label: format!("{} ({})", short_group(group_name), window_label(window, window_secs)),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization: Some(1.0 - remaining),
            resets_at,
            window_secs,
            // Plan allowance not spent before the reset is gone.
            expires_unused: Some(true),
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: id == REPRESENTATIVE_BUCKET,
    })
}

/// `weekly` and `5h` are the two `agy` sends; `<n>h` and `<n>d` are read
/// generally, anything else is a window of unknown length.
fn window_secs(window: &str) -> Option<i64> {
    match window {
        "weekly" => Some(7 * 86_400),
        "daily" => Some(86_400),
        w => {
            let (n, unit) = w.split_at(w.find(|c: char| !c.is_ascii_digit())?);
            let n: i64 = n.parse().ok()?;
            match unit {
                "h" => Some(n * 3_600),
                "d" => Some(n * 86_400),
                "m" => Some(n * 60),
                _ => None,
            }
        }
    }
}

fn window_label(window: Option<&str>, secs: Option<i64>) -> String {
    match secs {
        Some(s) if s % 86_400 == 0 => plural(s / 86_400, "day"),
        Some(s) if s % 3_600 == 0 => plural(s / 3_600, "hour"),
        Some(s) => plural(s / 60, "minute"),
        None => window.map_or("window unknown".to_string(), |w| format!("window {w}")),
    }
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") }
}

/// "Gemini Models" → "Gemini", "Claude and GPT models" → "Claude/GPT": short
/// enough for the 22-column status cell with the window beside it.
fn short_group(name: &str) -> String {
    let trimmed = name.trim();
    let base = trimmed
        .strip_suffix(" models")
        .or_else(|| trimmed.strip_suffix(" Models"))
        .unwrap_or(trimmed);
    base.replace(" and ", "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real reply, captured 23 Nov 2026 12:15 from agy 1.5.15.
    const REAL: &str = include_str!("../tests/fixtures/usage-2026-11-23.json");

    fn run(stdout: &str, stderr: &str, exit_code: i32) -> Run {
        Run { stdout: stdout.into(), stderr: stderr.into(), exit_code: Some(exit_code) }
    }

    fn resources(obs: &Observation) -> &[Resource] {
        match &obs.outcome {
            Outcome::Ok { resources, .. } => resources,
            Outcome::Failure { kind, message, .. } => panic!("failure {kind:?}: {message}"),
        }
    }

    fn failure(obs: &Observation) -> (&FailureKind, &str) {
        match &obs.outcome {
            Outcome::Failure { kind, message, .. } => (kind, message),
            Outcome::Ok { .. } => panic!("expected a failure"),
        }
    }

    #[test]
    fn the_real_reply_gives_four_account_buckets() {
        let obs = observe(&run(REAL, "", 0));
        let rs = resources(&obs);
        let ids: Vec<_> = rs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["antigravity-gemini-weekly", "antigravity-gemini-5h", "antigravity-3p-weekly", "antigravity-3p-5h"]
        );
        let labels: Vec<_> = rs.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["Gemini (7 days)", "Gemini (5 hours)", "Claude/GPT (7 days)", "Claude/GPT (5 hours)"]);
        assert!(labels.iter().all(|l| l.chars().count() <= 22), "{labels:?}");

        let week = &rs[0];
        assert!(week.vendor_representative);
        assert!(rs[1..].iter().all(|r| !r.vendor_representative));
        let used = week.facets.utilization.unwrap();
        assert!((used - (1.0 - 0.9712302684783936)).abs() < 1e-12, "{used}");
        assert_eq!(week.facets.window_secs, Some(604_800));
        assert_eq!(rs[1].facets.window_secs, Some(18_000));
        assert_eq!(week.facets.resets_at, Some(chrono::DateTime::parse_from_rfc3339("2026-11-29T22:27:03Z").unwrap().timestamp()));
        assert_eq!(week.facets.expires_unused, Some(true));
        assert_eq!(rs[2].facets.utilization, Some(0.0));

        assert_eq!(obs.scope, MeterScope::Account);
        assert_eq!(obs.provider, "google");
        assert_eq!(obs.assistant.as_deref(), Some("antigravity"));
        match &obs.outcome {
            Outcome::Ok { side_effect, raw, .. } => {
                assert_eq!(*side_effect, SideEffect::RequestConsuming);
                assert!(raw.as_ref().unwrap().get("groups").is_some());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_banner_before_the_reply_is_skipped() {
        let stdout = format!("A new version of agy is available\n{}", REAL.trim());
        assert_eq!(resources(&observe(&run(&stdout, "", 0))).len(), 4);
    }

    /// Known red: `/usage` sent to the model as a prompt. No command block,
    /// tokens spent: never read the model's prose as a reading.
    #[test]
    fn a_usage_that_reached_the_model_is_refused() {
        let stdout = r#"{"status":"SUCCESS","response":"Gemini Models Weekly 98%","num_turns":1,"usage":{"total_tokens":4211}}"#;
        let obs = observe(&run(stdout, "", 0));
        let (kind, message) = failure(&obs);
        assert_eq!(*kind, FailureKind::MalformedResponse);
        assert!(message.contains("reached the model") && message.contains("4211 tokens"), "{message}");
    }

    #[test]
    fn a_percentage_where_a_fraction_belongs_is_not_read() {
        let stdout = REAL.replace("0.9712302684783936", "97.12");
        let obs = observe(&run(&stdout, "", 0));
        let ids: Vec<_> = resources(&obs).iter().map(|r| r.id.clone()).collect();
        assert!(!ids.contains(&"antigravity-gemini-weekly".to_string()), "{ids:?}");
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn no_readable_bucket_is_malformed() {
        let stdout = r#"{"status":"SUCCESS","num_turns":0,"command":{"name":"usage","data":{"groups":[{"name":"Gemini Models","buckets":[{"id":"gemini-weekly","window":"weekly"}]}]}}}"#;
        let obs = observe(&run(stdout, "", 0));
        let (kind, message) = failure(&obs);
        assert_eq!(*kind, FailureKind::MalformedResponse);
        assert!(message.contains("gemini-weekly"), "{message}");
    }

    #[test]
    fn a_lost_sign_in_is_credentials() {
        let stdout = r#"{"status":"ERROR","response":"error getting token source: You are not logged into Antigravity."}"#;
        assert_eq!(*failure(&observe(&run(stdout, "", 1))).0, FailureKind::InvalidCredentials);
        let stderr = "Eligibility check failed: Verify your account to continue.";
        assert_eq!(*failure(&observe(&run("", stderr, 1))).0, FailureKind::InvalidCredentials);
    }

    #[test]
    fn a_launcher_refusal_is_reported_with_its_words() {
        let stderr = "ai-masked: refusing to start: a folder could not be hidden";
        let obs = observe(&run("", stderr, masked_launch::REFUSED_EXIT));
        let (_, message) = failure(&obs);
        assert!(message.contains("ai-masked: refusing to start"), "{message}");
        assert_eq!(obs.scope, MeterScope::Account);
    }

    #[test]
    fn windows_and_groups_read_generally() {
        assert_eq!(window_secs("weekly"), Some(604_800));
        assert_eq!(window_secs("5h"), Some(18_000));
        assert_eq!(window_secs("30d"), Some(2_592_000));
        assert_eq!(window_secs("fortnightly"), None);
        assert_eq!(window_label(Some("fortnightly"), None), "window fortnightly");
        assert_eq!(window_label(Some("1h"), Some(3_600)), "1 hour");
        assert_eq!(short_group("Gemini Models"), "Gemini");
        assert_eq!(short_group("Claude and GPT models"), "Claude/GPT");
        assert_eq!(short_group("Imagen"), "Imagen");
    }
}
