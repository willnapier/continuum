//! Probe discovery and invocation.
//!
//! A probe is any executable named `usage-probe-<name>` on `PATH`. It takes no
//! arguments, writes exactly one JSON object to stdout, and exits. That is the
//! entire contract — which is what makes a twenty-line shell script a
//! first-class adapter and keeps vendor code out of core entirely.
//!
//! Core links no vendor SDK, holds no vendor credential, and knows no vendor
//! name. Adding a provider is dropping a file on `PATH`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre::Result;

use crate::envelope::{FailureKind, Observation, Outcome, SideEffect};
use crate::policy::Policy;
use crate::store::ProbeMeta;

pub const PROBE_PREFIX: &str = "usage-probe-";

/// Hard wall-clock cap on a single probe run.
///
/// `Command::output()` has no timeout and blocks until stdout reaches EOF, so a
/// probe that hangs — or whose own child hangs — wedges the entire run with no
/// envelope and no diagnostic. systemd will not start a second instance of a
/// still-running oneshot, so the effect is that the monitor stops working,
/// quietly, until someone notices. The probe is killed at this deadline and the
/// failure recorded like any other.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Probe {
    /// Bare name, e.g. `claude`.
    pub name: String,
    pub path: PathBuf,
}

/// Every probe on `PATH`, deduplicated by name with the first hit winning —
/// the same precedence rule the shell itself applies.
pub fn discover() -> Vec<Probe> {
    let Ok(path) = std::env::var("PATH") else {
        return vec![];
    };
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let Some(name) = file_name.strip_prefix(PROBE_PREFIX) else {
                continue;
            };
            if name.is_empty() || !is_executable(&entry.path()) {
                continue;
            }
            if seen.insert(name.to_string()) {
                out.push(Probe {
                    name: name.to_string(),
                    path: entry.path(),
                });
            }
        }
    }
    out.sort();
    out
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Whether a probe may run now.
///
/// The point of this gate: the Claude probe spends the very allowance it
/// measures. Nothing may drive it at the cadence of a probe that just reads a
/// local file. On an explicit refresh the user has asked, so it always runs; on
/// a schedule, costly probes are held to a far longer minimum interval.
pub fn may_run(
    meta: Option<&ProbeMeta>,
    policy: &Policy,
    now_unix: i64,
    explicit_refresh: bool,
) -> Result<(), i64> {
    if explicit_refresh {
        return Ok(());
    }
    let Some(meta) = meta else {
        // Never seen: its cost is unknown, so let it speak once. From then on
        // its declared side effect governs.
        return Ok(());
    };
    let Some(last) = meta.last_run_unix else {
        return Ok(());
    };
    let min = match meta.last_side_effect {
        Some(SideEffect::Passive) => policy.cadence.passive_min_secs,
        Some(SideEffect::RequestConsuming) => policy.cadence.request_min_secs,
        Some(s) if s.is_costly() => policy.cadence.costly_min_secs,
        // Unknown cost: be conservative rather than generous.
        _ => policy.cadence.costly_min_secs,
        };
    let elapsed = now_unix - last;
    if elapsed >= min {
        Ok(())
    } else {
        Err(min - elapsed)
    }
}

/// Run a probe and parse its envelope.
///
/// A probe that crashes, times out, or emits unparseable output still yields an
/// observation — a structured failure one. Losing that distinction is how a
/// `402 quota exhausted` becomes indistinguishable from a segfault.
pub fn run(probe: &Probe) -> Observation {
    let fail = |kind, msg: String| {
        Observation::failure(&probe.name, "unknown", "unknown", kind, msg)
    };

    let mut child = match Command::new(&probe.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return fail(
                FailureKind::Unknown,
                format!("could not execute {}: {e}", probe.path.display()),
            )
        }
    };

    // Drain both pipes on threads: waiting on the child while a full pipe
    // blocks it would deadlock, and reading one pipe to EOF before the other
    // has the same problem.
    // Results come back over channels, not by joining.
    //
    // Killing the child does NOT necessarily close these pipes: a shell probe's
    // grandchild inherits the same descriptors and keeps them open, so
    // `read_to_end` blocks past the kill and a `join()` would hang exactly as
    // long as the original defect did. Verified — the first version of this fix
    // still had to be killed externally at 90s.
    let mut out_handle = child.stdout.take();
    let mut err_handle = child.stderr.take();
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(h) = out_handle.as_mut() {
            let _ = h.read_to_end(&mut b);
        }
        let _ = out_tx.send(b);
    });
    std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(h) = err_handle.as_mut() {
            let _ = h.read_to_end(&mut b);
        }
        let _ = err_tx.send(b);
    });

    let deadline = Instant::now() + RUN_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return fail(FailureKind::Unknown, format!("waiting on probe failed: {e}"));
            }
        }
    };

    // Short grace for the readers to drain after exit-or-kill, then give up on
    // them. A leaked reader thread on a pipe held open by an orphan is harmless
    // — the process is about to exit — whereas waiting on it is the hang.
    let grace = Duration::from_secs(2);
    let stdout_bytes = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr_bytes = err_rx.recv_timeout(grace).unwrap_or_default();

    let Some(status) = status else {
        return fail(
            FailureKind::ProviderOutage,
            format!(
                "probe exceeded {}s and was killed: {}",
                RUN_TIMEOUT.as_secs(),
                String::from_utf8_lossy(&stderr_bytes)
                    .trim()
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        );
    };

    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let trimmed = stdout.trim();

    if trimmed.is_empty() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        return fail(
            FailureKind::MalformedResponse,
            format!(
                "probe produced no JSON (exit {}): {}",
                status.code().unwrap_or(-1),
                stderr.trim().chars().take(300).collect::<String>()
            ),
        );
    }

    match serde_json::from_str::<Observation>(trimmed) {
        Ok(obs) => obs,
        Err(e) => fail(
            FailureKind::MalformedResponse,
            format!("probe output did not parse as a v2 envelope: {e}"),
        ),
    }
}

/// The envelope core writes when it declines to run a costly probe. A skip is
/// bookkeeping, not a fault — but it is still recorded, so a gap in history is
/// explained rather than mysterious.
pub fn skipped(probe: &Probe, wait_secs: i64) -> Observation {
    let mut obs = Observation::failure(
        &probe.name,
        "core",
        "unknown",
        FailureKind::SkippedByCadence,
        format!("cadence gate: {wait_secs}s remaining before this probe may run again"),
    );
    if let Outcome::Failure { .. } = obs.outcome {
        obs.assistant = None;
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ProbeMeta;

    const NOW: i64 = 1_000_000;

    fn meta(last: i64, se: SideEffect) -> ProbeMeta {
        ProbeMeta {
            last_run_unix: Some(last),
            last_side_effect: Some(se),
        }
    }

    #[test]
    fn costly_probe_cannot_be_driven_at_passive_cadence() {
        let p = Policy::default();
        // 90 seconds ago: fine for a file reader, far too soon for one that
        // spends the quota it measures.
        let recent = meta(NOW - 90, SideEffect::QuotaConsuming);
        assert!(may_run(Some(&recent), &p, NOW, false).is_err());

        let passive = meta(NOW - 90, SideEffect::Passive);
        assert!(may_run(Some(&passive), &p, NOW, false).is_ok());
    }

    #[test]
    fn explicit_refresh_overrides_the_gate() {
        let p = Policy::default();
        let recent = meta(NOW - 1, SideEffect::Chargeable);
        assert!(may_run(Some(&recent), &p, NOW, true).is_ok());
    }

    #[test]
    fn unknown_cost_is_treated_conservatively() {
        let p = Policy::default();
        let unknown = ProbeMeta {
            last_run_unix: Some(NOW - 120),
            last_side_effect: None,
        };
        assert!(
            may_run(Some(&unknown), &p, NOW, false).is_err(),
            "a probe of unknown cost must not be polled like a passive one"
        );
    }

    #[test]
    fn first_sight_of_a_probe_is_allowed() {
        let p = Policy::default();
        assert!(may_run(None, &p, NOW, false).is_ok());
    }

    #[test]
    fn costly_probe_runs_once_the_interval_elapses() {
        let p = Policy::default();
        let old = meta(NOW - p.cadence.costly_min_secs - 1, SideEffect::QuotaConsuming);
        assert!(may_run(Some(&old), &p, NOW, false).is_ok());
    }
}
