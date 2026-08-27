//! Terminal rendering.
//!
//! One rule governs everything here: **absence renders as absence.** An axis
//! that does not apply prints a dash, not a tick. An axis nobody can measure
//! prints "not assessable", not "healthy". A vendor's own nomination of a
//! representative resource is shown as an annotation, never as *the* number for
//! the account — otherwise a permissive five-hour claim could mask a threatened
//! weekly one.

use crate::envelope::{FailureKind, Outcome, StoredObservation};
use crate::policy::{assess, AxisState, Policy};

pub fn axis_cell(state: AxisState) -> String {
    match state {
        AxisState::Healthy => "ok".into(),
        AxisState::Approaching => "APPROACHING".into(),
        AxisState::Critical => "CRITICAL".into(),
        AxisState::Opportunity => "OPPORTUNITY".into(),
        // Not a tick. The reader must be able to see we did not check.
        AxisState::Inapplicable => "—".into(),
        AxisState::NotAssessable => "?".into(),
        AxisState::Stale => "stale".into(),
    }
}

fn human_duration(secs: i64) -> String {
    if secs <= 0 {
        return "due".into();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h >= 24 {
        format!("{}d {}h", h / 24, h % 24)
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn pct(u: Option<f64>) -> String {
    match u {
        Some(v) => format!("{:.0}%", v * 100.0),
        None => "—".into(),
    }
}

/// Full status across every probe with a stored observation.
pub fn status(rows: &[StoredObservation], policy: &Policy, now_unix: i64) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("No observations yet. Run `usagewatch refresh`.\n");
        return out;
    }

    for row in rows {
        let obs = &row.observation;
        let age = now_unix - row.ingested_at_unix;
        out.push_str(&format!(
            "\n{} ({})  {}  [{}]\n",
            obs.probe.name,
            obs.provider,
            obs.account.as_deref().unwrap_or("-"),
            row.machine_id
        ));

        match &obs.outcome {
            Outcome::Failure { kind, message, .. } => {
                let tag = if kind.is_fault() { "FAILED" } else { "skipped" };
                out.push_str(&format!("  {tag}: {kind:?} — {message}\n"));
                continue;
            }
            Outcome::Ok {
                side_effect,
                resources,
                ..
            } => {
                out.push_str(&format!(
                    "  observed {}s ago, probe cost: {:?}\n",
                    age, side_effect
                ));
                out.push_str(&format!(
                    "  {:<22} {:>6}  {:<13} {:<13} {}\n",
                    "resource", "used", "scarcity", "perishable", "resets in"
                ));
                for r in resources {
                    let a = assess(r, policy, now_unix, age);
                    let star = if r.vendor_representative { " *" } else { "" };
                    out.push_str(&format!(
                        "  {:<22} {:>6}  {:<13} {:<13} {}{}\n",
                        truncate(&r.label, 22),
                        pct(a.utilization),
                        axis_cell(a.scarcity),
                        axis_cell(a.perishability),
                        a.seconds_to_reset
                            .map(human_duration)
                            .unwrap_or_else(|| "—".into()),
                        star
                    ));
                }
            }
        }
    }
    out.push_str("\n  * vendor-nominated representative resource (display hint only)\n");
    out.push_str("  — = axis does not apply here   ? = cannot be measured\n");
    out
}

/// Only what is worth interrupting William for.
pub fn alerts(rows: &[StoredObservation], policy: &Policy, now_unix: i64) -> Vec<String> {
    let mut out = vec![];
    for row in rows {
        let obs = &row.observation;
        let age = now_unix - row.ingested_at_unix;

        if let Outcome::Failure { kind, message, .. } = &obs.outcome {
            // A quota-denied reading is the most informative alert there is:
            // the account is actually out, right now.
            if *kind == FailureKind::QuotaDenied {
                out.push(format!("{} ({}): EXHAUSTED — {}", obs.probe.name, obs.provider, message));
            } else if kind.is_fault() {
                out.push(format!("{} ({}): probe failed — {:?}", obs.probe.name, obs.provider, kind));
            }
            continue;
        }

        for r in obs.resources() {
            let a = assess(r, policy, now_unix, age);
            match a.scarcity {
                AxisState::Critical => out.push(format!(
                    "{} / {}: {} used — approaching the ceiling",
                    obs.probe.name,
                    r.label,
                    pct(a.utilization)
                )),
                AxisState::Approaching => out.push(format!(
                    "{} / {}: {} used",
                    obs.probe.name,
                    r.label,
                    pct(a.utilization)
                )),
                _ => {}
            }
            if a.perishability == AxisState::Opportunity {
                out.push(format!(
                    "{} / {}: only {} used and it resets in {} — might as well use it",
                    obs.probe.name,
                    r.label,
                    pct(a.utilization),
                    a.seconds_to_reset.map(human_duration).unwrap_or_default()
                ));
            }
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Facets, KindHint, Monetary, Observation, Resource, SideEffect};

    fn stored(obs: Observation, age: i64, now: i64) -> StoredObservation {
        StoredObservation {
            observation: obs,
            machine_id: "nimbini".into(),
            sequence: 1,
            ingested_at: "".into(),
            ingested_at_unix: now - age,
        }
    }

    fn res(id: &str, kind: KindHint, facets: Facets) -> Resource {
        Resource {
            id: id.into(),
            label: id.into(),
            kind_hint: kind,
            facets,
            vendor_status: None,
            vendor_representative: false,
        }
    }

    const NOW: i64 = 1_000_000;

    #[test]
    fn inapplicable_never_renders_as_a_tick() {
        assert_eq!(axis_cell(AxisState::Inapplicable), "—");
        assert_ne!(axis_cell(AxisState::Inapplicable), axis_cell(AxisState::Healthy));
        assert_ne!(
            axis_cell(AxisState::NotAssessable),
            axis_cell(AxisState::Healthy)
        );
    }

    #[test]
    fn monetary_resource_raises_no_opportunity_alert() {
        let obs = Observation::ok(
            "claude",
            "0.1.0",
            "anthropic",
            SideEffect::QuotaConsuming,
            vec![res(
                "overage",
                KindHint::Continuous,
                Facets {
                    utilization: Some(0.05),
                    resets_at: Some(NOW + 600),
                    window_secs: Some(2_592_000),
                    expires_unused: Some(false),
                    monetary: Some(Monetary {
                        currency: "USD".into(),
                        spent: None,
                        cap: None,
                    }),
                    ..Default::default()
                },
            )],
        );
        let a = alerts(&[stored(obs, 10, NOW)], &Policy::default(), NOW);
        assert!(a.is_empty(), "must never advise burning credit: {a:?}");
    }

    #[test]
    fn perishable_surplus_produces_a_might_as_well_alert() {
        let obs = Observation::ok(
            "claude",
            "0.1.0",
            "anthropic",
            SideEffect::QuotaConsuming,
            vec![res(
                "weekly",
                KindHint::ResetWindow,
                Facets {
                    utilization: Some(0.25),
                    resets_at: Some(NOW + 3600),
                    window_secs: Some(604_800),
                    expires_unused: Some(true),
                    ..Default::default()
                },
            )],
        );
        let a = alerts(&[stored(obs, 10, NOW)], &Policy::default(), NOW);
        assert_eq!(a.len(), 1);
        assert!(a[0].contains("might as well"), "{a:?}");
    }

    #[test]
    fn quota_denied_failure_surfaces_as_exhausted() {
        let obs = Observation::failure(
            "grok",
            "0.1.0",
            "xai",
            FailureKind::QuotaDenied,
            "402 balance exhausted",
        );
        let a = alerts(&[stored(obs, 10, NOW)], &Policy::default(), NOW);
        assert_eq!(a.len(), 1);
        assert!(a[0].contains("EXHAUSTED"), "{a:?}");
    }

    #[test]
    fn cadence_skip_is_not_an_alert() {
        let obs = Observation::failure(
            "claude",
            "core",
            "anthropic",
            FailureKind::SkippedByCadence,
            "too soon",
        );
        assert!(alerts(&[stored(obs, 10, NOW)], &Policy::default(), NOW).is_empty());
    }
}
