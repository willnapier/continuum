//! `usage-probe-grok-smoke` — does a Grok Build request actually go through?
//!
//! `usage-probe-grok` reads the billing endpoint: *how much allowance is
//! used*. It never asks *will a request succeed* — and the two fail
//! independently. The billing read goes stale on an expired OIDC token that a
//! real `grok` run would simply refresh; a real run can be refused for a CLI
//! version, a withdrawn model or an exhausted balance while billing still
//! answers. This probe sends one minimal request down the real path — the
//! same `grok -p` a person would type, with the default model — and reports
//! what came back. Same shape as `usage-probe-codex-smoke`, which exists
//! because of 2026-09-13; the shared spine is `continuum_usage_core::smoke`.
//!
//! It diagnoses nothing. A refusal is reported with the vendor's own words,
//! classified only as far as the words allow, and left for a reader.
//!
//! How the request is kept a ping and not a session:
//!
//! - `--system-prompt-override` — the user rules tell a fresh Grok Build
//!   session to load the startup contract before anything else. Measured
//!   2026-09-20 without the override: Grok announced it would do that,
//!   ran out of its one turn, and exited 1 without ever saying OK.
//! - `--tools "" --disable-web-search --no-subagents --max-turns 1` — a reply
//!   is all that is wanted, and one turn is all it may take.
//! - **Grok has no ephemeral mode**, so each run leaves a session under
//!   `~/.grok/sessions/<encoded scratch cwd>/`. Left alone those would be
//!   imported into continuum-logs by the Stop/SessionEnd hook and counted by
//!   `usage-probe-grok` as this host's Build mix — 24 phantom sessions a day.
//!   So the scratch cwd has a reserved name
//!   (`continuum_core::adapters::grok_cli::SMOKE_CWD_NAME`): `continuum-grok`
//!   skips it, and this probe deletes it before and after every run.
//!
//! **This probe spends the allowance it stands next to.** It is declared
//! `QuotaConsuming`, so the cadence gate holds it to once an hour on a timer;
//! an explicit `usagewatch refresh` always runs it. About 20k tokens and
//! roughly one US cent of the shared weekly pool per run (measured
//! 2026-09-20). If the weekly pool is exhausted and Auto Top Up is on, that
//! cent comes from the wallet instead.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use continuum_core::adapters::grok_cli::{is_smoke_cwd, SMOKE_CWD_NAME};
use continuum_usage_core::envelope::{FailureKind, Observation, Outcome};
use continuum_usage_core::smoke::{
    self, kind_for, tail, Run, SmokeProbe, Verdict, PROMPT, REQUEST_TIMEOUT,
};
use serde_json::{json, Value};

const SMOKE: SmokeProbe = SmokeProbe {
    probe: "grok-smoke",
    version: env!("CARGO_PKG_VERSION"),
    provider: "xai",
    assistant: "grok",
    resource_id: "grok-request-path",
};
/// Verified error path: point this at a script that prints a refusal.
const BIN_OVERRIDE: &str = "CONTINUUM_GROK_BIN";
const SYSTEM_PROMPT: &str =
    "You are a connectivity check. Follow the user's instruction literally.";

fn main() -> ExitCode {
    let obs = probe();
    println!("{}", serde_json::to_string(&obs).expect("envelope serialises"));
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn probe() -> Observation {
    let grok = match smoke::resolve_bin(BIN_OVERRIDE, "grok") {
        Ok(p) => p,
        Err(e) => return SMOKE.fail(FailureKind::NetworkFailure, format!("no grok binary: {e}")),
    };
    let sessions = sessions_root();
    // Before as well as after: a run killed at the deadline never reaches the
    // second sweep.
    let swept_before = sessions.as_deref().map(sweep_smoke_sessions).unwrap_or(0);
    let started = Instant::now();
    let run = run_grok(&grok);
    let elapsed = started.elapsed();
    let swept_after = sessions.as_deref().map(sweep_smoke_sessions).unwrap_or(0);
    let run = match run {
        Ok(run) => run,
        Err(e) => return SMOKE.fail(FailureKind::NetworkFailure, e),
    };

    let output = output_object(&run.stdout);
    let model = output.as_ref().and_then(model_that_answered);
    let verdict = classify(output.as_ref(), &run);
    let raw = match &verdict {
        Verdict::Ok { tokens } => json!({
            "model": model,
            "grok": grok.display().to_string(),
            "elapsed_secs": elapsed.as_secs_f64(),
            "tokens_used": tokens,
            "cost_usd": output.as_ref().and_then(|o| o.get("total_cost_usd").cloned()),
            "smoke_session_dirs_removed": swept_before + swept_after,
        }),
        Verdict::Refused { .. } => json!({
            "model": model,
            "grok": grok.display().to_string(),
            "exit_code": run.exit_code,
            "stderr_tail": tail(&run.stderr, 600),
            "stdout_tail": tail(&run.stdout, 600),
        }),
    };
    SMOKE.observe(verdict, model.as_deref(), elapsed, raw)
}

fn run_grok(grok: &Path) -> Result<Run, String> {
    let workdir = std::env::temp_dir().join(SMOKE_CWD_NAME);
    std::fs::create_dir_all(&workdir).map_err(|e| format!("scratch dir: {e}"))?;
    let mut cmd = Command::new(grok);
    cmd.args(["-p", PROMPT, "--output-format", "json", "--cwd"])
        .arg(&workdir)
        .args([
            "--tools",
            "",
            "--disable-web-search",
            "--no-subagents",
            "--max-turns",
            "1",
            "--system-prompt-override",
            SYSTEM_PROMPT,
        ])
        .current_dir(&workdir);
    smoke::run_with_deadline(cmd, REQUEST_TIMEOUT)
}

/// `~/.grok` (or `$GROK_HOME`) `/sessions`.
fn sessions_root() -> Option<PathBuf> {
    let base = match std::env::var_os("GROK_HOME").filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".grok"),
    };
    Some(base.join("sessions"))
}

/// Delete the session groups this probe's own scratch cwd produced — direct
/// children of the sessions root whose encoded name ends in the reserved
/// directory name, and nothing else. Returns how many were removed.
fn sweep_smoke_sessions(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.is_symlink() && is_smoke_cwd(p))
        .filter(|p| std::fs::remove_dir_all(p).is_ok())
        .count()
}

/// `--output-format json` prints one object: pretty-printed across lines for
/// a reply, on a single line for an error. Try the whole of stdout, then the
/// last line that parses.
fn output_object(stdout: &str) -> Option<Value> {
    serde_json::from_str::<Value>(stdout.trim()).ok().or_else(|| {
        stdout
            .lines()
            .rev()
            .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
    })
}

/// Live shapes, 2026-09-20. A reply is `{"text":"OK","stopReason":"end_turn",…}`
/// with exit 0. A refusal is `{"type":"error","message":"…"}` with exit 1 and
/// the same sentence after `Error:` on stderr.
fn classify(output: Option<&Value>, run: &Run) -> Verdict {
    if let Some(message) = output
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("error"))
        .and_then(|o| o.get("message").and_then(Value::as_str))
        .or_else(|| {
            // No JSON at all: the `Error:` line is the vendor's words.
            output.is_none().then(|| {
                run.stderr.lines().find_map(|l| l.trim().strip_prefix("Error:")).map(str::trim)
            })?
        })
    {
        let status = leading_status(message);
        return Verdict::Refused {
            kind: kind_for(status, message),
            message: message.to_string(),
        };
    }
    let Some(o) = output else {
        return smoke::no_reply(run);
    };
    let text = o.get("text").and_then(Value::as_str).unwrap_or_default();
    if smoke::is_ok_reply(text) && run.exit_code == Some(0) {
        let tokens = o.pointer("/usage/total_tokens").and_then(Value::as_u64);
        return Verdict::Ok { tokens };
    }
    // A request went through but the reply is not the one asked for — most
    // likely the rules won over the system prompt again. Say so; do not call
    // it a refusal.
    Verdict::Refused {
        kind: FailureKind::MalformedResponse,
        message: format!(
            "unexpected reply (exit {:?}, stop {}): {}",
            run.exit_code,
            o.get("stopReason").and_then(Value::as_str).unwrap_or("?"),
            tail(text.trim(), 200)
        ),
    }
}

/// `402 Payment Required — …` → 402. Only a message that *starts* with a
/// three-digit HTTP status counts; a number elsewhere is just a number.
fn leading_status(message: &str) -> Option<u64> {
    let head: String = message.trim_start().chars().take_while(char::is_ascii_digit).collect();
    (head.len() == 3).then(|| head.parse().ok()).flatten().filter(|s| (400..600).contains(s))
}

/// The key of `modelUsage` with the most calls — the model that replied.
fn model_that_answered(output: &Value) -> Option<String> {
    let usage = output.get("modelUsage")?.as_object()?;
    usage
        .iter()
        .max_by_key(|(_, v)| v.get("modelCalls").and_then(Value::as_u64).unwrap_or(0))
        .map(|(k, _)| k.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Live shapes, 2026-09-20, trimmed to the fields that are read.
    const REPLIED: &str = "{\n  \"text\": \"OK\",\n  \"stopReason\": \"end_turn\",\n  \"usage\": {\n    \"input_tokens\": 19334,\n    \"total_tokens\": 19912\n  },\n  \"total_cost_usd\": 0.01408688,\n  \"modelUsage\": {\n    \"grok-4.6-build\": { \"modelCalls\": 1 }\n  }\n}\n";
    const RULES_WON: &str = "{\n  \"text\": \"I'll load the session startup contract first, then reply as requested.\",\n  \"stopReason\": \"cancelled\",\n  \"usage\": { \"total_tokens\": 20819 }\n}\n";
    const REFUSED: &str = "{\"type\":\"error\",\"message\":\"Couldn't set model 'grok-nonexistent-9': Invalid params: \\\"unknown model id\\\". Run 'grok models' to see available models.\"}\n";

    fn run(stdout: &str, stderr: &str, exit_code: i32) -> Run {
        Run { stdout: stdout.into(), stderr: stderr.into(), exit_code: Some(exit_code) }
    }

    #[test]
    fn a_plain_ok_reply_is_a_reading() {
        let r = run(REPLIED, "", 0);
        let o = output_object(&r.stdout);
        assert_eq!(classify(o.as_ref(), &r), Verdict::Ok { tokens: Some(19_912) });
        assert_eq!(model_that_answered(&o.unwrap()).as_deref(), Some("grok-4.6-build"));
    }

    #[test]
    fn a_refusal_is_reported_in_the_vendors_words() {
        let r = run(REFUSED, "Error: Couldn't set model", 1);
        match classify(output_object(&r.stdout).as_ref(), &r) {
            Verdict::Refused { kind, message } => {
                assert_eq!(kind, FailureKind::RequestRefused);
                assert!(message.starts_with("Couldn't set model 'grok-nonexistent-9'"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_2026_09_20_rules_over_prompt_reply_is_not_called_ok_or_refused() {
        let r = run(RULES_WON, "Error: max turns reached", 1);
        match classify(output_object(&r.stdout).as_ref(), &r) {
            Verdict::Refused { kind, message } => {
                assert_eq!(kind, FailureKind::MalformedResponse);
                assert!(message.contains("stop cancelled"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_exhausted_balance_is_quota_not_a_generic_refusal() {
        let out = r#"{"type":"error","message":"402 Payment Required — Grok Build usage balance exhausted"}"#;
        let r = run(out, "", 1);
        assert!(matches!(
            classify(output_object(&r.stdout).as_ref(), &r),
            Verdict::Refused { kind: FailureKind::QuotaDenied, .. }
        ));
    }

    #[test]
    fn with_no_json_the_error_line_on_stderr_is_the_message() {
        let r = run("", "Error: not logged in — run `grok login`\n", 1);
        match classify(None, &r) {
            Verdict::Refused { kind, message } => {
                assert_eq!(kind, FailureKind::InvalidCredentials);
                assert_eq!(message, "not logged in — run `grok login`");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_reply_with_a_nonzero_exit_is_not_trusted() {
        let r = run(REPLIED, "", 1);
        assert!(matches!(classify(output_object(&r.stdout).as_ref(), &r), Verdict::Refused { .. }));
    }

    #[test]
    fn only_a_leading_http_status_is_a_status() {
        assert_eq!(leading_status("402 Payment Required"), Some(402));
        assert_eq!(leading_status("Couldn't set model after 402 tries"), None);
        assert_eq!(leading_status("2026 was a year"), None);
        assert_eq!(leading_status("200 OK"), None);
    }

    #[test]
    fn the_sweep_removes_only_the_smoke_probes_own_session_groups() {
        let root = std::env::temp_dir().join(format!("grok-smoke-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let smoke_dir = root.join(format!("%2Ftmp%2F{SMOKE_CWD_NAME}"));
        let real_dir = root.join("%2FUsers%2Fu");
        std::fs::create_dir_all(smoke_dir.join("0190-session")).unwrap();
        std::fs::create_dir_all(real_dir.join("0190-session")).unwrap();
        std::fs::write(root.join("prompt_history.jsonl"), "{}").unwrap();

        assert_eq!(sweep_smoke_sessions(&root), 1);
        assert!(!smoke_dir.exists());
        assert!(real_dir.join("0190-session").is_dir());
        assert!(root.join("prompt_history.jsonl").is_file());
        assert_eq!(sweep_smoke_sessions(&root), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
