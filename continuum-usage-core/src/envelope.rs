//! Schema v2 observation envelope.
//!
//! Decided on design-forum thread `continuum-resource-observatory-v1`
//! (DECIDED 2026-08-27). The shape here is deliberately *compositional*: a
//! resource is a bag of optional facets plus a `kind_hint` that owns no fields.
//!
//! The round-1 proposal was a closed tagged union
//! (`reset-window | rolling-bucket | monetary | opaque`). It was rejected on the
//! evidence of Anthropic's `anthropic-ratelimit-unified-overage-*` headers,
//! which carry a reset *and* spend semantics at once. Tagging that row
//! `monetary` discards the reset the vendor sent; tagging it `reset-window`
//! invites core to paint perishability onto a credit meter; splitting it in two
//! invents structure the vendor never emitted. Optional facets hold it whole.
//!
//! Nothing in this module assesses anything. Probes emit facts; verdicts are
//! derived in `policy`, so stored history can be replayed under corrected
//! thresholds.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped only when a field changes meaning. v1 was `VendorUsage`, which hoisted
/// a single non-optional `used_percent` to the top level and kept an untyped
/// `secondary` escape hatch — it could not hold Claude (3+ concurrent
/// resources) or Grok (no remaining concept at all).
pub const SCHEMA_VERSION: u32 = 2;

/// What running the probe costs. Declared by the probe, enforced by core:
/// a quota-consuming probe must never be driven at passive-probe cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SideEffect {
    /// Reads local files or a local socket. Free. May heartbeat.
    Passive,
    /// Costs a network request but no metered allowance.
    RequestConsuming,
    /// Spends the very allowance it is measuring. The Claude probe is this:
    /// one Haiku token per observation.
    QuotaConsuming,
    /// Costs money directly.
    Chargeable,
}

impl SideEffect {
    /// Probes that spend the thing they measure must not be polled casually.
    pub fn is_costly(self) -> bool {
        matches!(self, SideEffect::QuotaConsuming | SideEffect::Chargeable)
    }
}

/// Why a probe produced no reading.
///
/// A bare exit code cannot distinguish these, and the difference matters: on
/// 2026-08-26 a forum round died on `402 Grok Build usage balance exhausted`,
/// which is a *quota-denied observation* — the single most informative thing
/// the observatory could have reported that day — and it was indistinguishable
/// from a crash. Hence: probes emit a structured envelope on failure too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    InvalidCredentials,
    ProviderOutage,
    MalformedResponse,
    NetworkFailure,
    /// The provider refused because the account is out of allowance. This is a
    /// *reading about scarcity*, not an error to be swallowed.
    QuotaDenied,
    /// Core declined to run a costly probe this soon. Not a fault.
    SkippedByCadence,
    Unknown,
}

impl FailureKind {
    /// `SkippedByCadence` is bookkeeping; everything else is worth surfacing.
    pub fn is_fault(self) -> bool {
        !matches!(self, FailureKind::SkippedByCadence)
    }
}

/// A hint about which axes *could* apply. It deliberately owns no fields —
/// that is the whole point of the compositional design. Core uses it only to
/// tell "this resource has no limit concept" (inapplicable) apart from "this
/// resource should have one but the probe could not read it" (not assessable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KindHint {
    /// Allowance replenishes wholly at a fixed instant.
    ResetWindow,
    /// Allowance recovers gradually (token buckets).
    RollingRecovery,
    /// Meter runs continuously; a "reset" closes an accounting period rather
    /// than refilling anything.
    Continuous,
    /// Consumption is observable but no capacity is known. Grok is this.
    Consumption,
    /// Semantics cannot be declared without guessing. Retained and rendered,
    /// never assessed, never coloured. Reserved as a genuine escape hatch —
    /// not a bucket for everything lacking a `remaining`.
    Opaque,
}

impl KindHint {
    /// True when the vendor is understood to impose a ceiling, so missing
    /// facets mean "could not read" rather than "does not exist".
    pub fn implies_capacity(self) -> bool {
        matches!(
            self,
            KindHint::ResetWindow | KindHint::RollingRecovery | KindHint::Continuous
        )
    }
}

/// A quantity with its unit spelled out. Units are never assumed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    pub value: f64,
    /// e.g. `tokens`, `requests`, `messages`, `GBP`, `USD`.
    pub unit: String,
}

impl Measure {
    pub fn new(value: f64, unit: impl Into<String>) -> Self {
        Self {
            value,
            unit: unit.into(),
        }
    }
}

/// Monetary consequences, when the provider exposes them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monetary {
    pub currency: String,
    /// Spend already incurred in the current period, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    /// Ceiling on spend, if the account declares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap: Option<f64>,
}

/// Every facet is optional. An assessment runs only when the facets it needs
/// are present; otherwise it reports why not, rather than defaulting to green.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Facets {
    /// Fraction of capacity consumed, 0.0..=1.0, when the vendor reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed: Option<Measure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<Measure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<Measure>,
    /// Unix seconds at which the window turns over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// Nominal window length in seconds; lets core scale opportunity lead time
    /// to the window rather than hard-coding hours.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_secs: Option<i64>,
    /// **The perishability switch.** `Some(true)`: unused allowance is lost at
    /// the reset, so a surplus near the reset is a genuine "might as well".
    /// `Some(false)`: nothing perishes — a monetary meter resets its accounting
    /// period but spending faster is never an opportunity. `None`: unknown.
    ///
    /// Without this, a reset alone would be read as perishable, and the
    /// observatory would cheerfully advise you to burn credit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_unused: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monetary: Option<Monetary>,
}

/// One measurable thing within a provider account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    /// Stable within a provider across observations — the join key for history.
    pub id: String,
    pub label: String,
    pub kind_hint: KindHint,
    #[serde(default)]
    pub facets: Facets,
    /// The vendor's own words for this resource's state, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_status: Option<String>,
    /// The vendor nominated this resource as representative of the account.
    /// A **display hint only** — core still renders every resource and refuses
    /// to collapse to one number, or a vendor could hide a threatened weekly
    /// limit behind a permissive five-hour claim.
    #[serde(default)]
    pub vendor_representative: bool,
}

/// What running the probe actually cost, when measurable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationCost {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Success or structured failure. Both are legitimate observations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    Ok {
        side_effect: SideEffect,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<ObservationCost>,
        resources: Vec<Resource>,
        /// Lossless vendor payload. Never parsed by core.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    Failure {
        kind: FailureKind,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeInfo {
    /// Bare probe name, e.g. `claude` for `usage-probe-claude`.
    pub name: String,
    pub version: String,
}

/// Exactly what a probe writes to stdout: one of these, as a single JSON object.
///
/// Note what is *absent*: machine id, sequence, ingest time. Those are stamped
/// by core at write. A legal adapter is a shell script that prints JSON, and a
/// shell script cannot maintain a monotonic counter — so requiring one would
/// quietly make shell adapters illegal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub schema_version: u32,
    pub probe: ProbeInfo,
    /// Vendor identity, e.g. `anthropic`, `openai`, `xai`.
    pub provider: String,
    /// Which assistant draws on this account, when meaningful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant: Option<String>,
    /// Stable pseudonym for the account. Never a secret, never an email.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The probe's clock. **Advisory** — clocks skew across machines, so
    /// ordering uses the core-stamped sequence instead.
    pub observed_at: String,
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl Observation {
    pub fn ok(
        probe: &str,
        version: &str,
        provider: &str,
        side_effect: SideEffect,
        resources: Vec<Resource>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            probe: ProbeInfo {
                name: probe.to_string(),
                version: version.to_string(),
            },
            provider: provider.to_string(),
            assistant: None,
            account: None,
            observed_at: now_rfc3339(),
            outcome: Outcome::Ok {
                side_effect,
                cost: None,
                resources,
                raw: None,
            },
        }
    }

    pub fn failure(probe: &str, version: &str, provider: &str, kind: FailureKind, msg: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            probe: ProbeInfo {
                name: probe.to_string(),
                version: version.to_string(),
            },
            provider: provider.to_string(),
            assistant: None,
            account: None,
            observed_at: now_rfc3339(),
            outcome: Outcome::Failure {
                kind,
                message: msg.into(),
                raw: None,
            },
        }
    }

    pub fn resources(&self) -> &[Resource] {
        match &self.outcome {
            Outcome::Ok { resources, .. } => resources,
            Outcome::Failure { .. } => &[],
        }
    }

    pub fn side_effect(&self) -> Option<SideEffect> {
        match &self.outcome {
            Outcome::Ok { side_effect, .. } => Some(*side_effect),
            Outcome::Failure { .. } => None,
        }
    }
}

/// A stored row: the probe's envelope plus the fields core owns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredObservation {
    #[serde(flatten)]
    pub observation: Observation,
    /// Resolved locally. Never `unknown-machine`: failure to resolve quarantines
    /// the write rather than pooling unattributable rows into a shared bucket.
    pub machine_id: String,
    /// Monotonic per machine. The only sound ordering across a synced tree.
    pub sequence: u64,
    pub ingested_at: String,
    pub ingested_at_unix: i64,
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overage_row() -> Resource {
        // The row that killed the tagged union: a reset AND spend semantics.
        Resource {
            id: "unified-overage".into(),
            label: "Overage (credit spend)".into(),
            kind_hint: KindHint::Continuous,
            facets: Facets {
                utilization: Some(0.0),
                resets_at: Some(1788220800),
                expires_unused: Some(false),
                monetary: Some(Monetary {
                    currency: "USD".into(),
                    spent: None,
                    cap: None,
                }),
                ..Default::default()
            },
            vendor_status: Some("allowed".into()),
            vendor_representative: false,
        }
    }

    #[test]
    fn hybrid_resource_keeps_both_reset_and_monetary() {
        let r = overage_row();
        assert!(r.facets.resets_at.is_some(), "reset must survive");
        assert!(r.facets.monetary.is_some(), "monetary must survive");
        assert_eq!(r.facets.expires_unused, Some(false));
    }

    #[test]
    fn roundtrips_without_loss() {
        let obs = Observation::ok(
            "claude",
            "0.1.0",
            "anthropic",
            SideEffect::QuotaConsuming,
            vec![overage_row()],
        );
        let json = serde_json::to_string(&obs).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        assert_eq!(obs, back);
    }

    #[test]
    fn failure_envelope_is_an_observation_not_an_error() {
        let obs = Observation::failure(
            "grok",
            "0.1.0",
            "xai",
            FailureKind::QuotaDenied,
            "402 Payment Required: Grok Build usage balance exhausted",
        );
        let json = serde_json::to_string(&obs).unwrap();
        assert!(json.contains("\"outcome\":\"failure\""));
        assert!(json.contains("quota-denied"));
        let back: Observation = serde_json::from_str(&json).unwrap();
        assert_eq!(obs, back);
    }

    #[test]
    fn absent_facets_serialise_as_absent_not_null() {
        let r = Resource {
            id: "grok-weekly".into(),
            label: "Weekly pool".into(),
            kind_hint: KindHint::Consumption,
            facets: Facets {
                consumed: Some(Measure::new(208_100_000.0, "tokens")),
                ..Default::default()
            },
            vendor_status: None,
            vendor_representative: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        // Absence must be absence. A null `utilization` invites a reader to
        // coerce it to zero, which is exactly how "cannot answer" becomes "fine".
        assert!(!json.contains("utilization"), "got: {json}");
        assert!(!json.contains("remaining"), "got: {json}");
        assert!(json.contains("consumed"));
    }
}
