//! `usage-probe-grok` — xAI / Grok Build probe.
//!
//! **The case the whole design exists for.** Grok Build draws on a weekly
//! SuperGrok pool whose remaining balance is not exposed anywhere on disk; the
//! TUI's `/usage` is the only surface. What *is* on disk is consumption:
//! `turn_completed.usage` records in `~/.grok/sessions/**/updates.jsonl`.
//!
//! So this probe reports `consumed` and nothing else. Core must then render the
//! consumption while reporting scarcity as **not assessable** — not
//! "inapplicable", and emphatically not "healthy".
//!
//! That distinction is not pedantry. On 2026-08-26 this account returned
//! `402 Payment Required: Grok Build usage balance exhausted` mid-forum-round.
//! A ceiling demonstrably exists. A schema that rendered this provider green
//! because it could not measure the ceiling would be lying at precisely the
//! moment the user most needed the truth.

use std::path::PathBuf;
use std::process::ExitCode;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Measure, Observation, Outcome, Resource, SideEffect,
};

const PROBE: &str = "grok";
const PROVIDER: &str = "xai";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let obs = probe();
    println!("{}", serde_json::to_string(&obs).expect("envelope serialises"));
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn fail(kind: FailureKind, msg: impl Into<String>) -> Observation {
    Observation::failure(PROBE, VERSION, PROVIDER, kind, msg)
}

#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cached: u64,
    turns: u64,
    sessions: u64,
}

/// Walk `~/.grok/sessions/**/updates.jsonl`, summing completed turns since
/// `since_unix`.
fn scan(root: &PathBuf, since_unix: i64) -> Totals {
    let mut t = Totals::default();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) != Some("updates.jsonl") {
                continue;
            }
            // Cheap pre-filter: a file untouched since the cutoff holds nothing new.
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH) {
                        if (age.as_secs() as i64) < since_unix {
                            continue;
                        }
                    }
                }
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut counted_session = false;
            for line in text.lines() {
                if !line.contains("turn_completed") {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let Some(usage) = v.pointer("/turn_completed/usage") else {
                    continue;
                };
                t.turns += 1;
                if !counted_session {
                    t.sessions += 1;
                    counted_session = true;
                }
                let n = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                t.input += n("input_tokens");
                t.output += n("output_tokens");
                t.cached += n("cached_read_tokens") + n("cache_read_input_tokens");
            }
        }
    }
    t
}

fn probe() -> Observation {
    let Ok(home) = std::env::var("HOME") else {
        return fail(FailureKind::Unknown, "HOME is not set");
    };
    let root = PathBuf::from(&home).join(".grok/sessions");
    if !root.exists() {
        return fail(
            FailureKind::Unknown,
            format!("{} does not exist; Grok Build is not installed here", root.display()),
        );
    }

    // ISO weeks are a *proxy* for the vendor's reset, which may fall on another
    // weekday. Labelled as such rather than presented as the real window.
    let now = chrono::Utc::now();
    let week_start = now
        .date_naive()
        .week(chrono::Weekday::Mon)
        .first_day()
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp())
        .unwrap_or(0);

    let t = scan(&root, week_start);

    let resource = Resource {
        id: "grok-build-week".to_string(),
        label: "Grok Build (ISO week)".to_string(),
        // Consumption is observable; capacity is not. The hint is what tells
        // core to say "cannot measure" instead of inventing a percentage.
        kind_hint: KindHint::Consumption,
        facets: Facets {
            consumed: Some(Measure::new((t.input + t.output) as f64, "tokens")),
            // remaining, limit, utilization: deliberately absent. The vendor
            // exposes no ceiling locally, so we assert none.
            //
            // expires_unused stays None rather than false: the weekly pool
            // almost certainly does perish, we simply cannot see it. `false`
            // would be a claim we have not earned.
            ..Default::default()
        },
        vendor_status: None,
        vendor_representative: false,
    };

    let mut obs = Observation::ok(
        PROBE,
        VERSION,
        PROVIDER,
        // Reads local files only. Free, so it may heartbeat.
        SideEffect::Passive,
        vec![resource],
    );
    obs.assistant = Some("grok-build".to_string());
    if let Outcome::Ok { raw, .. } = &mut obs.outcome {
        *raw = Some(serde_json::json!({
            "window": "iso-week-proxy",
            "window_start_unix": week_start,
            "note": "ISO week is a proxy; the SuperGrok pool may reset on another weekday. \
                     Remaining balance is not exposed locally — read /usage in the TUI.",
            "turns": t.turns,
            "sessions": t.sessions,
            "input_tokens": t.input,
            "output_tokens": t.output,
            "cached_read_tokens": t.cached,
        }));
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuum_usage_core::policy::{assess, AxisState, Policy};

    #[test]
    fn consumption_only_resource_is_never_healthy() {
        let r = Resource {
            id: "grok-build-week".into(),
            label: "Grok".into(),
            kind_hint: KindHint::Consumption,
            facets: Facets {
                consumed: Some(Measure::new(1_000.0, "tokens")),
                ..Default::default()
            },
            vendor_status: None,
            vendor_representative: false,
        };
        let a = assess(&r, &Policy::default(), 1_000_000, 0);
        assert_eq!(a.scarcity, AxisState::NotAssessable);
        // The 402 of 2026-08-26 proves a ceiling exists; claiming the axis does
        // not apply would be the lie this design is built to prevent.
        assert_ne!(a.scarcity, AxisState::Inapplicable);
        assert_ne!(a.scarcity, AxisState::Healthy);
    }
}
