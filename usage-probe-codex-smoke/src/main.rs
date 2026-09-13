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

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use continuum_usage_core::envelope::{
    FailureKind, KindHint, Observation, Outcome, Resource, SideEffect,
};
use serde_json::{json, Value};

const PROBE: &str = "codex-smoke";
const PROVIDER: &str = "openai";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const PROMPT: &str = "Reply with exactly the word OK and nothing else.";
/// Inside core's 45s `RUN_TIMEOUT`, with room to report the overrun as a
/// reading rather than be killed silently.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);

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
    obs.assistant = Some("codex".to_string());
    obs
}

fn probe() -> Observation {
    let codex = match continuum_core::codex_cli::resolve_codex(None) {
        Ok(r) => r.path,
        Err(e) => return fail(FailureKind::NetworkFailure, format!("no codex binary: {e}")),
    };
    let model = configured_model();
    let started = Instant::now();
    let run = match run_codex(&codex) {
        Ok(run) => run,
        Err(e) => return fail(FailureKind::NetworkFailure, e),
    };
    let elapsed = started.elapsed();
    let verdict = classify(&run.stdout, &run.stderr, run.exit_code);
    // Short enough for the 22-column status cell with a model name inside.
    let label = match &model {
        Some(m) => format!("Smoke ({m})"),
        None => "Smoke (default model)".to_string(),
    };

    match verdict {
        Verdict::Ok { tokens } => {
            let mut obs = Observation::ok(
                PROBE,
                VERSION,
                PROVIDER,
                SideEffect::QuotaConsuming,
                vec![Resource {
                    id: "codex-request-path".into(),
                    label,
                    kind_hint: KindHint::Opaque,
                    facets: Default::default(),
                    vendor_status: Some(format!(
                        "OK in {:.1}s{}",
                        elapsed.as_secs_f64(),
                        tokens.map(|t| format!(", {t} tokens")).unwrap_or_default()
                    )),
                    vendor_representative: false,
                }],
            );
            obs.assistant = Some("codex".to_string());
            if let Outcome::Ok { raw, .. } = &mut obs.outcome {
                *raw = Some(json!({
                    "model": model,
                    "codex": codex.display().to_string(),
                    "elapsed_secs": elapsed.as_secs_f64(),
                    "tokens_used": tokens,
                }));
            }
            obs
        }
        Verdict::Refused { kind, message } => {
            let mut obs = fail(
                kind,
                match &model {
                    Some(m) => format!("{m}: {message}"),
                    None => message,
                },
            );
            if let Outcome::Failure { raw, .. } = &mut obs.outcome {
                *raw = Some(json!({
                    "model": model,
                    "codex": codex.display().to_string(),
                    "exit_code": run.exit_code,
                    "stderr_tail": tail(&run.stderr, 600),
                    "stdout_tail": tail(&run.stdout, 600),
                }));
            }
            obs
        }
    }
}

struct Run {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

/// One request, with a hard deadline. `Command::output()` blocks until EOF
/// and has no timeout; a hung child would wedge the whole refresh.
fn run_codex(codex: &PathBuf) -> Result<Run, String> {
    let workdir = std::env::temp_dir().join("usage-probe-codex-smoke");
    std::fs::create_dir_all(&workdir).map_err(|e| format!("scratch dir: {e}"))?;

    let mut child = Command::new(codex)
        .args([
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
        .arg(PROMPT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", codex.display()))?;

    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    if let Some(mut so) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = so.read_to_string(&mut s);
            let _ = out_tx.send(s);
        });
    }
    if let Some(mut se) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = se.read_to_string(&mut s);
            let _ = err_tx.send(s);
        });
    }

    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "no reply within {}s — request path hung",
                    REQUEST_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    let grace = Duration::from_secs(2);
    Ok(Run {
        stdout: out_rx.recv_timeout(grace).unwrap_or_default(),
        stderr: err_rx.recv_timeout(grace).unwrap_or_default(),
        exit_code,
    })
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Ok { tokens: Option<u64> },
    Refused { kind: FailureKind, message: String },
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
    let replied = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .any(|l| l.eq_ignore_ascii_case("ok") || l.eq_ignore_ascii_case("ok."));
    if replied && exit_code == Some(0) {
        // The reply lands on stdout; the transcript, token count included,
        // goes to stderr.
        return Verdict::Ok {
            tokens: tokens_used(&combined),
        };
    }
    let message = if stderr.trim().is_empty() {
        format!(
            "no reply (exit {:?}): {}",
            exit_code,
            tail(stdout, 240).trim()
        )
    } else {
        format!("no reply (exit {:?}): {}", exit_code, tail(stderr, 240).trim())
    };
    Verdict::Refused {
        kind: FailureKind::MalformedResponse,
        message,
    }
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

/// Classify as far as the words allow — no further.
fn kind_for(status: Option<u64>, message: &str) -> FailureKind {
    let m = message.to_ascii_lowercase();
    match status {
        Some(401) | Some(403) => FailureKind::InvalidCredentials,
        Some(429) => FailureKind::QuotaDenied,
        Some(s) if s >= 500 => FailureKind::ProviderOutage,
        _ if m.contains("usage limit") || m.contains("rate limit") || m.contains("quota") => {
            FailureKind::QuotaDenied
        }
        _ if m.contains("unauthori") || m.contains("log in") || m.contains("login") => {
            FailureKind::InvalidCredentials
        }
        _ => FailureKind::RequestRefused,
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

fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        s.chars().skip(count - n).collect()
    }
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
    fn statuses_and_words_classify_only_as_far_as_they_go() {
        assert_eq!(kind_for(Some(429), "slow down"), FailureKind::QuotaDenied);
        assert_eq!(kind_for(Some(401), "nope"), FailureKind::InvalidCredentials);
        assert_eq!(kind_for(Some(503), "nope"), FailureKind::ProviderOutage);
        assert_eq!(kind_for(None, "You've hit your usage limit"), FailureKind::QuotaDenied);
        assert_eq!(kind_for(Some(400), "requires a newer version"), FailureKind::RequestRefused);
        assert_eq!(kind_for(None, "something else entirely"), FailureKind::RequestRefused);
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
