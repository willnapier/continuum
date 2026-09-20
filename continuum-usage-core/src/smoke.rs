//! Shared spine for the smoke probes — `usage-probe-<assistant>-smoke`.
//!
//! A limits probe asks the vendor *how much allowance is used*. It never asks
//! *will a request succeed*. On 2026-09-13 those answers diverged for Codex
//! (0% of the week used, every request refused for an out-of-date CLI), and
//! the refusal was read as an exhausted allowance. A smoke probe sends one
//! minimal request down the real path — the same headless CLI call a person
//! would type — and reports what came back, in the vendor's own words.
//!
//! What lives here is the part that must not diverge between vendors: the
//! deadline (a hung child would wedge the whole refresh), the
//! words-to-`FailureKind` table, and the envelope shape. What stays in each
//! probe is the vendor part: which binary, which flags, and how to read that
//! CLI's output. Nothing here knows a vendor.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::envelope::{FailureKind, KindHint, Observation, Outcome, Resource, SideEffect};

/// The whole request. Short, so the reply can be checked exactly.
pub const PROMPT: &str = "Reply with exactly the word OK and nothing else.";

/// Inside core's 45s `RUN_TIMEOUT`, with room to report the overrun as a
/// reading rather than be killed silently.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);

/// Who is being pinged. One per probe, all `'static`.
pub struct SmokeProbe {
    /// Bare probe name, e.g. `codex-smoke`.
    pub probe: &'static str,
    pub version: &'static str,
    pub provider: &'static str,
    /// The assistant whose request path this is, e.g. `codex`.
    pub assistant: &'static str,
    /// Stable store identity, e.g. `codex-request-path`.
    pub resource_id: &'static str,
}

pub struct Run {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Ok { tokens: Option<u64> },
    Refused { kind: FailureKind, message: String },
}

impl SmokeProbe {
    pub fn fail(&self, kind: FailureKind, msg: impl Into<String>) -> Observation {
        let mut obs = Observation::failure(self.probe, self.version, self.provider, kind, msg);
        obs.assistant = Some(self.assistant.to_string());
        obs
    }

    /// Turn a verdict into the envelope. `model` names what was actually
    /// tested; `raw` is whatever the probe wants kept beside the reading.
    pub fn observe(
        &self,
        verdict: Verdict,
        model: Option<&str>,
        elapsed: Duration,
        raw: Value,
    ) -> Observation {
        match verdict {
            Verdict::Ok { tokens } => {
                // Short enough for the 22-column status cell with a model
                // name inside.
                let label = match model {
                    Some(m) => format!("Smoke ({m})"),
                    None => "Smoke (default model)".to_string(),
                };
                let mut obs = Observation::ok(
                    self.probe,
                    self.version,
                    self.provider,
                    SideEffect::QuotaConsuming,
                    vec![Resource {
                        id: self.resource_id.into(),
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
                obs.assistant = Some(self.assistant.to_string());
                if let Outcome::Ok { raw: slot, .. } = &mut obs.outcome {
                    *slot = Some(raw);
                }
                obs
            }
            Verdict::Refused { kind, message } => {
                let mut obs = self.fail(
                    kind,
                    match model {
                        Some(m) => format!("{m}: {message}"),
                        None => message,
                    },
                );
                if let Outcome::Failure { raw: slot, .. } = &mut obs.outcome {
                    *slot = Some(raw);
                }
                obs
            }
        }
    }
}

/// `$<env_override>` if set, else `name` on `PATH`, else `~/.local/bin/<name>`
/// (a launchd or systemd timer does not always carry the interactive `PATH`).
/// The override is also the verified error path: point it at a script that
/// prints a refusal and the whole chain renders it.
pub fn resolve_bin(env_override: &str, name: &str) -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os(env_override).filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!("{env_override}={} is not a file", p.display()))
        };
    }
    let on_path = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default();
    let fallback = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin"));
    on_path
        .into_iter()
        .chain(fallback)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("no `{name}` on PATH or in ~/.local/bin"))
}

/// One request, with a hard deadline. `Command::output()` blocks until EOF
/// and has no timeout; a hung child would wedge the whole refresh. stdin is
/// closed; stdout and stderr are captured whole.
pub fn run_with_deadline(mut cmd: Command, timeout: Duration) -> Result<Run, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))?;

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

    let deadline = Instant::now() + timeout;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "no reply within {}s — request path hung",
                    timeout.as_secs()
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

/// Classify as far as the words allow — no further.
pub fn kind_for(status: Option<u64>, message: &str) -> FailureKind {
    let m = message.to_ascii_lowercase();
    match status {
        Some(401) | Some(403) => FailureKind::InvalidCredentials,
        Some(402) | Some(429) => FailureKind::QuotaDenied,
        Some(s) if s >= 500 => FailureKind::ProviderOutage,
        _ if ["usage limit", "rate limit", "quota", "usage balance", "payment required"]
            .iter()
            .any(|w| m.contains(w)) =>
        {
            FailureKind::QuotaDenied
        }
        _ if m.contains("unauthori") || m.contains("log in") || m.contains("login") => {
            FailureKind::InvalidCredentials
        }
        _ => FailureKind::RequestRefused,
    }
}

/// Is this text the reply that was asked for — the lone word, nothing else?
pub fn is_ok_reply(text: &str) -> bool {
    let t = text.trim();
    t.eq_ignore_ascii_case("ok") || t.eq_ignore_ascii_case("ok.")
}

/// The "no reply" refusal: whatever text is available, stderr first.
pub fn no_reply(run: &Run) -> Verdict {
    let source = if run.stderr.trim().is_empty() { &run.stdout } else { &run.stderr };
    Verdict::Refused {
        kind: FailureKind::MalformedResponse,
        message: format!("no reply (exit {:?}): {}", run.exit_code, tail(source, 240).trim()),
    }
}

pub fn tail(s: &str, n: usize) -> String {
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

    #[test]
    fn statuses_and_words_classify_only_as_far_as_they_go() {
        assert_eq!(kind_for(Some(429), "slow down"), FailureKind::QuotaDenied);
        assert_eq!(kind_for(Some(401), "nope"), FailureKind::InvalidCredentials);
        assert_eq!(kind_for(Some(503), "nope"), FailureKind::ProviderOutage);
        assert_eq!(kind_for(None, "You've hit your usage limit"), FailureKind::QuotaDenied);
        assert_eq!(kind_for(None, "402 Grok Build usage balance exhausted"), FailureKind::QuotaDenied);
        assert_eq!(kind_for(None, "Please run /login"), FailureKind::InvalidCredentials);
        assert_eq!(kind_for(Some(400), "requires a newer version"), FailureKind::RequestRefused);
        assert_eq!(kind_for(Some(404), "model may not exist"), FailureKind::RequestRefused);
        assert_eq!(kind_for(None, "something else entirely"), FailureKind::RequestRefused);
    }

    #[test]
    fn only_the_lone_word_is_a_reply() {
        assert!(is_ok_reply("OK"));
        assert!(is_ok_reply(" ok.\n"));
        assert!(!is_ok_reply("OK, loading the startup contract first"));
        assert!(!is_ok_reply(""));
    }

    #[test]
    fn a_hung_child_is_killed_and_reported_not_waited_for() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let err = run_with_deadline(cmd, Duration::from_millis(300)).err().expect("must time out");
        assert!(err.contains("request path hung"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_and_exit_code_come_back_whole() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let run = run_with_deadline(cmd, Duration::from_secs(5)).unwrap();
        assert_eq!((run.stdout.trim(), run.stderr.trim(), run.exit_code), ("out", "err", Some(3)));
    }

    #[test]
    fn a_refusal_carries_the_model_and_the_vendors_words() {
        let p = SmokeProbe {
            probe: "x-smoke",
            version: "1",
            provider: "x",
            assistant: "x",
            resource_id: "x-request-path",
        };
        let obs = p.observe(
            Verdict::Refused { kind: FailureKind::RequestRefused, message: "needs a newer CLI".into() },
            Some("model-9"),
            Duration::from_secs(1),
            Value::Null,
        );
        match obs.outcome {
            Outcome::Failure { kind, message, .. } => {
                assert_eq!(kind, FailureKind::RequestRefused);
                assert_eq!(message, "model-9: needs a newer CLI");
            }
            _ => panic!("expected a failure"),
        }
        assert_eq!(obs.assistant.as_deref(), Some("x"));
    }
}
