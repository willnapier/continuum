//! `usagewatch` — the Continuum Resource Observatory command line.
//!
//! Reporting and notification across every paid subscription and API account,
//! flagging both scarcity ("approaching a limit") and perishability
//! ("might as well use it before it resets").
//!
//! Core knows no vendor. Providers are `usage-probe-*` executables on `PATH`.

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;

use continuum_usage_core::{
    discover::{self, Probe},
    envelope::{FailureKind, Outcome},
    policy::Policy,
    render,
    store::Store,
};

#[derive(Parser)]
#[command(
    name = "usagewatch",
    about = "Cross-provider usage, limit and opportunity monitoring",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the latest reading for every provider (default).
    Status,
    /// Run probes and store fresh observations.
    Refresh {
        /// Only this probe, by bare name (e.g. `claude`).
        #[arg(long)]
        probe: Option<String>,
        /// Respect the cadence gate instead of forcing a run. Use for timers:
        /// it stops a quota-consuming probe being driven like a file reader.
        #[arg(long)]
        scheduled: bool,
    },
    /// Only what is worth interrupting for.
    Alerts,
    /// List the probes discovered on PATH.
    Probes,
    /// Print the newest stored envelope for a probe, verbatim.
    Raw {
        #[arg(long)]
        probe: String,
    },
    /// Store health: conflict files skipped, quarantined rows, parse failures.
    Doctor,
    /// One-off v1 migration: quarantine `unknown-machine.jsonl`.
    Migrate,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let store = Store::open_default()?;
    let policy = Policy::default();
    let now = chrono::Utc::now().timestamp();

    match cli.command.unwrap_or(Command::Status) {
        Command::Status => {
            // Readings, not cadence markers — a skip means the existing reading
            // is still fresh, so blanking the view would be backwards.
            let latest: Vec<_> = store.latest_reading_per_probe()?.into_values().collect();
            print!("{}", render::status(&latest, &policy, now));
        }

        Command::Alerts => {
            let latest: Vec<_> = store.latest_reading_per_probe()?.into_values().collect();
            let alerts = render::alerts(&latest, &policy, now);
            if alerts.is_empty() {
                println!("Nothing to flag.");
            }
            for line in alerts {
                println!("{line}");
            }
        }

        Command::Probes => {
            let probes = discover::discover();
            if probes.is_empty() {
                println!(
                    "No probes found. Core discovers executables named `{}*` on PATH.",
                    discover::PROBE_PREFIX
                );
            }
            let meta = store.probe_meta();
            for p in probes {
                let m = meta.get(&p.name);
                println!(
                    "{:<10} {:<45} cost={:?}",
                    p.name,
                    p.path.display(),
                    m.and_then(|m| m.last_side_effect)
                );
            }
        }

        Command::Refresh { probe, scheduled } => {
            let probes: Vec<Probe> = discover::discover()
                .into_iter()
                .filter(|p| probe.as_ref().map(|want| &p.name == want).unwrap_or(true))
                .collect();

            if probes.is_empty() {
                println!("No matching probes on PATH.");
                return Ok(());
            }

            let meta = store.probe_meta();
            let previous = store.latest_per_probe()?;
            for p in &probes {
                // An explicit refresh is the user asking; a scheduled run is not.
                let explicit = !scheduled;
                let (observation, actually_ran) =
                    match discover::may_run(meta.get(&p.name), &policy, now, explicit) {
                        Ok(()) => (discover::run(p), true),
                        Err(wait) => (discover::skipped(p, wait), false),
                    };

                // Capture before `append` takes ownership.
                let side_effect = observation.side_effect();
                let summary = match &observation.outcome {
                    Outcome::Ok { resources, .. } => {
                        format!("ok, {} resource(s)", resources.len())
                    }
                    Outcome::Failure { kind, message, .. } => {
                        format!("{kind:?}: {}", message.chars().take(100).collect::<String>())
                    }
                };

                // Record at most one consecutive skip per probe. A timer firing
                // more often than a probe's interval would otherwise bury real
                // history under thousands of identical cadence markers, while
                // still leaving the gap explained by the first one.
                let last_was_skip = previous
                    .get(&p.name)
                    .map(|row| {
                        matches!(
                            &row.observation.outcome,
                            Outcome::Failure { kind: FailureKind::SkippedByCadence, .. }
                        )
                    })
                    .unwrap_or(false);

                if !actually_ran && last_was_skip {
                    println!("{:<10} {summary} (not re-recorded)", p.name);
                } else {
                    match store.append(observation) {
                        Ok(_) => println!("{:<10} {summary}", p.name),
                        Err(e) => println!("{:<10} NOT STORED — {e}", p.name),
                    }
                }

                // Only a real run advances the clock. Stamping it on a skip
                // would push the next allowed run forward on every poll, so a
                // timer firing faster than the interval would starve a costly
                // probe permanently — it could never become due.
                if actually_ran {
                    store.record_probe_run(&p.name, side_effect)?;
                }
            }
        }

        Command::Raw { probe } => match store.latest_reading_per_probe()?.get(&probe) {
            Some(row) => println!("{}", serde_json::to_string_pretty(row)?),
            None => println!("No stored observation for probe `{probe}`."),
        },

        Command::Doctor => {
            let report = store.read_all()?;
            println!("store:              {}", store.root().display());
            println!("observations:       {}", report.rows.len());
            println!("malformed rows:     {}", report.malformed);
            println!("quarantined rows:   {}", report.quarantined);
            println!(
                "sync-conflict files skipped: {}",
                report.sync_conflicts_skipped.len()
            );
            for name in &report.sync_conflicts_skipped {
                println!("  ! {name}");
            }
            match Store::machine_id() {
                Some(m) => println!("machine id:         {m}"),
                None => println!("machine id:         UNRESOLVED — writes will be quarantined"),
            }
            println!("probes on PATH:     {}", discover::discover().len());
        }

        Command::Migrate => {
            let moved = store.quarantine_v1_unknown_machine()?;
            println!(
                "quarantined {moved} unattributable v1 row(s) from unknown-machine.jsonl\n\
                 (excluded from aggregation by design — a short history you can attribute\n\
                  beats a longer one you cannot)"
            );
        }
    }

    Ok(())
}
