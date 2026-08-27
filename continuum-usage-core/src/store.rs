//! The observation store: one append-only JSONL file per machine, under a
//! Syncthing-replicated tree.
//!
//! Three rules make that safe, all decided on the forum thread:
//!
//! 1. **One writer per file, path-namespaced by machine.** No shared append log
//!    ever. Readers merge at read time.
//! 2. **`*.sync-conflict-*` files are quarantined, not globbed.** A conflict
//!    file silently matched by the reader duplicates observations and skews
//!    every rate derivation, so exclusion is explicit and counted.
//! 3. **Machine identity cannot fail open.** The v1 tree already contains
//!    `unknown-machine.jsonl` next to the real hosts — an unbounded set of
//!    machines whose observations are interleaved, from which any per-machine
//!    rate is wrong in a way that looks fine. Failure to resolve now quarantines.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use color_eyre::eyre::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::envelope::{Observation, SideEffect, StoredObservation};

/// Rows whose machine could not be resolved land here and are excluded from
/// every aggregation. Never merged back.
pub const QUARANTINE_DIR: &str = "quarantine";

pub struct Store {
    root: PathBuf,
}

/// What core remembers about a probe between runs, so cadence can be enforced
/// before paying the cost of running it again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeMeta {
    pub last_run_unix: Option<i64>,
    /// Learned from the last successful envelope. Unknown on first sight, which
    /// is why the first run of any probe is always permitted.
    pub last_side_effect: Option<SideEffect>,
}

#[derive(Debug, Clone, Default)]
pub struct ReadReport {
    pub rows: Vec<StoredObservation>,
    /// Conflict files skipped, by filename. Surfaced rather than ignored.
    pub sync_conflicts_skipped: Vec<String>,
    /// Rows that would not parse.
    pub malformed: usize,
    /// Rows sitting in quarantine, excluded from `rows`.
    pub quarantined: usize,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default location: alongside the v1 tree, not on top of it. The v1 files
    /// stay readable for migration and are never written again.
    pub fn default_root() -> Result<PathBuf> {
        // Override exists so the whole pipeline can be exercised end to end
        // against a scratch tree, without polluting real history.
        if let Ok(v) = std::env::var("USAGEWATCH_STORE") {
            if !v.trim().is_empty() {
                return Ok(PathBuf::from(v));
            }
        }
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join("Assistants/continuum-usage/v2"))
    }

    pub fn open_default() -> Result<Self> {
        Ok(Self::new(Self::default_root()?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn observations_dir(&self) -> PathBuf {
        self.root.join("observations")
    }

    fn quarantine_dir(&self) -> PathBuf {
        self.root.join(QUARANTINE_DIR)
    }

    /// Exposed so notification dedup state lives beside the rest of the store.
    pub fn state_dir_public(&self) -> PathBuf {
        self.state_dir()
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn latest_dir(&self) -> PathBuf {
        self.root.join("latest")
    }

    fn ensure_dirs(&self) -> Result<()> {
        for d in [
            self.observations_dir(),
            self.quarantine_dir(),
            self.state_dir(),
            self.latest_dir(),
        ] {
            fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    // -- machine identity ----------------------------------------------------

    /// Resolve this host's identity. Returns `None` rather than a placeholder:
    /// an identity that can fail open is not an identity.
    pub fn machine_id() -> Option<String> {
        if let Ok(v) = std::env::var("CONTINUUM_MACHINE_ID") {
            if !v.trim().is_empty() {
                return Some(sanitize(v.trim()));
            }
        }
        if let Ok(v) = fs::read_to_string("/etc/hostname") {
            if !v.trim().is_empty() {
                return Some(sanitize(v.trim()));
            }
        }
        let out = std::process::Command::new("hostname").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let v = String::from_utf8(out.stdout).ok()?;
        let v = v.trim();
        if v.is_empty() {
            None
        } else {
            Some(sanitize(v))
        }
    }

    // -- sequence ------------------------------------------------------------

    /// Monotonic per machine. Ordering *within* a machine is sound; ordering
    /// across machines is not, because clocks skew — hence a sequence rather
    /// than a timestamp.
    fn next_sequence(&self, machine: &str) -> Result<u64> {
        let path = self.state_dir().join(format!("sequence-{machine}"));
        let current: u64 = fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        write_atomic(&path, next.to_string().as_bytes())?;
        Ok(next)
    }

    // -- writing -------------------------------------------------------------

    /// Append one observation. Quarantines rather than writing when the machine
    /// cannot be identified.
    pub fn append(&self, observation: Observation) -> Result<StoredObservation> {
        self.ensure_dirs()?;
        let now = chrono::Utc::now();

        let machine = match Self::machine_id() {
            Some(m) => m,
            None => {
                self.quarantine("unresolved-machine", &observation)?;
                bail!(
                    "machine identity could not be resolved; observation quarantined under {}",
                    self.quarantine_dir().display()
                );
            }
        };

        let stored = StoredObservation {
            observation,
            machine_id: machine.clone(),
            sequence: self.next_sequence(&machine)?,
            ingested_at: now.to_rfc3339(),
            ingested_at_unix: now.timestamp(),
        };

        let line = serde_json::to_string(&stored)?;
        let path = self.observations_dir().join(format!("{machine}.jsonl"));
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("appending to {}", path.display()))?;
        writeln!(f, "{line}")?;

        // Convenience pointer for cheap reads; the JSONL remains authoritative.
        let latest = self
            .latest_dir()
            .join(format!("{}-{}.json", stored.observation.probe.name, machine));
        write_atomic(&latest, serde_json::to_string_pretty(&stored)?.as_bytes())?;

        Ok(stored)
    }

    fn quarantine(&self, reason: &str, observation: &Observation) -> Result<()> {
        self.ensure_dirs()?;
        let path = self.quarantine_dir().join(format!("{reason}.jsonl"));
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{}", serde_json::to_string(observation)?)?;
        Ok(())
    }

    // -- reading -------------------------------------------------------------

    /// Merge every machine's log at read time, reporting what was skipped.
    pub fn read_all(&self) -> Result<ReadReport> {
        let mut report = ReadReport::default();
        let dir = self.observations_dir();
        if !dir.exists() {
            return Ok(report);
        }
        let mut entries: Vec<_> = fs::read_dir(&dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            // Rule 2: never glob a conflict file into the merge.
            if name.contains(".sync-conflict-") {
                report.sync_conflicts_skipped.push(name);
                continue;
            }
            if !name.ends_with(".jsonl") {
                continue;
            }
            let file = File::open(entry.path())?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<StoredObservation>(&line) {
                    Ok(row) => report.rows.push(row),
                    Err(_) => report.malformed += 1,
                }
            }
        }

        if let Ok(q) = fs::read_dir(self.quarantine_dir()) {
            for entry in q.filter_map(|e| e.ok()) {
                if let Ok(text) = fs::read_to_string(entry.path()) {
                    report.quarantined += text.lines().filter(|l| !l.trim().is_empty()).count();
                }
            }
        }

        Ok(report)
    }

    /// The newest row per probe, across machines.
    ///
    /// Ordered by `(ingested_at_unix, sequence)`. The sequence tie-break is not
    /// decoration: two writes commonly land in the same second, and comparing
    /// timestamps alone silently keeps whichever was read first. Within a
    /// machine the sequence is monotonic, which is the only sound ordering we
    /// have — probe clocks are advisory and machine clocks skew.
    pub fn latest_per_probe(&self) -> Result<BTreeMap<String, StoredObservation>> {
        Ok(newest_by_probe(self.read_all()?.rows.into_iter()))
    }

    /// The newest *reading* per probe — cadence markers excluded.
    ///
    /// A skip is bookkeeping, not a measurement. Letting it displace the last
    /// real reading would mean any scheduled run blanked the status view, which
    /// is precisely backwards: the reason we skipped is that the existing
    /// reading is still fresh enough to use.
    pub fn latest_reading_per_probe(&self) -> Result<BTreeMap<String, StoredObservation>> {
        Ok(newest_by_probe(self.read_all()?.rows.into_iter().filter(|row| {
            !matches!(
                &row.observation.outcome,
                crate::envelope::Outcome::Failure {
                    kind: crate::envelope::FailureKind::SkippedByCadence,
                    ..
                }
            )
        })))
    }

    // -- probe metadata ------------------------------------------------------

    fn meta_path(&self) -> PathBuf {
        self.state_dir().join("probe-meta.json")
    }

    pub fn probe_meta(&self) -> BTreeMap<String, ProbeMeta> {
        fs::read_to_string(self.meta_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn record_probe_run(&self, probe: &str, side_effect: Option<SideEffect>) -> Result<()> {
        self.ensure_dirs()?;
        let mut all = self.probe_meta();
        let entry = all.entry(probe.to_string()).or_default();
        entry.last_run_unix = Some(chrono::Utc::now().timestamp());
        if side_effect.is_some() {
            entry.last_side_effect = side_effect;
        }
        write_atomic(&self.meta_path(), serde_json::to_string_pretty(&all)?.as_bytes())
    }

    // -- migration -----------------------------------------------------------

    /// Move v1's `unknown-machine.jsonl` into quarantine.
    ///
    /// Quarantined, not merged: a short history you can attribute beats a longer
    /// one you cannot. William accepted that data loss explicitly.
    pub fn quarantine_v1_unknown_machine(&self) -> Result<usize> {
        self.ensure_dirs()?;
        let home = std::env::var("HOME").context("HOME is not set")?;
        let v1 = PathBuf::from(&home).join("Assistants/continuum-usage/observations/unknown-machine.jsonl");
        if !v1.exists() {
            return Ok(0);
        }
        let text = fs::read_to_string(&v1)?;
        let count = text.lines().filter(|l| !l.trim().is_empty()).count();
        let dest = self.quarantine_dir().join("v1-unknown-machine.jsonl");
        fs::write(&dest, &text)?;
        fs::remove_file(&v1)?;
        Ok(count)
    }
}

/// Keep the newest row per probe, ordered by ingest time then sequence.
fn newest_by_probe(
    rows: impl Iterator<Item = StoredObservation>,
) -> BTreeMap<String, StoredObservation> {
    let mut out: BTreeMap<String, StoredObservation> = BTreeMap::new();
    for row in rows {
        let key = row.observation.probe.name.clone();
        let newer = match out.get(&key) {
            None => true,
            Some(existing) => {
                (row.ingested_at_unix, row.sequence) > (existing.ingested_at_unix, existing.sequence)
            }
        };
        if newer {
            out.insert(key, row);
        }
    }
    out
}

fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

/// Write via a temp file in the same directory, then rename. A rename within
/// one directory is atomic; a cross-filesystem move is a copy-then-unlink and
/// is not.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}",
        std::process::id()
    ));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{FailureKind, Observation, SideEffect};

    fn tmp_root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("usage-store-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn append_then_read_roundtrips() {
        let root = tmp_root("roundtrip");
        let store = Store::new(&root);
        let obs = Observation::ok("codex", "0.1.0", "openai", SideEffect::Passive, vec![]);
        store.append(obs).expect("append");
        let report = store.read_all().expect("read");
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.malformed, 0);
        assert!(report.rows[0].sequence >= 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sequence_is_monotonic() {
        let root = tmp_root("sequence");
        let store = Store::new(&root);
        let mut seen = vec![];
        for _ in 0..3 {
            let obs = Observation::ok("codex", "0.1.0", "openai", SideEffect::Passive, vec![]);
            seen.push(store.append(obs).unwrap().sequence);
        }
        assert_eq!(seen, vec![seen[0], seen[0] + 1, seen[0] + 2]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_conflict_files_are_skipped_and_counted() {
        let root = tmp_root("conflict");
        let store = Store::new(&root);
        store
            .append(Observation::ok("codex", "0.1.0", "openai", SideEffect::Passive, vec![]))
            .unwrap();

        // Simulate Syncthing dropping a conflict copy beside the real log.
        let dir = root.join("observations");
        let real = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .unwrap()
            .path();
        let conflict = dir.join("nimbini.sync-conflict-20260827-120000-ABCDEFG.jsonl");
        fs::copy(&real, &conflict).unwrap();

        let report = store.read_all().unwrap();
        assert_eq!(report.rows.len(), 1, "conflict copy must not be merged");
        assert_eq!(report.sync_conflicts_skipped.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failure_envelopes_are_stored_like_any_other_observation() {
        let root = tmp_root("failure");
        let store = Store::new(&root);
        store
            .append(Observation::failure(
                "grok",
                "0.1.0",
                "xai",
                FailureKind::QuotaDenied,
                "402 balance exhausted",
            ))
            .unwrap();
        let report = store.read_all().unwrap();
        assert_eq!(report.rows.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cadence_skip_does_not_displace_the_last_real_reading() {
        let root = tmp_root("skipdisplace");
        let store = Store::new(&root);
        store
            .append(Observation::ok("claude", "0.1.0", "anthropic", SideEffect::QuotaConsuming, vec![]))
            .unwrap();
        store
            .append(Observation::failure(
                "claude",
                "core",
                "anthropic",
                FailureKind::SkippedByCadence,
                "too soon",
            ))
            .unwrap();

        // The raw view sees the marker...
        let raw = store.latest_per_probe().unwrap();
        assert!(matches!(
            raw["claude"].observation.outcome,
            crate::envelope::Outcome::Failure { .. }
        ));
        // ...but status must still show the measurement.
        let reading = store.latest_reading_per_probe().unwrap();
        assert!(matches!(
            reading["claude"].observation.outcome,
            crate::envelope::Outcome::Ok { .. }
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_meta_remembers_side_effect() {
        let root = tmp_root("meta");
        let store = Store::new(&root);
        store.record_probe_run("claude", Some(SideEffect::QuotaConsuming)).unwrap();
        let meta = store.probe_meta();
        assert_eq!(
            meta.get("claude").unwrap().last_side_effect,
            Some(SideEffect::QuotaConsuming)
        );
        let _ = fs::remove_dir_all(&root);
    }
}
