//! Continuum Resource Observatory — core.
//!
//! Vendor-neutral usage, limit and opportunity monitoring across providers,
//! accounts and machines. Decided on design-forum thread
//! `continuum-resource-observatory-v1` (2026-08-27).
//!
//! Core links no vendor code and holds no vendor credential. Providers are
//! standalone `usage-probe-*` executables on `PATH` that emit one JSON envelope
//! on stdout — including on failure. Adding a provider is dropping a file on
//! `PATH`; a shell script is a legal adapter.

pub mod discover;
pub mod envelope;
pub mod notify;
pub mod policy;
pub mod render;
pub mod store;

pub use envelope::{
    Facets, FailureKind, KindHint, Measure, Monetary, Observation, ObservationCost, Outcome,
    ProbeInfo, Resource, SideEffect, StoredObservation, WorkUnit, SCHEMA_VERSION,
};
pub use policy::{assess, Assessment, AxisState, Policy, POLICY_VERSION};
pub use store::Store;
