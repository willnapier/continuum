//! Derivation of verdicts from stored facts.
//!
//! Probes never assess. Everything here runs at *read* time against the
//! append-only store, so a corrected threshold can be replayed over history
//! instead of freezing bad verdicts at observation time. That re-derivability
//! is the whole reason assessment lives in core rather than in the adapters.
//!
//! Two independent axes, per the forum decision:
//!
//! * **Scarcity** — how close am I to being unable to work?
//! * **Perishability** — is there surplus that expires unused if I don't spend
//!   it now? This is the "might as well" case.
//!
//! They are orthogonal. A metered API key has scarcity (rate limits) and no
//! perishability (spending faster is never an opportunity). Grok has neither
//! that we can currently read. Forcing one enum across both would make every
//! provider answer a question most cannot coherently be asked.

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use crate::envelope::{Facets, KindHint, Outcome, Resource, StoredObservation};

/// Earliest reading of each resource within its *current* window, keyed by
/// `(probe, resource id)`. The window is matched on `resets_at`, so a rollover
/// starts a fresh baseline instead of averaging across two windows.
pub type Baselines = BTreeMap<(String, String), (Resource, i64)>;

/// Build baselines from stored history.
pub fn baselines(rows: &[StoredObservation]) -> Baselines {
    let mut out: Baselines = BTreeMap::new();
    for row in rows {
        let Outcome::Ok { resources, .. } = &row.observation.outcome else {
            continue;
        };
        for r in resources {
            let key = (row.observation.probe.name.clone(), r.id.clone());
            match out.get(&key) {
                // Same window and older: it is the better baseline.
                Some((existing, at))
                    if existing.facets.resets_at == r.facets.resets_at
                        && *at <= row.ingested_at_unix => {}
                // Different window: the old baseline belongs to a spent cycle.
                Some((existing, _)) if existing.facets.resets_at != r.facets.resets_at => {
                    out.insert(key, (r.clone(), row.ingested_at_unix));
                }
                _ => {
                    out.insert(key, (r.clone(), row.ingested_at_unix));
                }
            }
        }
    }
    out
}

/// Assess a resource, folding in a burn-rate projection when history allows.
pub fn assess_with_history(
    probe: &str,
    resource: &Resource,
    baselines: &Baselines,
    policy: &Policy,
    now_unix: i64,
    age_secs: i64,
) -> Assessment {
    let base = assess(resource, policy, now_unix, age_secs);
    let projection = baselines
        .get(&(probe.to_string(), resource.id.clone()))
        .filter(|(earlier, _)| earlier.facets.resets_at == resource.facets.resets_at)
        .and_then(|(earlier, at)| project(earlier, *at, resource, now_unix));
    apply_projection(base, projection)
}

/// Bump when thresholds or rules change. Recorded alongside every rendered
/// verdict so an old assessment can be told apart from a current one.
pub const POLICY_VERSION: u32 = 1;

/// The state of one axis for one resource.
///
/// The three "cannot say" variants are deliberately distinct. Collapsing them
/// is the failure this whole design exists to prevent: a resource nobody can
/// measure must not render as a green tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AxisState {
    /// Comfortable. Scarcity only.
    Healthy,
    /// Nearing the limit. Scarcity only.
    Approaching,
    /// At or past the reserve threshold. Scarcity only.
    Critical,
    /// Surplus that will expire at the reset. Perishability only — the
    /// "might as well" signal.
    Opportunity,
    /// This axis does not exist for this resource. Renders as *absence*, not as
    /// a tick: a monetary meter has no perishability, and saying "healthy"
    /// would imply we checked.
    Inapplicable,
    /// This axis should exist but the facts needed are missing. Different from
    /// `Inapplicable` and load-bearing: Grok demonstrably has a weekly ceiling
    /// (it returned 402 on 2026-08-26), we simply cannot read it.
    NotAssessable,
    /// The underlying observation is too old to trust.
    Stale,
}

impl AxisState {
    /// Worth putting in front of the user unprompted.
    pub fn is_notable(self) -> bool {
        matches!(
            self,
            AxisState::Approaching | AxisState::Critical | AxisState::Opportunity
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            AxisState::Healthy => "healthy",
            AxisState::Approaching => "approaching",
            AxisState::Critical => "critical",
            AxisState::Opportunity => "opportunity",
            AxisState::Inapplicable => "—",
            AxisState::NotAssessable => "not assessable",
            AxisState::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// Utilization at which scarcity becomes `Approaching`.
    pub approaching: f64,
    /// Utilization at which scarcity becomes `Critical`.
    pub critical: f64,
    /// Opportunity fires in the last `lead_fraction` of a window. Scaling to
    /// the window rather than a fixed hour count means a 5h window and a 7d
    /// window both get a proportionate warning.
    pub lead_fraction: f64,
    /// Cap on that lead, so a very long window does not nag for days.
    pub lead_cap_secs: i64,
    /// Above this utilization there is no meaningful surplus left to burn.
    pub opportunity_max_utilization: f64,
    /// Observations older than this cannot be trusted.
    pub stale_after_secs: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        // Carried over from the shipped Codex monitor: opportunity in the final
        // 2/7 of a window capped at 48h, reserve threat at 85%.
        Self {
            approaching: 0.75,
            critical: 0.85,
            lead_fraction: 2.0 / 7.0,
            lead_cap_secs: 48 * 3600,
            opportunity_max_utilization: 0.80,
            stale_after_secs: 6 * 3600,
        }
    }
}

/// Minimum seconds between runs, by how much running the probe costs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cadence {
    pub passive_min_secs: i64,
    pub request_min_secs: i64,
    /// Costly probes spend the allowance they measure. On explicit refresh they
    /// always run; on a schedule they are rate-limited hard.
    pub costly_min_secs: i64,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            passive_min_secs: 60,
            request_min_secs: 300,
            costly_min_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub version: u32,
    pub thresholds: Thresholds,
    pub cadence: Cadence,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            version: POLICY_VERSION,
            thresholds: Thresholds::default(),
            cadence: Cadence::default(),
        }
    }
}

/// A burn-rate projection for one resource.
///
/// Utilization alone is a **lagging average** and it hides a burst. A monthly
/// allowance can read a comfortable 75% while the last four days are running
/// at seven times the earlier daily rate, so the remaining quarter will be gone
/// days before the reset. Scarcity that only reads the level, never the slope,
/// says "fine" right up until it says "empty".
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    /// Units consumed per second, measured between two observations.
    pub rate_per_sec: f64,
    /// When the remaining allowance runs out at that rate.
    pub exhausted_at_unix: i64,
    /// Whether that lands before the window resets.
    pub exhausts_before_reset: bool,
    /// Seconds of headroom left at the current rate.
    pub seconds_of_headroom: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    pub resource_id: String,
    pub scarcity: AxisState,
    pub perishability: AxisState,
    /// Seconds until the window turns over, when known.
    pub seconds_to_reset: Option<i64>,
    pub utilization: Option<f64>,
    /// Present only when two observations in the same window allow a rate.
    pub projection: Option<Projection>,
    pub policy_version: u32,
}

impl Assessment {
    pub fn is_notable(&self) -> bool {
        self.scarcity.is_notable() || self.perishability.is_notable()
    }
}

/// Measure the burn rate between an earlier and a current reading of the same
/// resource, and project it forward to the reset.
///
/// Returns `None` unless both readings carry a consumption figure in the same
/// unit, the interval is positive, and consumption actually rose — a flat or
/// falling counter means the window turned over and the comparison is void.
pub fn project(
    earlier: &Resource,
    earlier_at: i64,
    current: &Resource,
    now_unix: i64,
) -> Option<Projection> {
    let (a, b) = (earlier.facets.consumed.as_ref()?, current.facets.consumed.as_ref()?);
    if a.unit != b.unit {
        return None;
    }
    let elapsed = now_unix - earlier_at;
    let burned = b.value - a.value;
    if elapsed <= 0 || burned <= 0.0 {
        return None;
    }
    let remaining = match current.facets.remaining.as_ref() {
        Some(r) if r.unit == b.unit => r.value,
        _ => {
            let limit = current.facets.limit.as_ref().filter(|l| l.unit == b.unit)?;
            (limit.value - b.value).max(0.0)
        }
    };
    let rate = burned / elapsed as f64;
    let headroom = (remaining / rate) as i64;
    let exhausted_at = now_unix + headroom;
    Some(Projection {
        rate_per_sec: rate,
        exhausted_at_unix: exhausted_at,
        exhausts_before_reset: current
            .facets
            .resets_at
            .map(|r| exhausted_at < r)
            .unwrap_or(false),
        seconds_of_headroom: headroom,
    })
}

/// Fold a projection into an existing assessment.
///
/// A resource projected to run dry before its window resets is `Critical`
/// however low its current utilization reads — that is the whole point.
pub fn apply_projection(mut assessment: Assessment, projection: Option<Projection>) -> Assessment {
    if let Some(p) = projection {
        if p.exhausts_before_reset
            && matches!(assessment.scarcity, AxisState::Healthy | AxisState::Approaching)
        {
            assessment.scarcity = AxisState::Critical;
        }
        // Surplus you are on course to spend is not surplus.
        if p.exhausts_before_reset && assessment.perishability == AxisState::Opportunity {
            assessment.perishability = AxisState::Healthy;
        }
        assessment.projection = Some(p);
    }
    assessment
}

/// Derive a utilization for scarcity purposes, from whatever the probe supplied.
fn effective_utilization(f: &Facets) -> Option<f64> {
    if let Some(u) = f.utilization {
        return Some(u);
    }
    // remaining + limit is as good as a reported percentage.
    match (&f.remaining, &f.limit) {
        (Some(rem), Some(lim)) if lim.value > 0.0 && rem.unit == lim.unit => {
            Some((1.0 - rem.value / lim.value).clamp(0.0, 1.0))
        }
        _ => match (&f.consumed, &f.limit) {
            (Some(used), Some(lim)) if lim.value > 0.0 && used.unit == lim.unit => {
                Some((used.value / lim.value).clamp(0.0, 1.0))
            }
            _ => None,
        },
    }
}

/// Assess one resource. `age_secs` is how old the observation is; `now_unix`
/// anchors reset arithmetic.
pub fn assess(resource: &Resource, policy: &Policy, now_unix: i64, age_secs: i64) -> Assessment {
    let t = &policy.thresholds;
    let f = &resource.facets;
    let util = effective_utilization(f);
    let seconds_to_reset = f.resets_at.map(|r| r - now_unix);

    // Staleness dominates: an old reading tells you nothing about now.
    if age_secs > t.stale_after_secs {
        return Assessment {
            resource_id: resource.id.clone(),
            scarcity: AxisState::Stale,
            perishability: AxisState::Stale,
            seconds_to_reset,
            utilization: util,
            projection: None,
            policy_version: policy.version,
        };
    }

    // ---- Scarcity ----------------------------------------------------------
    let scarcity = if resource.kind_hint == KindHint::Opaque {
        // Retained and rendered, never coloured.
        AxisState::Inapplicable
    } else if let Some(u) = util {
        if u >= t.critical {
            AxisState::Critical
        } else if u >= t.approaching {
            AxisState::Approaching
        } else {
            AxisState::Healthy
        }
    } else if resource.kind_hint.implies_capacity() {
        // A ceiling is understood to exist; we just could not read it.
        AxisState::NotAssessable
    } else if f.limit.is_some() {
        // Capacity declared but no consumption figure.
        AxisState::NotAssessable
    } else {
        // Consumption-only: Grok. We know from a live 402 that a weekly ceiling
        // exists, so this is emphatically NOT `Inapplicable` — that would be
        // the lie of omission the forum spent two rounds guarding against.
        AxisState::NotAssessable
    };

    // ---- Perishability -----------------------------------------------------
    let perishability = match f.expires_unused {
        // Nothing perishes. Monetary meters land here: they have a reset (an
        // accounting period) but burning credit faster is never an opportunity.
        Some(false) => AxisState::Inapplicable,
        None => {
            if resource.kind_hint == KindHint::Opaque {
                AxisState::Inapplicable
            } else {
                AxisState::NotAssessable
            }
        }
        Some(true) => match (util, seconds_to_reset, f.window_secs) {
            (Some(u), Some(to_reset), window) if to_reset > 0 => {
                let lead = window
                    .map(|w| ((w as f64) * t.lead_fraction) as i64)
                    .unwrap_or(t.lead_cap_secs)
                    .min(t.lead_cap_secs);
                if to_reset <= lead && u <= t.opportunity_max_utilization {
                    AxisState::Opportunity
                } else {
                    AxisState::Healthy
                }
            }
            // Perishable but unreadable, or the reset has already passed.
            _ => AxisState::NotAssessable,
        },
    };

    Assessment {
        resource_id: resource.id.clone(),
        scarcity,
        perishability,
        seconds_to_reset,
        utilization: util,
        projection: None,
        policy_version: policy.version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Measure, Monetary};

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
    fn monetary_meter_never_reports_an_opportunity() {
        // The Claude overage row: has a reset, but nothing perishes.
        let r = res(
            "unified-overage",
            KindHint::Continuous,
            Facets {
                utilization: Some(0.02),
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
        );
        let a = assess(&r, &Policy::default(), NOW, 0);
        assert_eq!(a.perishability, AxisState::Inapplicable);
        assert_eq!(a.scarcity, AxisState::Healthy);
    }

    #[test]
    fn surplus_near_a_reset_is_an_opportunity() {
        // Claude's 7d window, late on Sunday, only 30% used.
        let r = res(
            "unified-7d",
            KindHint::ResetWindow,
            Facets {
                utilization: Some(0.30),
                resets_at: Some(NOW + 3600),
                window_secs: Some(604_800),
                expires_unused: Some(true),
                ..Default::default()
            },
        );
        let a = assess(&r, &Policy::default(), NOW, 0);
        assert_eq!(a.perishability, AxisState::Opportunity);
    }

    #[test]
    fn no_opportunity_early_in_a_window() {
        let r = res(
            "unified-7d",
            KindHint::ResetWindow,
            Facets {
                utilization: Some(0.30),
                resets_at: Some(NOW + 500_000),
                window_secs: Some(604_800),
                expires_unused: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(
            assess(&r, &Policy::default(), NOW, 0).perishability,
            AxisState::Healthy
        );
    }

    #[test]
    fn heavy_use_near_reset_is_not_an_opportunity() {
        let r = res(
            "unified-5h",
            KindHint::ResetWindow,
            Facets {
                utilization: Some(0.92),
                resets_at: Some(NOW + 60),
                window_secs: Some(18_000),
                expires_unused: Some(true),
                ..Default::default()
            },
        );
        let a = assess(&r, &Policy::default(), NOW, 0);
        assert_eq!(a.scarcity, AxisState::Critical);
        assert_eq!(a.perishability, AxisState::Healthy);
    }

    #[test]
    fn consumption_only_is_not_assessable_never_healthy() {
        // Grok. This is the case the whole design exists for.
        let r = res(
            "grok-weekly",
            KindHint::Consumption,
            Facets {
                consumed: Some(Measure::new(208_100_000.0, "tokens")),
                ..Default::default()
            },
        );
        let a = assess(&r, &Policy::default(), NOW, 0);
        assert_eq!(a.scarcity, AxisState::NotAssessable);
        assert_ne!(a.scarcity, AxisState::Healthy);
        assert_ne!(a.scarcity, AxisState::Inapplicable);
    }

    #[test]
    fn stale_observations_report_stale_on_both_axes() {
        let r = res(
            "unified-5h",
            KindHint::ResetWindow,
            Facets {
                utilization: Some(0.10),
                resets_at: Some(NOW + 100),
                expires_unused: Some(true),
                ..Default::default()
            },
        );
        let a = assess(&r, &Policy::default(), NOW, 99_999);
        assert_eq!(a.scarcity, AxisState::Stale);
        assert_eq!(a.perishability, AxisState::Stale);
    }

    #[test]
    fn utilization_derives_from_remaining_over_limit() {
        let r = res(
            "dpa-tokens",
            KindHint::RollingRecovery,
            Facets {
                remaining: Some(Measure::new(200.0, "tokens")),
                limit: Some(Measure::new(1000.0, "tokens")),
                expires_unused: Some(false),
                ..Default::default()
            },
        );
        let a = assess(&r, &Policy::default(), NOW, 0);
        assert_eq!(a.utilization, Some(0.8));
        assert_eq!(a.scarcity, AxisState::Approaching);
        assert_eq!(a.perishability, AxisState::Inapplicable);
    }

    #[test]
    fn a_burst_is_caught_even_though_utilization_reads_comfortable() {
        // Real Grok numbers observed 2026-08-27. 11,558 of 15,500 credits
        // used = 75%, under the 75% threshold, and four days to the reset.
        // But 6,523 of that burned in the last four days: at that rate the
        // remaining 3,942 lasts about 2.4 days, not 4.
        let mk = |consumed: f64| Resource {
            id: "grok-monthly-credits".into(),
            label: "Monthly allowance".into(),
            kind_hint: KindHint::ResetWindow,
            facets: Facets {
                utilization: Some(consumed / 15_500.0),
                consumed: Some(Measure::new(consumed, "credits")),
                remaining: Some(Measure::new(15_500.0 - consumed, "credits")),
                limit: Some(Measure::new(15_500.0, "credits")),
                resets_at: Some(1_788_220_800),
                window_secs: Some(31 * 86_400),
                expires_unused: Some(true),
                ..Default::default()
            },
            vendor_status: None,
            vendor_representative: true,
        };
        let four_days_ago = 1_788_220_800 - 8 * 86_400;
        let now = 1_788_220_800 - 4 * 86_400;
        let earlier = mk(11_558.0 - 6_523.0);
        let current = mk(11_558.0);

        let plain = assess(&current, &Policy::default(), now, 0);
        assert_eq!(plain.scarcity, AxisState::Healthy, "the level alone looks fine");

        let p = project(&earlier, four_days_ago, &current, now).expect("projection");
        assert!(p.exhausts_before_reset, "must see it running dry early");
        // ~2.4 days of headroom against 4 days to the reset.
        assert!(p.seconds_of_headroom < 3 * 86_400, "got {}s", p.seconds_of_headroom);

        let with = apply_projection(plain, Some(p));
        assert_eq!(with.scarcity, AxisState::Critical, "the slope must override the level");
    }

    #[test]
    fn a_projection_that_lands_after_the_reset_changes_nothing() {
        let mk = |consumed: f64| Resource {
            id: "r".into(),
            label: "r".into(),
            kind_hint: KindHint::ResetWindow,
            facets: Facets {
                utilization: Some(consumed / 1000.0),
                consumed: Some(Measure::new(consumed, "credits")),
                remaining: Some(Measure::new(1000.0 - consumed, "credits")),
                resets_at: Some(NOW + 86_400),
                expires_unused: Some(true),
                ..Default::default()
            },
            vendor_status: None,
            vendor_representative: false,
        };
        let p = project(&mk(10.0), NOW - 3600, &mk(11.0), NOW).expect("projection");
        assert!(!p.exhausts_before_reset);
        let a = apply_projection(assess(&mk(11.0), &Policy::default(), NOW, 0), Some(p));
        assert_eq!(a.scarcity, AxisState::Healthy);
    }

    #[test]
    fn a_window_rollover_voids_the_comparison() {
        // Consumption fell, so the counter reset. No rate can be inferred.
        let mk = |consumed: f64| Resource {
            id: "r".into(), label: "r".into(), kind_hint: KindHint::ResetWindow,
            facets: Facets {
                consumed: Some(Measure::new(consumed, "credits")),
                remaining: Some(Measure::new(100.0, "credits")),
                ..Default::default()
            },
            vendor_status: None, vendor_representative: false,
        };
        assert!(project(&mk(900.0), NOW - 3600, &mk(5.0), NOW).is_none());
        assert!(project(&mk(50.0), NOW - 3600, &mk(50.0), NOW).is_none(), "flat is not a rate");
    }

    #[test]
    fn opaque_is_rendered_but_never_coloured() {
        let r = res("mystery", KindHint::Opaque, Facets::default());
        let a = assess(&r, &Policy::default(), NOW, 0);
        assert_eq!(a.scarcity, AxisState::Inapplicable);
        assert_eq!(a.perishability, AxisState::Inapplicable);
        assert!(!a.is_notable());
    }
}
