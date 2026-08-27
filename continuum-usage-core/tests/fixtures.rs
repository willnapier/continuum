//! Acceptance: the v2 contract must hold providers with genuinely different
//! limit ontologies **without a schema bump**.
//!
//! Per the forum decision, the Anthropic DPA metered key ships as a checked-in
//! fixture rather than a live probe in v1. Its job is to be the discriminating
//! shape: monetary, rolling rather than fixed-reset, and with perishability
//! structurally absent. If a future change to the envelope breaks it, that is
//! the signal that the schema has quietly become subscription-shaped again.
//!
//! Grok-build's warning in round 2 was that four adapters all drawn from these
//! two machines can overfit the union and then call the overfitting neutrality.
//! These fixtures are the cheapest available guard against that.

use continuum_usage_core::envelope::{KindHint, Observation, Outcome, SCHEMA_VERSION};
use continuum_usage_core::policy::{assess, AxisState, Policy};

fn load(name: &str) -> Observation {
    let path = format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {path}: {e}"))
}

#[test]
fn dpa_metered_key_validates_without_a_schema_bump() {
    let obs = load("anthropic-dpa.json");
    assert_eq!(obs.schema_version, SCHEMA_VERSION);
    assert_eq!(obs.resources().len(), 3);
    assert!(matches!(obs.outcome, Outcome::Ok { .. }));
}

#[test]
fn metered_key_never_offers_a_might_as_well() {
    // The discriminating case. Rate limits mean scarcity applies; money means
    // perishability does not. Spending faster is never an opportunity.
    let obs = load("anthropic-dpa.json");
    let policy = Policy::default();
    for r in obs.resources() {
        let a = assess(r, &policy, 1_787_831_000, 0);
        assert_eq!(
            a.perishability,
            AxisState::Inapplicable,
            "resource {} must not be perishable",
            r.id
        );
        assert_ne!(a.perishability, AxisState::Opportunity);
    }
}

#[test]
fn rolling_buckets_derive_scarcity_from_remaining_over_limit() {
    let obs = load("anthropic-dpa.json");
    let policy = Policy::default();
    let tokens = obs
        .resources()
        .iter()
        .find(|r| r.id == "input-tokens-per-minute")
        .expect("token bucket present");
    let a = assess(tokens, &policy, 1_787_831_000, 0);
    // 18000 remaining of 20000 => 10% used, and no percentage was supplied.
    let u = a.utilization.expect("derived from remaining/limit");
    assert!((u - 0.1).abs() < 1e-9, "expected ~0.1, got {u}");
    assert_eq!(a.scarcity, AxisState::Healthy);
}

#[test]
fn a_pure_spend_row_reports_scarcity_as_not_assessable() {
    let obs = load("anthropic-dpa.json");
    let policy = Policy::default();
    let spend = obs
        .resources()
        .iter()
        .find(|r| r.id == "month-to-date-spend")
        .expect("spend row present");
    let a = assess(spend, &policy, 1_787_831_000, 0);
    // There is a meter but no declared ceiling: we cannot say how close it is.
    assert_eq!(a.scarcity, AxisState::NotAssessable);
    assert_ne!(a.scarcity, AxisState::Healthy);
}

#[test]
fn claude_max_carries_three_concurrent_resources() {
    // v1's `VendorUsage` could hold exactly one window plus an untyped
    // `secondary`. This is the shape that forced schema v2.
    let obs = load("claude-max.json");
    assert_eq!(obs.schema_version, SCHEMA_VERSION);
    assert_eq!(obs.resources().len(), 3);
    assert_eq!(
        obs.resources().iter().filter(|r| r.vendor_representative).count(),
        1,
        "exactly one vendor-nominated representative"
    );
}

#[test]
fn the_overage_row_keeps_both_its_reset_and_its_money() {
    // The counter-example that killed the round-1 tagged union: tagging this
    // `monetary` drops the reset, tagging it `reset-window` invites core to
    // treat credit spend as perishable surplus.
    let obs = load("claude-max.json");
    let overage = obs
        .resources()
        .iter()
        .find(|r| r.id == "unified-overage")
        .expect("overage row present");
    assert_eq!(overage.kind_hint, KindHint::Continuous);
    assert!(overage.facets.resets_at.is_some(), "reset survives");
    assert!(overage.facets.monetary.is_some(), "money survives");

    let a = assess(overage, &Policy::default(), 1_787_830_000, 0);
    assert_eq!(a.perishability, AxisState::Inapplicable);
}

#[test]
fn weekly_surplus_late_in_the_window_is_an_opportunity() {
    // The "might as well" case William asked for, on real numbers: 3% used on
    // the 7-day window with an hour to go before it resets.
    let obs = load("claude-max.json");
    let weekly = obs
        .resources()
        .iter()
        .find(|r| r.id == "unified-7d")
        .expect("weekly window present");
    let one_hour_before_reset = weekly.facets.resets_at.unwrap() - 3600;
    let a = assess(weekly, &Policy::default(), one_hour_before_reset, 0);
    assert_eq!(a.perishability, AxisState::Opportunity);
}

#[test]
fn every_fixture_round_trips_losslessly() {
    for name in ["anthropic-dpa.json", "claude-max.json"] {
        let obs = load(name);
        let json = serde_json::to_string(&obs).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        assert_eq!(obs, back, "{name} did not round-trip");
    }
}
