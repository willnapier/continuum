//! `usage-probe-codex-smoke` — does a Codex request actually go through?
//!
//! The limits probe asks the vendor *how much allowance is used*. It never
//! asks *will a request succeed*. On 2026-09-13 the two answers diverged:
//! Codex read 0% of its week while every request failed with "The
//! 'gpt-6-astra' model requires a newer version of Codex", and the refusal
//! was read as an exhausted allowance. No limits endpoint can see that class
//! of failure. This probe sends one minimal request down the real path — the
//! same `codex exec` a person would type, with the model the config selects —
//! and reports what came back.
//!
//! It diagnoses nothing. A refusal is reported with the vendor's own words,
//! classified only as far as the words allow, and left for a reader.
//!
//! **This probe spends the allowance it stands next to.** It is declared
//! `QuotaConsuming`, so the cadence gate holds it to once an hour on a timer;
//! an explicit `usagewatch refresh` always runs it. About 10k tokens per run,
//! nearly all cached prompt.
//!
//! The deadline, the words-to-kind table and the envelope shape are shared
//! with the other smoke probes in `continuum_usage_core::smoke`; what is here
//! is only what is Codex's own.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use continuum_usage_core::envelope::{FailureKind, Observation, Outcome};
use continuum_usage_core::smoke::{
    self, kind_for, tail, Run, SmokeProbe, Verdict, PROMPT, REQUEST_TIMEOUT,
};
use serde_json::{json, Value};

const SMOKE: SmokeProbe = SmokeProbe {
    probe: "codex-smoke",
    version: env!("CARGO_PKG_VERSION"),
    provider: "openai",
    assistant: "codex",
    resource_id: "codex-request-path",
};

fn main() -> ExitCode {
    let obs = probe();
    println!("{}", serde_json::to_string(&obs).expect("envelope serialises"));
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn probe() -> Observation {
    let codex = match continuum_core::codex_cli::resolve_codex(None) {
        Ok(r) => r.path,
        Err(e) => return SMOKE.fail(FailureKind::NetworkFailure, format!("no codex binary: {e}")),
    };
    let model = configured_model();
    let started = Instant::now();
    let run = match run_codex(&codex) {
        Ok(run) => run,
        Err(e) => return SMOKE.fail(FailureKind::NetworkFailure, e),
    };
    let elapsed = started.elapsed();
    let verdict = classify(&run.stdout, &run.stderr, run.exit_code);
    let raw = match &verdict {
        Verdict::Ok { tokens } => json!({
            "model": model,
            "codex": codex.display().to_string(),
            "elapsed_secs": elapsed.as_secs_f64(),
            "tokens_used": tokens,
        }),
        Verdict::Refused { .. } => json!({
            "model": model,
            "codex": codex.display().to_string(),
            "exit_code": run.exit_code,
            "stderr_tail": tail(&run.stderr, 600),
            "stdout_tail": tail(&run.stdout, 600),
        }),
    };
    SMOKE.observe(verdict, model.as_deref(), elapsed, raw)
}

/// `--ephemeral` so no session file is written; read-only sandbox; a scratch
/// directory; whatever `model` the config selects.
fn run_codex(codex: &Path) -> Result<Run, String> {
    let workdir = std::env::temp_dir().join("usage-probe-codex-smoke");
    std::fs::create_dir_all(&workdir).map_err(|e| format!("scratch dir: {e}"))?;
    let mut cmd = Command::new(codex);
    cmd.args([
        "exec",
        "--skip-git-repo-check",
        "--ephemeral",
        "--color",
        "never",
        "-s",
        "read-only",
        "-C",
    ])
    .arg(&workdir)
    .arg(PROMPT);
    smoke::run_with_deadline(cmd, REQUEST_TIMEOUT)
}

/// Read the transcript `codex exec` prints and decide whether a reply came
/// back.
///
/// Codex reports server errors as `ERROR: {"type":"error","status":400,...}`
/// lines and repeats them per retry; the vendor message inside is the whole
/// diagnosis. A success is the model's reply — the lone `OK` — with no error
/// line anywhere. Anything else is refused with whatever text is available.
fn classify(stdout: &str, stderr: &str, exit_code: Option<i32>) -> Verdict {
    let combined = format!("{stdout}\n{stderr}");
    if let Some(err) = combined
        .lines()
        .filter_map(|l| l.trim().strip_prefix("ERROR:"))
        .next()
    {
        let (status, message) = parse_error(err.trim());
        return Verdict::Refused {
            kind: kind_for(status, &message),
            message,
        };
    }
    let replied = stdout.lines().any(smoke::is_ok_reply);
    if replied && exit_code == Some(0) {
        // The reply lands on stdout; the transcript, token count included,
        // goes to stderr.
        return Verdict::Ok {
            tokens: tokens_used(&combined),
        };
    }
    smoke::no_reply(&Run {
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
        exit_code,
    })
}

/// `{"type":"error","status":400,"error":{"message":"..."}}` → (status, message).
/// Falls back to the raw text when it is not JSON.
fn parse_error(text: &str) -> (Option<u64>, String) {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => {
            let status = v.get("status").and_then(Value::as_u64);
            let message = v
                .pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| text.to_string());
            (status, message)
        }
        Err(_) => (None, text.to_string()),
    }
}

/// The `tokens used` / `9,860` pair Codex prints after a reply.
fn tokens_used(stdout: &str) -> Option<u64> {
    let mut lines = stdout.lines().map(str::trim);
    while let Some(l) = lines.next() {
        if l.eq_ignore_ascii_case("tokens used") {
            return lines
                .next()
                .and_then(|n| n.replace(',', "").parse().ok());
        }
    }
    None
}

/// `model = "..."` from `~/.codex/config.toml`, so the label names what was
/// actually tested. Absent means Codex's own default.
fn configured_model() -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let text = std::fs::read_to_string(home.join(".codex/config.toml")).ok()?;
    model_from_config(&text)
}

fn model_from_config(text: &str) -> Option<String> {
    let table: toml::Table = text.parse().ok()?;
    table.get("model")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSED: &str = "OpenAI Codex v0.152.1\n--------\nmodel: gpt-6-astra\n--------\nuser\nReply with exactly the word OK and nothing else.\nwarning: Model metadata for `gpt-6-astra` not found.\nERROR: {\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-6-astra' model requires a newer version of Codex. Please upgrade to the latest app or CLI and try again.\"}}\nERROR: {\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-6-astra' model requires a newer version of Codex. Please upgrade to the latest app or CLI and try again.\"}}\n";

    // Live shape, 2026-09-13: the reply alone on stdout, the transcript on
    // stderr.
    const REPLIED: &str = "OK\n";
    const TRANSCRIPT: &str = "OpenAI Codex v0.154.0\n--------\nmodel: gpt-6-astra\n--------\nuser\nReply with exactly the word OK and nothing else.\ncodex\nOK\ntokens used\n9,860\n";

    #[test]
    fn the_2026_09_13_refusal_is_reported_in_the_vendors_words() {
        // Exit code was 0 — the transcript, not the status, carries the truth.
        let v = classify(REFUSED, "", Some(0));
        match v {
            Verdict::Refused { kind, message } => {
                assert_eq!(kind, FailureKind::RequestRefused);
                assert!(message.starts_with("The 'gpt-6-astra' model requires a newer version"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_plain_ok_reply_is_a_reading() {
        assert_eq!(classify(REPLIED, TRANSCRIPT, Some(0)), Verdict::Ok { tokens: Some(9_860) });
    }

    #[test]
    fn a_reply_with_a_nonzero_exit_is_not_trusted() {
        assert!(matches!(classify(REPLIED, TRANSCRIPT, Some(1)), Verdict::Refused { .. }));
    }

    #[test]
    fn the_transcript_alone_is_not_a_reply() {
        // stderr echoes the model's "OK" too; only stdout carries the reply.
        assert!(matches!(classify("", TRANSCRIPT, Some(0)), Verdict::Refused { .. }));
    }

    #[test]
    fn the_prompt_echo_does_not_count_as_a_reply() {
        // The transcript repeats the prompt, which contains "OK" — but not on
        // a line of its own.
        let no_reply = "user\nReply with exactly the word OK and nothing else.\n";
        assert!(matches!(classify(no_reply, "", Some(0)), Verdict::Refused { .. }));
    }

    #[test]
    fn a_non_json_error_line_is_kept_verbatim() {
        let (status, msg) = parse_error("connection reset by peer");
        assert_eq!(status, None);
        assert_eq!(msg, "connection reset by peer");
    }

    #[test]
    fn model_comes_from_the_config_table_not_a_regex() {
        let cfg = "# model = \"not-this\"\nmodel = \"gpt-6-astra\"\nmodel_reasoning_effort = \"high\"\n[profiles.x]\nmodel = \"other\"\n";
        assert_eq!(model_from_config(cfg).as_deref(), Some("gpt-6-astra"));
        assert_eq!(model_from_config("model_reasoning_effort = \"high\"\n"), None);
    }
}
