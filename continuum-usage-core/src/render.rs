//! Terminal rendering.
//!
//! One rule governs everything here: **absence renders as absence.** An axis
//! that does not apply prints a dash, not a tick. An axis nobody can measure
//! prints "not assessable", not "healthy". A vendor's own nomination of a
//! representative resource is shown as an annotation, never as *the* number for
//! the account — otherwise a permissive five-hour claim could mask a threatened
//! weekly one.

use crate::envelope::{FailureKind, KindHint, MeterScope, Outcome, StoredObservation};
use crate::policy::{assess_with_history, Baselines, AxisState, Policy};

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

/// How to show the reset column.
///
/// A rolling bucket's "reset" is the instant the current bucket is fully
/// recovered — which, when it is already full, is *now*. Rendering that as
/// "due" implies something is pending and reads as a warning. It is not: the
/// bucket recovers continuously whether or not you draw on it.
fn reset_cell(kind: KindHint, seconds: Option<i64>) -> String {
    match (kind, seconds) {
        (KindHint::RollingRecovery, Some(s)) if s <= 0 => "rolling".into(),
        (_, Some(s)) => human_duration(s),
        (_, None) => "—".into(),
    }
}

/// Translate a resource into terms a person actually reasons in.
///
/// Vendor accounting units are meaningless outside the vendor's own books.
/// Two things are not: **how much more work is left**, and **real money**.
///
/// Money appears only where money actually moves — a metered key, an overage
/// meter. Pricing a flat-rate subscription allowance at list rates invents a
/// figure the user never pays and implies a spend that is not happening; that
/// is worse than showing the raw vendor unit, because a currency symbol is
/// believed.
fn currency_sym(unit: &str) -> Option<&'static str> {
    match unit {
        "USD" => Some("$"),
        "GBP" => Some("£"),
        "EUR" => Some("€"),
        _ => None,
    }
}

fn human_gloss(r: &crate::envelope::Resource) -> Option<String> {
    // Remaining money is remaining money. Do not divide a wallet by one
    // host's session size and call that capacity.
    if let Some(rem) = &r.facets.remaining {
        if let Some(sym) = currency_sym(&rem.unit) {
            return Some(format!("{sym}{:.2} in wallet", rem.value));
        }
    }

    // Work first: it is the question a prepaid allowance actually answers.
    let parts: Vec<String> = r
        .facets
        .work_units
        .iter()
        .filter(|w| w.cost > 0.0)
        .filter_map(|w| match (&r.facets.remaining, &r.facets.consumed) {
            // A ceiling is known: say how much more work is left.
            (Some(rem), _) => Some(format!("≈ {} more {}s", scale(rem.value / w.cost), w.label)),
            // No ceiling: the only honest statement is what has been used. Note
            // this converts the resource's own consumption, never the sample
            // count — showing "16 tokens" because 16 sessions were observed is
            // the sort of nonsense that discredits the whole display.
            (None, Some(used)) => Some(format!("{} {}s used", scale(used.value / w.cost), w.label)),
            (None, None) => None,
        })
        .collect();
    if !parts.is_empty() {
        let n = r.facets.work_units.first().map(|w| w.observed).unwrap_or(0);
        return Some(format!("{} — at your recent mix, from {n} sessions", parts.join(" · ")));
    }
    let m = r.facets.monetary.as_ref()?;
    let sym = match m.currency.as_str() {
        "USD" => "$",
        "GBP" => "£",
        "EUR" => "€",
        other => return Some(format!("{other} {:.2} spent", m.spent?)),
    };
    match (m.spent, m.cap) {
        (Some(spent), Some(cap)) => Some(format!("{sym}{spent:.2} of {sym}{cap:.2} spent")),
        (Some(spent), None) => Some(format!("{sym}{spent:.2} spent")),
        _ => None,
    }
}

/// Note when a resource's ceiling has moved during the current window.
///
/// A limit that rises mid-period is not a neutral fact: **somebody bought
/// more.** Grok raises `monthlyLimit` by one top-up block per purchase, so the
/// ceiling is the only visible trace of a charge — the API exposes no purchase
/// history and no prepaid balance. Detecting the movement is therefore the only
/// way the observatory can see real money being spent on that account.
///
/// Vendor-neutral by construction: it compares a resource against its own
/// earlier reading and knows nothing about who raised it or why.
fn limit_change(baseline: Option<&crate::envelope::Resource>, current: &crate::envelope::Resource) -> Option<String> {
    let (was, now) = (
        baseline?.facets.limit.as_ref()?,
        current.facets.limit.as_ref()?,
    );
    if now.value <= was.value || was.unit != now.unit {
        return None;
    }
    Some(format!(
        "ceiling raised {} → {} {} this period (+{}) — something was purchased",
        scale(was.value),
        scale(now.value),
        now.unit,
        scale(now.value - was.value)
    ))
}

/// Say when a counter fell before the reset it had advertised.
///
/// A sudden 0% with days left on the clock reads, at a glance, like the week
/// has been spent — the exact opposite of what happened. Naming it as a
/// provider-side reset is the whole value; the verdicts are untouched.
fn reset_gloss(
    baselines: &Baselines,
    probe: &str,
    resource_id: &str,
    policy: &Policy,
    now_unix: i64,
) -> Option<String> {
    let r = baselines
        .reset(probe, resource_id)
        .filter(|r| r.visible(policy, now_unix))?;
    Some(format!(
        "reset early: fell from {} to {} {} ago, {} before the scheduled reset — \
         a provider-side reset or plan change, not consumption",
        pct(Some(r.from)),
        pct(Some(r.to)),
        human_duration(now_unix - r.at_unix),
        human_duration(r.early_by_secs())
    ))
}

/// Round large counts to something readable: 139000000 -> 139M.
fn scale(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.0}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.0}k", n / 1_000.0)
    } else {
        format!("{}", n.floor() as i64)
    }
}

fn pct(u: Option<f64>) -> String {
    match u {
        Some(v) => format!("{:.0}%", v * 100.0),
        None => "—".into(),
    }
}

/// Full status across every probe with a stored observation.
pub fn status(
    rows: &[StoredObservation],
    baselines: &Baselines,
    policy: &Policy,
    now_unix: i64,
) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("No observations yet. Run `usagewatch refresh`.\n");
        return out;
    }

    for row in rows {
        let obs = &row.observation;
        let age = now_unix - row.ingested_at_unix;
        let where_from = match obs.scope {
            MeterScope::Account => format!("account · probed from {}", row.machine_id),
            MeterScope::Machine => row.machine_id.clone(),
        };
        out.push_str(&format!(
            "\n{} ({})  {}  [{}]\n",
            obs.probe.name,
            obs.provider,
            obs.account.as_deref().unwrap_or("-"),
            where_from
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
                    let a = assess_with_history(&obs.probe.name, r, baselines, policy, now_unix, age);
                    let star = if r.vendor_representative { " *" } else { "" };
                    out.push_str(&format!(
                        "  {:<22} {:>6}  {:<13} {:<13} {}{}\n",
                        truncate(&r.label, 22),
                        pct(a.utilization),
                        axis_cell(a.scarcity),
                        axis_cell(a.perishability),
                        reset_cell(r.kind_hint, a.seconds_to_reset),
                        star
                    ));
                    if let Some(g) = human_gloss(r) {
                        out.push_str(&format!("  {:<22} ↳ {g}\n", ""));
                    }
                    // The vendor's own words, verbatim. For an opaque resource
                    // (a smoke test's "OK in 5.0s") they are the whole reading.
                    if let Some(s) = r.vendor_status.as_deref().filter(|s| !s.trim().is_empty()) {
                        out.push_str(&format!("  {:<22} ↳ {}\n", "", truncate(s, 100)));
                    }
                    if let Some(c) = limit_change(
                        baselines
                            .get(&(obs.probe.name.clone(), r.id.clone()))
                            .map(|(res, _)| res),
                        r,
                    ) {
                        out.push_str(&format!("  {:<22} ↳ {c}\n", ""));
                    }
                    if let Some(g) = reset_gloss(baselines, &obs.probe.name, &r.id, policy, now_unix) {
                        out.push_str(&format!("  {:<22} ↳ {g}\n", ""));
                    }
                    if let Some(p) = a.projection.filter(|p| p.exhausts_before_reset) {
                        out.push_str(&format!(
                            "  {:<22} ↳ at the current rate this runs out in {}, {} before it resets\n",
                            "",
                            human_duration(p.seconds_of_headroom),
                            human_duration(
                                a.seconds_to_reset.unwrap_or(0) - p.seconds_of_headroom
                            )
                        ));
                    }
                }
            }
        }
    }
    out.push_str("\n  * vendor-nominated representative resource (display hint only)\n");
    out.push_str("  — = axis does not apply here   ? = cannot be measured\n");
    out
}

/// Only what is worth interrupting the user for.
pub fn alerts(
    rows: &[StoredObservation],
    baselines: &Baselines,
    policy: &Policy,
    now_unix: i64,
) -> Vec<String> {
    let mut out = vec![];
    for row in rows {
        let obs = &row.observation;
        let age = now_unix - row.ingested_at_unix;

        if let Outcome::Failure { kind, message, .. } = &obs.outcome {
            // A quota-denied reading is the most informative alert there is:
            // the account is actually out, right now.
            if *kind == FailureKind::QuotaDenied {
                out.push(format!("{} ({}): EXHAUSTED — {}", obs.probe.name, obs.provider, message));
            } else if *kind == FailureKind::RequestRefused {
                // The vendor's words are the whole diagnosis; a bare kind
                // would send the reader back to `raw` for the one line that
                // matters.
                out.push(format!("{} ({}): REFUSED — {}", obs.probe.name, obs.provider, message));
            } else if kind.is_fault() {
                out.push(format!("{} ({}): probe failed — {:?}", obs.probe.name, obs.provider, kind));
            }
            continue;
        }

        for r in obs.resources() {
            let a = assess_with_history(&obs.probe.name, r, baselines, policy, now_unix, age);
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
            if let Some(p) = a.projection.filter(|p| p.exhausts_before_reset) {
                let gloss = human_gloss(r).map(|g| format!(" ({g})")).unwrap_or_default();
                out.push(format!(
                    "{} / {}: burning fast — {} used{}, runs out in {} but resets in {}",
                    obs.probe.name,
                    r.label,
                    pct(a.utilization),
                    gloss,
                    human_duration(p.seconds_of_headroom),
                    a.seconds_to_reset.map(human_duration).unwrap_or_default()
                ));
            }
            if let Some(g) = reset_gloss(baselines, &obs.probe.name, &r.id, policy, now_unix) {
                out.push(format!("{} / {}: {g}", obs.probe.name, r.label));
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
    use crate::envelope::{Facets, KindHint, MeterScope, Monetary, Observation, Resource, SideEffect};

    fn stored(obs: Observation, age: i64, now: i64) -> StoredObservation {
        StoredObservation {
            observation: obs,
            machine_id: "desk".into(),
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
    fn a_prepaid_allowance_is_glossed_as_work_not_money() {
        // Real observed Grok numbers: 3,942 credits left, 16 sessions this
        // week costing 6,523 => about 408 per session => about 9 sessions.
        let r = res(
            "grok-monthly-credits",
            KindHint::ResetWindow,
            Facets {
                remaining: Some(crate::envelope::Measure::new(3_942.0, "credits")),
                work_units: vec![crate::envelope::WorkUnit {
                    label: "session".into(),
                    cost: 6_523.0 / 16.0,
                    observed: 16,
                }],
                ..Default::default()
            },
        );
        let g = human_gloss(&r).expect("gloss");
        assert!(g.starts_with("≈ 9 more sessions"), "got {g}");
        assert!(!g.contains('$'), "a flat-rate allowance must not be priced: {g}");
        assert!(g.contains("from 16 sessions"), "sample size must be visible: {g}");
    }

    #[test]
    fn remaining_currency_is_a_wallet_not_remaining_sessions() {
        let mut r = res(
            "grok-monthly-credits",
            KindHint::Consumption,
            Facets {
                remaining: Some(crate::envelope::Measure::new(85.42, "USD")),
                work_units: vec![crate::envelope::WorkUnit {
                    label: "session".into(),
                    cost: 4.56,
                    observed: 15,
                }],
                ..Default::default()
            },
        );
        assert_eq!(human_gloss(&r).as_deref(), Some("$85.42 in wallet"));
        r.facets.work_units.clear();
        assert_eq!(human_gloss(&r).as_deref(), Some("$85.42 in wallet"));
    }

    #[test]
    fn account_scope_does_not_label_the_reading_as_the_host() {
        let mut obs = Observation::ok(
            "grok",
            "0.1.0",
            "xai",
            SideEffect::RequestConsuming,
            vec![res("grok-week", KindHint::ResetWindow, Facets::default())],
        );
        obs.account = Some("unified".into());
        obs.scope = MeterScope::Account;
        let s = status(
            &[stored(obs, 10, NOW)],
            &Baselines::new(),
            &Policy::default(),
            NOW,
        );
        assert!(s.contains("[account · probed from desk]"), "{s}");
        assert!(!s.contains("[desk]\n"), "{s}");
    }

    #[test]
    fn claude_codex_and_dpa_key_are_account_meters_not_host_readings() {
        // Same labels the three probes emit. Their numbers come from vendor
        // account APIs (Max unified headers, Codex account/rateLimits/read, DPA
        // org rate-limit headers), so the host is only where the call ran.
        let probes = [
            ("claude", "anthropic", "max"),
            ("codex", "openai", "plus"),
            ("anthropic-api", "anthropic-api", "dpa"),
        ];
        let rows: Vec<StoredObservation> = probes
            .iter()
            .map(|(probe, provider, account)| {
                let mut obs = Observation::ok(
                    probe,
                    "0.1.0",
                    provider,
                    SideEffect::RequestConsuming,
                    vec![res("window", KindHint::ResetWindow, Facets::default())],
                );
                obs.account = Some((*account).into());
                obs.scope = MeterScope::Account;
                stored(obs, 10, NOW)
            })
            .collect();
        let s = status(&rows, &Baselines::new(), &Policy::default(), NOW);
        for (probe, provider, account) in probes {
            let header = format!("{probe} ({provider})  {account}  [account · probed from desk]");
            assert!(s.contains(&header), "missing {header:?} in:\n{s}");
        }
        assert!(!s.contains("[desk]\n"), "a bare host label survived:\n{s}");
    }

    #[test]
    fn real_money_is_still_shown_where_money_actually_moves() {
        // A metered key: billed per token, so pounds are the honest unit.
        let r = res(
            "dpa-spend",
            KindHint::Continuous,
            Facets {
                monetary: Some(Monetary {
                    currency: "GBP".into(),
                    spent: Some(41.18),
                    cap: None,
                }),
                ..Default::default()
            },
        );
        assert_eq!(human_gloss(&r).as_deref(), Some("£41.18 spent"));
    }

    #[test]
    fn a_raised_ceiling_is_reported_as_a_purchase() {
        // Grok raises monthlyLimit by one 5,000-credit block per $50 top-up,
        // and that movement is the only trace of the charge the API offers.
        let mk = |limit: f64| res(
            "grok-monthly-credits",
            KindHint::ResetWindow,
            Facets {
                limit: Some(crate::envelope::Measure::new(limit, "credits")),
                ..Default::default()
            },
        );
        let note = limit_change(Some(&mk(10_500.0)), &mk(15_500.0)).expect("change noted");
        assert!(note.contains("10k"), "{note}");
        assert!(note.contains("16k") || note.contains("15k"), "{note}");
        assert!(note.contains("purchased"), "{note}");

        // Unchanged, or falling (a reset), is not a purchase.
        assert!(limit_change(Some(&mk(15_500.0)), &mk(15_500.0)).is_none());
        assert!(limit_change(Some(&mk(15_500.0)), &mk(5_500.0)).is_none());
        assert!(limit_change(None, &mk(15_500.0)).is_none());
    }

    #[test]
    fn a_ceilingless_resource_converts_consumption_not_the_sample_count() {
        // Regression: the token row once read "16 tokens this week" because it
        // printed the session count instead of converting consumption.
        let r = res(
            "grok-build-week",
            KindHint::Consumption,
            Facets {
                consumed: Some(crate::envelope::Measure::new(6_523.0, "credits")),
                work_units: vec![crate::envelope::WorkUnit {
                    label: "token".into(),
                    cost: 6_523.0 / 346_028_596.0,
                    observed: 16,
                }],
                ..Default::default()
            },
        );
        let g = human_gloss(&r).expect("gloss");
        assert!(g.starts_with("346M tokens used"), "got {g}");
    }

    #[test]
    fn a_resource_with_no_money_facet_gets_no_gloss() {
        let r = res("x", KindHint::ResetWindow, Facets::default());
        assert!(human_gloss(&r).is_none());
    }

    #[test]
    fn a_full_rolling_bucket_reads_as_rolling_not_due() {
        assert_eq!(reset_cell(KindHint::RollingRecovery, Some(0)), "rolling");
        assert_eq!(reset_cell(KindHint::RollingRecovery, Some(-5)), "rolling");
        // A reset window genuinely at its instant is a different matter.
        assert_eq!(reset_cell(KindHint::ResetWindow, Some(0)), "due");
        assert_eq!(reset_cell(KindHint::ResetWindow, Some(3600)), "1h 0m");
    }

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
        let a = alerts(&[stored(obs, 10, NOW)], &Baselines::new(), &Policy::default(), NOW);
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
        let a = alerts(&[stored(obs, 10, NOW)], &Baselines::new(), &Policy::default(), NOW);
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
        let a = alerts(&[stored(obs, 10, NOW)], &Baselines::new(), &Policy::default(), NOW);
        assert_eq!(a.len(), 1);
        assert!(a[0].contains("EXHAUSTED"), "{a:?}");
    }

    #[test]
    fn an_early_reset_is_named_in_alerts_and_status() {
        use crate::policy::baselines;
        let policy = Policy::default();
        let mk = |util: f64, resets_at: i64| {
            Observation::ok("codex", "1", "openai", SideEffect::RequestConsuming,
                vec![res("w", KindHint::ResetWindow, Facets {
                    utilization: Some(util),
                    resets_at: Some(resets_at),
                    window_secs: Some(604_800),
                    expires_unused: Some(true),
                    ..Default::default()
                })])
        };
        let rows = [
            stored(mk(0.77, NOW + 3 * 86_400), 1200, NOW),
            stored(mk(0.0, NOW + 604_800), 600, NOW),
        ];
        let base = baselines(&rows, &policy);
        let latest = [rows[1].clone()];

        let a = alerts(&latest, &base, &policy, NOW);
        assert_eq!(a.len(), 1, "{a:?}");
        assert!(a[0].contains("reset early"), "{a:?}");
        assert!(a[0].contains("77% to 0%"), "{a:?}");
        assert!(a[0].contains("not consumption"), "{a:?}");

        let s = status(&latest, &base, &policy, NOW);
        assert!(s.contains("↳ reset early"), "{s}");

        // Nothing to say once it is old news.
        assert!(alerts(&latest, &base, &policy, NOW + 3 * 86_400).is_empty());
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
        assert!(alerts(&[stored(obs, 10, NOW)], &Baselines::new(), &Policy::default(), NOW).is_empty());
    }
}
