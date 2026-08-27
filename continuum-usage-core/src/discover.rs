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
use std::process::Command;

use color_eyre::eyre::Result;

use crate::envelope::{FailureKind, Observation, Outcome, SideEffect};
use crate::policy::Policy;
use crate::store::ProbeMeta;

pub const PROBE_PREFIX: &str = "usage-probe-";

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
    let output = Command::new(&probe.path).output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            return Observation::failure(
                &probe.name,
                "unknown",
                "unknown",
                FailureKind::Unknown,
                format!("could not execute {}: {e}", probe.path.display()),
            )
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    if trimmed.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Observation::failure(
            &probe.name,
            "unknown",
            "unknown",
            FailureKind::MalformedResponse,
            format!(
                "probe produced no JSON (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr.trim().chars().take(300).collect::<String>()
            ),
        );
    }

    match serde_json::from_str::<Observation>(trimmed) {
        Ok(obs) => obs,
        Err(e) => Observation::failure(
            &probe.name,
            "unknown",
            "unknown",
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
