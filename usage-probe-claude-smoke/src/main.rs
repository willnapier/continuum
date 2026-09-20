//! `usage-probe-claude-smoke` — does a Claude Code request actually go through?
//!
//! `usage-probe-claude` reads the Max subscription's rate-limit headers: *how
//! much allowance is used*. It never asks *will a request succeed* — and it
//! cannot see an out-of-date CLI, a model the account has lost, a login that
//! `claude` itself no longer accepts, or a settings file that selects
//! something unservable. This probe sends one minimal request down the real
//! path — the same `claude -p` a person would type, with the model the
//! settings select — and reports what came back. Same shape as
//! `usage-probe-codex-smoke`, which exists because of 2026-09-13; the shared
//! spine is `continuum_usage_core::smoke`.
//!
//! It diagnoses nothing. A refusal is reported with the vendor's own words,
//! classified only as far as the words allow, and left for a reader.
//!
//! How the request is kept a ping and not a session:
//!
//! - `--safe-mode` — no hooks, no CLAUDE.md, no skills, no MCP. Without it the
//!   SessionStart hook would inject the whole startup contract and the ping
//!   would cost tens of thousands of tokens and log itself. Auth and model
//!   selection are untouched by safe mode, which is the point.
//! - `--no-session-persistence` — nothing written under `~/.claude/projects`,
//!   so nothing for the continuum importer to pick up.
//! - `--tools ""` — a reply is all that is wanted.
//! - `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` removed from the child's
//!   environment — the path under test is the **subscription login**. Run from
//!   a `cc-clinical` shell this would otherwise ping the DPA key instead and
//!   report the wrong path as healthy (and bill it).
//!
//! **This probe spends the allowance it stands next to.** It is declared
//! `QuotaConsuming`, so the cadence gate holds it to once an hour on a timer;
//! an explicit `usagewatch refresh` always runs it. About 3.6k tokens per run
//! (measured 2026-09-20), against Codex's 10–16k.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use continuum_usage_core::envelope::{FailureKind, Observation, Outcome};
use continuum_usage_core::smoke::{
    self, kind_for, tail, Run, SmokeProbe, Verdict, PROMPT, REQUEST_TIMEOUT,
};
use serde_json::{json, Value};

const SMOKE: SmokeProbe = SmokeProbe {
    probe: "claude-smoke",
    version: env!("CARGO_PKG_VERSION"),
    provider: "anthropic",
    assistant: "claude-code",
    resource_id: "claude-request-path",
};
/// Verified error path: point this at a script that prints a refusal.
const BIN_OVERRIDE: &str = "CONTINUUM_CLAUDE_BIN";

fn main() -> ExitCode {
    let obs = probe();
    println!("{}", serde_json::to_string(&obs).expect("envelope serialises"));
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn probe() -> Observation {
    let claude = match smoke::resolve_bin(BIN_OVERRIDE, "claude") {
        Ok(p) => p,
        Err(e) => return SMOKE.fail(FailureKind::NetworkFailure, format!("no claude binary: {e}")),
    };
    let started = Instant::now();
    let run = match run_claude(&claude) {
        Ok(run) => run,
        Err(e) => return SMOKE.fail(FailureKind::NetworkFailure, e),
    };
    let elapsed = started.elapsed();
    let result = result_object(&run.stdout);
    // What answered, from the vendor's own accounting; on a refusal nothing
    // answered, so fall back to what the settings asked for.
    let model = result
        .as_ref()
        .and_then(model_that_answered)
        .or_else(configured_model);
    let verdict = classify(result.as_ref(), &run);
    let raw = match &verdict {
        Verdict::Ok { tokens } => json!({
            "model": model,
            "claude": claude.display().to_string(),
            "elapsed_secs": elapsed.as_secs_f64(),
            "tokens_used": tokens,
        }),
        Verdict::Refused { .. } => json!({
            "model": model,
            "claude": claude.display().to_string(),
            "exit_code": run.exit_code,
            "api_error_status": result.as_ref().and_then(|r| r.get("api_error_status").cloned()),
            "stderr_tail": tail(&run.stderr, 600),
            "stdout_tail": tail(&run.stdout, 600),
        }),
    };
    SMOKE.observe(verdict, model.as_deref(), elapsed, raw)
}

fn run_claude(claude: &Path) -> Result<Run, String> {
    let workdir = std::env::temp_dir().join("usage-probe-claude-smoke");
    std::fs::create_dir_all(&workdir).map_err(|e| format!("scratch dir: {e}"))?;
    let mut cmd = Command::new(claude);
    cmd.args([
        "-p",
        PROMPT,
        "--output-format",
        "json",
        "--no-session-persistence",
        "--safe-mode",
        "--tools",
        "",
    ])
    .current_dir(&workdir)
    .env_remove("ANTHROPIC_API_KEY")
    .env_remove("ANTHROPIC_AUTH_TOKEN");
    smoke::run_with_deadline(cmd, REQUEST_TIMEOUT)
}

/// `--output-format json` prints one `{"type":"result",…}` object. Take the
/// last line that is one, so a stray warning line above it does not matter.
fn result_object(stdout: &str) -> Option<Value> {
    stdout
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v.get("type").and_then(Value::as_str) == Some("result"))
}

/// Live shapes, 2026-09-20. A reply is `is_error: false` with the lone word in
/// `result`. A refusal is `is_error: true` with the vendor's sentence in
/// `result` and the HTTP status in `api_error_status` — note `subtype` reads
/// `"success"` either way, so it is not consulted.
fn classify(result: Option<&Value>, run: &Run) -> Verdict {
    let Some(r) = result else {
        return smoke::no_reply(run);
    };
    let text = r.get("result").and_then(Value::as_str).unwrap_or_default();
    let is_error = r.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    if is_error {
        let status = r.get("api_error_status").and_then(Value::as_u64);
        let message = if text.trim().is_empty() {
            format!("refused with no message (exit {:?})", run.exit_code)
        } else {
            text.trim().to_string()
        };
        return Verdict::Refused {
            kind: kind_for(status, &message),
            message,
        };
    }
    if smoke::is_ok_reply(text) && run.exit_code == Some(0) {
        return Verdict::Ok { tokens: tokens_used(r) };
    }
    Verdict::Refused {
        kind: FailureKind::MalformedResponse,
        message: format!(
            "unexpected reply (exit {:?}): {}",
            run.exit_code,
            tail(text.trim(), 240)
        ),
    }
}

/// Everything the request was billed for: fresh, cache-written, cache-read
/// and output tokens.
fn tokens_used(result: &Value) -> Option<u64> {
    let usage = result.get("usage")?;
    let sum: u64 = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ]
    .iter()
    .filter_map(|k| usage.get(k).and_then(Value::as_u64))
    .sum();
    (sum > 0).then_some(sum)
}

/// The key of `modelUsage` that carried output — the model that replied.
fn model_that_answered(result: &Value) -> Option<String> {
    let usage = result.get("modelUsage")?.as_object()?;
    usage
        .iter()
        .max_by_key(|(_, v)| v.get("outputTokens").and_then(Value::as_u64).unwrap_or(0))
        .map(|(k, _)| k.clone())
}

/// `"model"` from `~/.claude/settings.json`. Absent means Claude Code's own
/// default.
fn configured_model() -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let text = std::fs::read_to_string(home.join(".claude/settings.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("model")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Live shapes, 2026-09-20, trimmed to the fields that are read.
    const REPLIED: &str = r#"{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"result":"OK","usage":{"input_tokens":2,"cache_creation_input_tokens":3634,"cache_read_input_tokens":0,"output_tokens":4},"modelUsage":{"claude-fable-5-1":{"inputTokens":2,"outputTokens":4}}}"#;
    const REFUSED: &str = r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":404,"result":"There's an issue with the selected model (claude-nonexistent-9). It may not exist or you may not have access to it. Run --model to pick a different model.","modelUsage":{}}"#;

    fn run(stdout: &str, exit_code: i32) -> Run {
        Run { stdout: stdout.into(), stderr: String::new(), exit_code: Some(exit_code) }
    }

    #[test]
    fn a_plain_ok_reply_is_a_reading() {
        let r = run(REPLIED, 0);
        let obj = result_object(&r.stdout);
        assert_eq!(classify(obj.as_ref(), &r), Verdict::Ok { tokens: Some(3_640) });
        assert_eq!(model_that_answered(&obj.unwrap()).as_deref(), Some("claude-fable-5-1"));
    }

    #[test]
    fn a_refusal_is_reported_in_the_vendors_words_despite_subtype_success() {
        let r = run(REFUSED, 1);
        match classify(result_object(&r.stdout).as_ref(), &r) {
            Verdict::Refused { kind, message } => {
                assert_eq!(kind, FailureKind::RequestRefused);
                assert!(message.starts_with("There's an issue with the selected model"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_logged_out_cli_reads_as_rejected_credentials() {
        let out = r#"{"type":"result","is_error":true,"result":"Invalid API key · Please run /login"}"#;
        let r = run(out, 1);
        assert!(matches!(
            classify(result_object(&r.stdout).as_ref(), &r),
            Verdict::Refused { kind: FailureKind::InvalidCredentials, .. }
        ));
    }

    #[test]
    fn a_reply_with_a_nonzero_exit_is_not_trusted() {
        let r = run(REPLIED, 1);
        assert!(matches!(classify(result_object(&r.stdout).as_ref(), &r), Verdict::Refused { .. }));
    }

    #[test]
    fn a_chatty_reply_is_not_the_reply_that_was_asked_for() {
        let out = r#"{"type":"result","is_error":false,"result":"OK — loading the startup contract first."}"#;
        let r = run(out, 0);
        assert!(matches!(
            classify(result_object(&r.stdout).as_ref(), &r),
            Verdict::Refused { kind: FailureKind::MalformedResponse, .. }
        ));
    }

    #[test]
    fn output_that_is_not_the_result_object_is_no_reply() {
        let r = Run { stdout: "Segmentation fault\n".into(), stderr: "boom".into(), exit_code: Some(139) };
        match classify(result_object(&r.stdout).as_ref(), &r) {
            Verdict::Refused { kind, message } => {
                assert_eq!(kind, FailureKind::MalformedResponse);
                assert!(message.contains("boom"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_warning_line_above_the_result_does_not_hide_it() {
        let out = format!("warning: something\n{REPLIED}\n");
        assert!(result_object(&out).is_some());
    }
}
