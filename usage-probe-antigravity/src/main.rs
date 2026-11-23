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
//! A reading is all or nothing (review round 1, continuum#9):
//!
//! - **The run must be clean.** Exit 0, `status` `SUCCESS`, and the reply's own
//!   accounting present and zero (`num_turns` 0, `usage.total_tokens` 0).
//!   Missing accounting is refused, not assumed zero. This detects a model
//!   turn *after* it happened; it cannot prevent the first one if a later
//!   `agy` starts sending `/usage` to the model. That is a stated limit.
//! - **Every bucket must read.** One unreadable bucket (a fraction outside
//!   0..=1, a missing field, a bucket that is not an object) fails the whole
//!   reading. Dropping it would hide exactly the limit it measures, and the
//!   other rows would read as a complete, healthy account.
//!
//! On Linux `agy` starts through the mask launcher `ai-masked`
//! (`continuum_usage_core::masked_launch`, `~/Assistants/shared/AI-FOOTING-MASK.md`),
//! in plan mode, from a new private (0700) folder made for that one run and
//! removed after it. A missing launcher is a failed reading, never a direct
//! start.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
/// How close `reset_time` must be to "now + window" to be the synthetic reset
/// of a window that has not started (see [`bucket_resource`]).
const NOT_STARTED_SLACK_SECS: i64 = 120;

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

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
    observe(&run, now_unix())
}

/// A folder made new for one run: `create_dir`, not `create_dir_all`, so an
/// existing path (a folder left behind, or a link planted there) is an error
/// rather than reused; mode 0700. Removed when dropped.
struct RunDir(PathBuf);

impl RunDir {
    fn new() -> Result<Self, String> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let path = std::env::temp_dir()
            .join(format!("usage-probe-antigravity-{}-{nanos:09}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&path).map_err(|e| format!("run folder {}: {e}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `agy -p /usage` in plan mode, from its own empty folder, so nothing in the
/// caller's folder or an earlier run's is offered to it as a workspace.
fn run_agy(agy: &Path) -> Result<Run, String> {
    let workdir = RunDir::new()?;
    let mut cmd = masked_launch::harness_command(agy)?;
    cmd.current_dir(&workdir.0).args([
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

/// The whole reading from one finished run. `now` is the reading's clock, for
/// telling a window that has not started from one that has.
fn observe(run: &Run, now: i64) -> Observation {
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
    if status != "SUCCESS" || run.exit_code != Some(0) {
        let said = reply
            .get("response")
            .or_else(|| reply.get("error"))
            .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
            .unwrap_or_default();
        let stderr = tail(run.stderr.trim(), 200);
        let message = format!(
            "agy /usage status {status:?}, exit {:?}: {}{}",
            run.exit_code,
            tail(said.trim(), 300),
            if stderr.is_empty() { String::new() } else { format!(" (stderr: {stderr})") }
        );
        return with_raw(fail(refusal_kind(&message), message), reply);
    }

    if let Err(message) = no_model_activity(&reply) {
        return with_raw(fail(FailureKind::MalformedResponse, message), reply);
    }

    match resources_from(&reply, now) {
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

/// The reply's own accounting must be present and show no model work. A
/// positive count is confirmed model activity; a missing one is an unexpected
/// reply, and neither is read as a quota reading.
fn no_model_activity(reply: &Value) -> Result<(), String> {
    let turns = reply.get("num_turns").and_then(Value::as_u64);
    let tokens = reply.pointer("/usage/total_tokens").and_then(Value::as_u64);
    match (turns, tokens) {
        (Some(0), Some(0)) => Ok(()),
        (Some(t), Some(k)) => Err(format!(
            "agy ran a model turn for /usage ({t} turns, {k} tokens spent): it no longer answers \
             the slash command itself, and each probe run spends quota. Check \
             `agy -p /usage --output-format json` by hand."
        )),
        _ => Err(format!(
            "agy /usage reply has no turn or token accounting (num_turns {}, usage.total_tokens {}), \
             so it cannot be shown that no model turn ran",
            turns.map_or("missing".into(), |n| n.to_string()),
            tokens.map_or("missing".into(), |n| n.to_string()),
        )),
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

/// One resource per bucket, or an error naming every bucket that would not
/// read. Partial readings are never returned.
fn resources_from(reply: &Value, now: i64) -> Result<Vec<Resource>, String> {
    let Some(command) = reply.get("command") else {
        return Err("agy /usage reply has no command block: an unexpected reply, not a reading".into());
    };
    if command.get("name").and_then(Value::as_str) != Some("usage") {
        return Err(format!("agy answered another command: {}", command.get("name").unwrap_or(&Value::Null)));
    }
    let groups = command
        .pointer("/data/groups")
        .and_then(Value::as_array)
        .ok_or("agy /usage reply has no data.groups list")?;

    let mut resources = vec![];
    let mut unreadable = vec![];
    for (gi, group) in groups.iter().enumerate() {
        let group_name = group.get("name").and_then(Value::as_str);
        let buckets = group.get("buckets").and_then(Value::as_array);
        let (Some(group_name), Some(buckets)) = (group_name, buckets) else {
            unreadable.push(format!("group {gi} (no name or no buckets list)"));
            continue;
        };
        for (bi, bucket) in buckets.iter().enumerate() {
            match bucket_resource(group_name, bucket, now) {
                Ok(r) => resources.push(r),
                Err(why) => {
                    let id = bucket.get("id").and_then(Value::as_str).map_or(format!("{group_name} #{bi}"), str::to_string);
                    unreadable.push(format!("{id} ({why})"));
                }
            }
        }
    }
    if !unreadable.is_empty() {
        return Err(format!("agy /usage reply has unreadable limits: {}", unreadable.join("; ")));
    }
    if resources.is_empty() {
        return Err("agy /usage reply holds no limit buckets".into());
    }
    Ok(resources)
}

fn bucket_resource(group_name: &str, bucket: &Value, now: i64) -> Result<Resource, String> {
    let id = bucket.get("id").and_then(Value::as_str).ok_or("no id")?;
    let remaining = bucket.get("remaining_fraction").and_then(Value::as_f64).ok_or("no remaining_fraction")?;
    // Refuse, do not clamp: a percentage arriving here (97.12) is a scale
    // change, not a reading.
    if !remaining.is_finite() || !(0.0..=1.0).contains(&remaining) {
        return Err(format!("remaining_fraction {remaining} outside 0..=1"));
    }
    let window = bucket.get("window").and_then(Value::as_str);
    let window_secs = window.and_then(window_secs);
    let reset_time = bucket.get("reset_time").and_then(Value::as_str).ok_or("no reset_time")?;
    let resets_at = chrono::DateTime::parse_from_rfc3339(reset_time)
        .map_err(|e| format!("reset_time {reset_time:?}: {e}"))?
        .timestamp();

    // A window nobody has used has not started: `agy` reports its reset as
    // "now + window", which moves with every read (23 Nov 2026). Nothing is
    // counting down, so nothing can perish, and the moving time is not a
    // deadline; core also matches windows on `resets_at`, so passing it on
    // would start a new window at every read. Both signals are required: the
    // full fraction and the synthetic time.
    let not_started = remaining == 1.0
        && window_secs.is_some_and(|w| (resets_at - (now + w)).abs() <= NOT_STARTED_SLACK_SECS);

    Ok(Resource {
        id: format!("antigravity-{id}"),
        label: format!("{} ({})", short_group(group_name), window_label(window, window_secs)),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization: Some(1.0 - remaining),
            resets_at: (!not_started).then_some(resets_at),
            window_secs,
            // A started window "will fully refresh" at its reset (the vendor's
            // own bucket description): the fraction returns to 1, so what was
            // not spent is not carried over. That is `USAGEWATCH.md`'s
            // `Some(true)`, surplus lost at the reset. A window that has not
            // started has nothing counting down: `Some(false)`.
            expires_unused: Some(!not_started),
            ..Default::default()
        },
        vendor_status: not_started.then(|| "not started".to_string()),
        vendor_representative: id == REPRESENTATIVE_BUCKET,
    })
}

/// `weekly` and `5h` are the two `agy` sends; `<n>h`, `<n>d` and `<n>m` are
/// read generally, anything else is a window of unknown length.
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

    /// A real reply, captured 23 Nov 2026 12:15:36 UTC from agy 1.5.15. The
    /// Claude/GPT buckets were unused: their reset is that moment + window.
    const REAL: &str = include_str!("../tests/fixtures/usage-2026-11-23.json");
    fn captured_at() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2026-11-23T12:15:36Z").unwrap().timestamp()
    }

    fn run(stdout: &str, stderr: &str, exit_code: i32) -> Run {
        Run { stdout: stdout.into(), stderr: stderr.into(), exit_code: Some(exit_code) }
    }

    fn read(stdout: &str) -> Observation {
        observe(&run(stdout, "", 0), captured_at())
    }

    fn resources(obs: &Observation) -> &[Resource] {
        match &obs.outcome {
            Outcome::Ok { resources, .. } => resources,
            Outcome::Failure { kind, message, .. } => panic!("failure {kind:?}: {message}"),
        }
    }

    fn failure(obs: &Observation) -> (FailureKind, String) {
        match &obs.outcome {
            Outcome::Failure { kind, message, .. } => (kind.clone(), message.clone()),
            Outcome::Ok { .. } => panic!("expected a failure"),
        }
    }

    #[test]
    fn the_real_reply_gives_four_account_buckets() {
        let obs = read(REAL);
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

    /// The unused Claude/GPT buckets: no deadline, nothing perishes.
    #[test]
    fn a_window_that_has_not_started_has_no_reset_and_nothing_perishes() {
        let obs = read(REAL);
        for r in &resources(&obs)[2..] {
            assert_eq!(r.facets.utilization, Some(0.0), "{}", r.id);
            assert_eq!(r.facets.resets_at, None, "{}", r.id);
            assert_eq!(r.facets.expires_unused, Some(false), "{}", r.id);
            assert_eq!(r.vendor_status.as_deref(), Some("not started"), "{}", r.id);
        }
        // A full bucket whose reset is not "now + window" has started (it may
        // round to 1): its reset stands.
        let later = observe(&run(REAL, "", 0), captured_at() - 3_600);
        let r = &resources(&later)[2];
        assert!(r.facets.resets_at.is_some() && r.facets.expires_unused == Some(true), "{r:?}");
    }

    #[test]
    fn a_banner_before_the_reply_is_skipped() {
        let stdout = format!("A new version of agy is available\n{}", REAL.trim());
        assert_eq!(resources(&read(&stdout)).len(), 4);
    }

    /// Known red: `/usage` sent to the model as a prompt, with or without a
    /// command block. Never a reading.
    #[test]
    fn model_activity_is_refused_in_any_shape() {
        let prose = r#"{"status":"SUCCESS","response":"Gemini Models Weekly 98%","num_turns":1,"usage":{"total_tokens":4211}}"#;
        let (kind, message) = failure(&read(prose));
        assert_eq!(kind, FailureKind::MalformedResponse);
        assert!(message.contains("1 turns, 4211 tokens"), "{message}");

        let with_block = REAL.replace(r#""num_turns":0"#, r#""num_turns":1"#);
        assert_ne!(with_block, REAL);
        let (kind, message) = failure(&read(&with_block));
        assert_eq!(kind, FailureKind::MalformedResponse);
        assert!(message.contains("model turn"), "{message}");

        let tokens_only = REAL.replace(r#""total_tokens":0"#, r#""total_tokens":12"#);
        assert_ne!(tokens_only, REAL);
        assert!(failure(&read(&tokens_only)).1.contains("12 tokens"));
    }

    #[test]
    fn missing_accounting_is_refused_not_assumed_zero() {
        let mut reply: Value = serde_json::from_str(REAL).unwrap();
        reply.as_object_mut().unwrap().remove("num_turns");
        let (kind, message) = failure(&read(&reply.to_string()));
        assert_eq!(kind, FailureKind::MalformedResponse);
        assert!(message.contains("num_turns missing"), "{message}");

        let mut reply: Value = serde_json::from_str(REAL).unwrap();
        reply.as_object_mut().unwrap().remove("usage");
        assert!(failure(&read(&reply.to_string())).1.contains("usage.total_tokens missing"));
    }

    #[test]
    fn a_reply_without_its_command_block_is_unexpected_not_proof_of_a_turn() {
        let mut reply: Value = serde_json::from_str(REAL).unwrap();
        reply.as_object_mut().unwrap().remove("command");
        let (kind, message) = failure(&read(&reply.to_string()));
        assert_eq!(kind, FailureKind::MalformedResponse);
        assert!(message.contains("no command block") && !message.contains("model turn"), "{message}");
    }

    #[test]
    fn a_nonzero_exit_with_success_shaped_json_is_not_a_reading() {
        let obs = observe(&run(REAL, "panic: something", 2), captured_at());
        let (_, message) = failure(&obs);
        assert!(message.contains("exit Some(2)") && message.contains("panic: something"), "{message}");
    }

    /// Known red for the range check, and the reason a reading is all or
    /// nothing: the bucket that bounds Neon's overnight work must not vanish
    /// while the rest read as a healthy account.
    #[test]
    fn one_unreadable_bucket_fails_the_whole_reading() {
        let percentage = REAL.replace("0.9712302684783936", "97.12");
        assert_ne!(percentage, REAL);
        let (kind, message) = failure(&read(&percentage));
        assert_eq!(kind, FailureKind::MalformedResponse);
        assert!(message.contains("gemini-weekly (remaining_fraction 97.12 outside 0..=1)"), "{message}");

        let no_reset = REAL.replace(r#""reset_time":"2026-11-23T15:28:53Z""#, r#""reset_time":"soon""#);
        assert_ne!(no_reset, REAL);
        assert!(failure(&read(&no_reset)).1.contains("gemini-5h (reset_time"));

        let not_object = REAL.replacen(r#"{"id":"3p-5h""#, r#""junk",{"id":"3p-5h""#, 1);
        assert_ne!(not_object, REAL);
        assert!(failure(&read(&not_object)).1.contains("Claude and GPT models #1 (no id)"));

        let no_buckets = REAL.replacen(r#""buckets":["#, r#""bucketz":["#, 1);
        assert!(failure(&read(&no_buckets)).1.contains("group 0 (no name or no buckets list)"));
    }

    #[test]
    fn a_lost_sign_in_is_credentials() {
        let stdout = r#"{"status":"ERROR","response":"error getting token source: You are not logged into Antigravity."}"#;
        assert_eq!(failure(&observe(&run(stdout, "", 1), captured_at())).0, FailureKind::InvalidCredentials);
        let stderr = "Eligibility check failed: Verify your account to continue.";
        assert_eq!(failure(&observe(&run("", stderr, 1), captured_at())).0, FailureKind::InvalidCredentials);
    }

    #[test]
    fn a_launcher_refusal_is_reported_with_its_words() {
        let stderr = "ai-masked: refusing to start: a folder could not be hidden";
        let obs = observe(&run("", stderr, masked_launch::REFUSED_EXIT), captured_at());
        let (_, message) = failure(&obs);
        assert!(message.contains("ai-masked: refusing to start"), "{message}");
        assert_eq!(obs.scope, MeterScope::Account);
    }

    #[test]
    fn each_run_folder_is_new_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let a = RunDir::new().unwrap();
        let b = RunDir::new().unwrap();
        assert_ne!(a.0, b.0);
        assert_eq!(std::fs::read_dir(&a.0).unwrap().count(), 0);
        assert_eq!(std::fs::metadata(&a.0).unwrap().permissions().mode() & 0o777, 0o700);
        let path = a.0.clone();
        std::fs::write(path.join("left-by-agy"), b"x").unwrap();
        drop(a);
        assert!(!path.exists(), "removed with what agy left in it");
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
