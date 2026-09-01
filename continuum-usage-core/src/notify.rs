//! Notification: deciding what is worth interrupting for, and saying it once.
//!
//! Dedup is **per machine**, deliberately. Cross-machine dedup would need shared
//! mutable state over a file-sync protocol — a distributed lock built on the
//! wrong primitive. Seeing the same alert on both machines is the
//! honest v1 cost, and it is cheaper than the bug that lock would introduce.
//!
//! An event fires once per *window*, not once per observation. The window key
//! is the vendor's own `resets_at`, so a 5-hour session limit can alert again
//! after it turns over, but a timer polling every ten minutes inside one window
//! says nothing further.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::Result;

use crate::envelope::{FailureKind, Outcome, StoredObservation};
use crate::policy::{assess_with_history, Baselines, AxisState, Policy};

/// Set this to silence delivery while keeping the decision logic live — useful
/// in timers under test, and the escape hatch if the alerts ever get noisy.
pub const DISABLE_ENV: &str = "USAGEWATCH_DISABLE_NOTIFICATIONS";

/// Entries older than this are pruned; a window that long gone cannot recur.
const RETAIN_SECS: i64 = 30 * 86_400;

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Stable across observations within one window. The dedup key.
    pub id: String,
    pub title: String,
    pub body: String,
}

/// Bucket width for the window key, in seconds.
///
/// The key must be *stable* across observations inside one window, or dedup
/// fails open and the same alert fires on every poll. A vendor that recomputes
/// its reset instant per response — rounding, or "now + remaining" — would drift
/// by a few seconds each time and mint a fresh key each time. Bucketing absorbs
/// that. Five minutes is safe: the shortest window in play is five *hours*, so
/// two genuinely different resets can never land in the same bucket.
const WINDOW_BUCKET_SECS: i64 = 300;

fn window_key(resets_at: Option<i64>) -> String {
    resets_at
        .map(|r| (r / WINDOW_BUCKET_SECS).to_string())
        .unwrap_or_else(|| "none".into())
}

/// Decide what deserves an interruption, given the newest reading per probe.
///
/// Only `Approaching`, `Critical` and `Opportunity` qualify. `NotAssessable`
/// never alerts — an unmeasurable resource is not an emergency, and crying wolf
/// about Grok every ten minutes would train the alerts to be ignored.
pub fn events(
    rows: &[StoredObservation],
    baselines: &Baselines,
    policy: &Policy,
    now_unix: i64,
) -> Vec<Event> {
    let mut out = vec![];

    for row in rows {
        let obs = &row.observation;
        let probe = &obs.probe.name;
        let age = now_unix - row.ingested_at_unix;

        if let Outcome::Failure { kind, message, .. } = &obs.outcome {
            match kind {
                // The account is out, right now. The single most useful alert
                // this tool can produce.
                FailureKind::QuotaDenied => out.push(Event {
                    id: format!("{probe}:probe:exhausted:{}", row.ingested_at_unix / 3600),
                    title: format!("{} exhausted", obs.provider),
                    body: message.chars().take(180).collect(),
                }),
                // Credentials rot silently; that is worth one nudge per day.
                FailureKind::InvalidCredentials => out.push(Event {
                    id: format!("{probe}:probe:credentials:{}", row.ingested_at_unix / 86_400),
                    title: format!("{} credentials rejected", obs.provider),
                    body: message.chars().take(180).collect(),
                }),
                // Outages, network blips and cadence skips are not the user's
                // problem. They show in `usagewatch doctor`.
                _ => {}
            }
            continue;
        }

        for r in obs.resources() {
            let a = assess_with_history(probe, r, baselines, policy, now_unix, age);
            let wk = window_key(r.facets.resets_at);
            let used = a
                .utilization
                .map(|u| format!("{:.0}%", u * 100.0))
                .unwrap_or_else(|| "?".into());

            match a.scarcity {
                AxisState::Critical => out.push(Event {
                    id: format!("{probe}:{}:scarcity:critical:{wk}", r.id),
                    title: format!("{} nearly spent", r.label),
                    body: format!("{used} used on {} ({}).", obs.provider, probe),
                }),
                AxisState::Approaching => out.push(Event {
                    id: format!("{probe}:{}:scarcity:approaching:{wk}", r.id),
                    title: format!("{} filling up", r.label),
                    body: format!("{used} used on {} ({}).", obs.provider, probe),
                }),
                _ => {}
            }

            if let Some(p) = a.projection.filter(|p| p.exhausts_before_reset) {
                out.push(Event {
                    id: format!("{probe}:{}:scarcity:burnrate:{wk}", r.id),
                    title: format!("{} burning fast", r.label),
                    body: format!(
                        "{used} used, but at the current rate it runs out {}h before the reset.",
                        (a.seconds_to_reset.unwrap_or(0) - p.seconds_of_headroom) / 3600
                    ),
                });
            }
            if a.perishability == AxisState::Opportunity {
                let left = a
                    .seconds_to_reset
                    .map(|s| {
                        let h = s / 3600;
                        if h >= 1 {
                            format!("{h}h")
                        } else {
                            format!("{}m", s / 60)
                        }
                    })
                    .unwrap_or_else(|| "soon".into());
                out.push(Event {
                    id: format!("{probe}:{}:perishability:opportunity:{wk}", r.id),
                    title: format!("{} resets in {left}", r.label),
                    body: format!("Only {used} used — surplus expires at the reset."),
                });
            }
        }
    }
    out
}

/// Per-machine record of what has already been said.
pub struct DedupLog {
    path: PathBuf,
    seen: BTreeMap<String, i64>,
}

impl DedupLog {
    pub fn load(state_dir: &Path, machine: &str) -> Self {
        let path = state_dir.join(format!("notified-{machine}.json"));
        let seen = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, seen }
    }

    pub fn already_sent(&self, id: &str) -> bool {
        self.seen.contains_key(id)
    }

    pub fn mark(&mut self, id: &str, now_unix: i64) {
        self.seen.insert(id.to_string(), now_unix);
    }

    pub fn save(&mut self, now_unix: i64) -> Result<()> {
        self.seen.retain(|_, ts| now_unix - *ts < RETAIN_SECS);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.seen)?)?;
        Ok(())
    }
}

pub fn deliver(event: &Event) -> Result<()> {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(&event.body),
            esc(&event.title)
        );
        Command::new("osascript").args(["-e", &script]).status()?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        Command::new("notify-send")
            .args(["--app-name=usagewatch", &event.title, &event.body])
            .status()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Facets, KindHint, Measure, Observation, Resource, SideEffect};

    const NOW: i64 = 1_000_000;

    fn stored(obs: Observation) -> StoredObservation {
        StoredObservation {
            observation: obs,
            machine_id: "desk".into(),
            sequence: 1,
            ingested_at: String::new(),
            ingested_at_unix: NOW,
        }
    }

    fn window(id: &str, util: f64, resets_in: i64, window_secs: i64) -> Resource {
        Resource {
            id: id.into(),
            label: id.into(),
            kind_hint: KindHint::ResetWindow,
            facets: Facets {
                utilization: Some(util),
                resets_at: Some(NOW + resets_in),
                window_secs: Some(window_secs),
                expires_unused: Some(true),
                ..Default::default()
            },
            vendor_status: None,
            vendor_representative: false,
        }
    }

    #[test]
    fn event_id_is_stable_within_a_window_and_changes_after_reset() {
        // Same reset instant, deeper usage: still one window, so one event.
        let a = events(
            &[stored(Observation::ok("claude", "1", "anthropic", SideEffect::QuotaConsuming,
                vec![window("w", 0.90, 600, 18_000)]))],
            &Baselines::new(), &Policy::default(), NOW);
        let b = events(
            &[stored(Observation::ok("claude", "1", "anthropic", SideEffect::QuotaConsuming,
                vec![window("w", 0.95, 600, 18_000)]))],
            &Baselines::new(), &Policy::default(), NOW);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].id, b[0].id, "one window must yield one event");

        // The window turned over: it may speak again.
        let c = events(
            &[stored(Observation::ok("claude", "1", "anthropic", SideEffect::QuotaConsuming,
                vec![window("w", 0.90, 600 + 18_000, 18_000)]))],
            &Baselines::new(), &Policy::default(), NOW);
        assert_ne!(a[0].id, c[0].id, "a new window is a new event");
    }

    #[test]
    fn small_reset_drift_does_not_defeat_dedup() {
        // A vendor recomputing its reset instant per response would otherwise
        // mint a fresh key every poll and alert forever.
        let a = events(
            &[stored(Observation::ok("claude", "1", "anthropic", SideEffect::QuotaConsuming,
                vec![window("w", 0.90, 600, 18_000)]))],
            &Baselines::new(), &Policy::default(), NOW);
        let drifted = events(
            &[stored(Observation::ok("claude", "1", "anthropic", SideEffect::QuotaConsuming,
                vec![window("w", 0.90, 603, 18_000)]))],
            &Baselines::new(), &Policy::default(), NOW);
        assert_eq!(a[0].id, drifted[0].id, "3s of drift must not be a new window");
    }

    #[test]
    fn escalation_from_approaching_to_critical_is_a_new_event() {
        let approaching = events(
            &[stored(Observation::ok("codex", "1", "openai", SideEffect::Passive,
                vec![window("w", 0.78, 900, 18_000)]))],
            &Baselines::new(), &Policy::default(), NOW);
        let critical = events(
            &[stored(Observation::ok("codex", "1", "openai", SideEffect::Passive,
                vec![window("w", 0.90, 900, 18_000)]))],
            &Baselines::new(), &Policy::default(), NOW);
        assert_ne!(approaching[0].id, critical[0].id, "escalation must not be deduped away");
    }

    #[test]
    fn unmeasurable_resources_never_alert() {
        // Grok. Not knowing is not an emergency; alerting here every poll would
        // train the notifications to be ignored.
        let e = events(
            &[stored(Observation::ok("grok", "1", "xai", SideEffect::Passive,
                vec![Resource {
                    id: "week".into(),
                    label: "week".into(),
                    kind_hint: KindHint::Consumption,
                    facets: Facets {
                        consumed: Some(Measure::new(1000.0, "tokens")),
                        ..Default::default()
                    },
                    vendor_status: None,
                    vendor_representative: false,
                }]))],
            &Baselines::new(), &Policy::default(), NOW);
        assert!(e.is_empty(), "{e:?}");
    }

    #[test]
    fn quota_denied_alerts_but_a_cadence_skip_does_not() {
        let denied = events(
            &[stored(Observation::failure("grok", "1", "xai", FailureKind::QuotaDenied, "402"))],
            &Baselines::new(), &Policy::default(), NOW);
        assert_eq!(denied.len(), 1);
        assert!(denied[0].title.contains("exhausted"));

        let skipped = events(
            &[stored(Observation::failure("claude", "core", "anthropic",
                FailureKind::SkippedByCadence, "too soon"))],
            &Baselines::new(), &Policy::default(), NOW);
        assert!(skipped.is_empty());
    }

    #[test]
    fn dedup_log_suppresses_a_repeat_and_survives_a_reload() {
        let dir = std::env::temp_dir().join(format!("usagewatch-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut log = DedupLog::load(&dir, "desk");
        assert!(!log.already_sent("e1"));
        log.mark("e1", NOW);
        log.save(NOW).unwrap();

        let reloaded = DedupLog::load(&dir, "desk");
        assert!(reloaded.already_sent("e1"));
        assert!(!reloaded.already_sent("e2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_entries_are_pruned() {
        let dir = std::env::temp_dir().join(format!("usagewatch-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut log = DedupLog::load(&dir, "desk");
        log.mark("ancient", NOW);
        log.save(NOW + RETAIN_SECS + 1).unwrap();

        assert!(!DedupLog::load(&dir, "desk").already_sent("ancient"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
