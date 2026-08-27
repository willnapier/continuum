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
use fs2::FileExt;
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
    /// Files that could not be opened at all. Surfaced, not swallowed.
    pub unreadable_files: Vec<String>,
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
            if let Some(id) = distinctive(&v) {
                return Some(id);
            }
        }
        if let Ok(v) = fs::read_to_string("/etc/hostname") {
            // First line only: a commented or multi-line file would otherwise
            // sanitize to something stable, plausible and wrong.
            if let Some(id) = v.lines().next().and_then(distinctive) {
                return Some(id);
            }
        }
        let out = std::process::Command::new("hostname").output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout)
            .ok()
            .and_then(|v| v.lines().next().and_then(distinctive))
    }

    // -- sequence ------------------------------------------------------------

    /// Monotonic per machine. Ordering *within* a machine is sound; ordering
    /// across machines is not, because clocks skew — hence a sequence rather
    /// than a timestamp.
    ///
    /// The caller must hold the write lock: this is a read-modify-write, and
    /// two unsynchronised callers would both read N and both stamp N+1. A
    /// duplicate sequence is worse than it sounds — `newest_by_probe` compares
    /// `(ingested_at_unix, sequence)`, so equal tuples compare equal, `newer`
    /// stays false, and the *first-encountered* row wins. That is the older
    /// one, so `status` silently shows a stale reading.
    fn next_sequence(&self, machine: &str) -> Result<u64> {
        let path = self.state_dir().join(format!("sequence-{machine}"));
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let current: u64 = if raw.trim().is_empty() {
            0
        } else {
            raw.trim().parse().map_err(|_| {
                color_eyre::eyre::eyre!(
                    "sequence counter {} is corrupt ({:?}); refusing to restart numbering \
                     and silently reissue sequences that already exist in the log",
                    path.display(),
                    raw.trim().chars().take(40).collect::<String>()
                )
            })?
        };
        let next = current + 1;
        write_atomic(&path, next.to_string().as_bytes())?;
        Ok(next)
    }

    /// Take the per-machine write lock.
    ///
    /// Guards the sequence read-modify-write and the append together, so a
    /// scheduled run and a hand-run `refresh` cannot interleave. Advisory
    /// `flock`, released by the OS when the handle closes — so a killed process
    /// never leaves a stale lock, which a lockfile-with-retry would.
    fn lock(&self, machine: &str) -> Result<File> {
        let path = self.state_dir().join(format!("write-{machine}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening lock {}", path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
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

        // Held across sequence allocation AND the append.
        let _guard = self.lock(&machine)?;

        let stored = StoredObservation {
            observation,
            machine_id: machine.clone(),
            sequence: self.next_sequence(&machine)?,
            ingested_at: now.to_rfc3339(),
            ingested_at_unix: now.timestamp(),
        };

        let path = self.observations_dir().join(format!("{machine}.jsonl"));
        append_line(&path, &serde_json::to_string(&stored)?)?;
        restrict(&path);

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
        append_line(&path, &serde_json::to_string(observation)?)?;
        restrict(&path);
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
            // A file that will not open, or a line that is not UTF-8, must not
            // abort the whole read. `doctor` routes through here, so propagating
            // would kill the diagnostic on exactly the corruption it exists to
            // report — and would skip every machine whose file sorts later.
            let Ok(file) = File::open(entry.path()) else {
                report.unreadable_files.push(name);
                continue;
            };
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else {
                    report.malformed += 1;
                    continue;
                };
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

    /// Per machine, like `sequence-` and `notified-`.
    ///
    /// A shared file here is a two-writer read-whole/modify-whole under
    /// Syncthing: one machine's update silently reverts the other's, and this
    /// file gates the *chargeable* and *quota-consuming* probes, so a lost
    /// update buys extra billed calls. It also made the cadence gate global —
    /// one machine's run suppressing the other's for an hour.
    fn meta_path(&self) -> PathBuf {
        let machine = Self::machine_id().unwrap_or_else(|| "unresolved".into());
        self.state_dir().join(format!("probe-meta-{machine}.json"))
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

/// Restrict a file the store owns to owner-only.
///
/// Everything here lands in a Syncthing-replicated tree shared across machines
/// and assistants, and default 0644 makes it readable by every local account.
/// Observations are not secret by design, but a failure message can carry
/// whatever a vendor library put in it, so the store defends in depth rather
/// than trusting every probe to be careful.
#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}

/// Names that identify no particular machine.
///
/// `localhost` is non-empty, so the old code accepted it — which reinstates the
/// `unknown-machine` pooling that Rule 3 exists to abolish, under a different
/// name. Two hosts would share one file and one counter.
const NON_DISTINCTIVE: &[&str] = &["localhost", "localhost-localdomain", "unknown", "-", ""];

/// Sanitize and reject anything that does not actually identify a machine.
fn distinctive(raw: &str) -> Option<String> {
    let id = sanitize(raw.trim());
    if id.is_empty() || NON_DISTINCTIVE.contains(&id.as_str()) {
        return None;
    }
    Some(id)
}

/// Lower-cased deliberately.
///
/// The store is replicated to a case-insensitive filesystem (APFS). Two ids
/// differing only in case are two files on Linux and one on the Mac, which
/// Syncthing surfaces as a case conflict it cannot resolve — and the folder
/// stops syncing. Folding case costs a theoretical collision between hosts
/// named `Foo` and `foo`, which is not a real configuration.
fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Append one line with a single `write_all`.
///
/// `writeln!` issues the payload and the newline as two `write(2)` calls.
/// `O_APPEND` makes each call atomic but not the pair, so two concurrent
/// appenders interleave: measured at 756 intact lines out of 40,000 across two
/// processes. Building the line with its newline and issuing one write restores
/// atomicity for records below the pipe buffer, which these are (~1.3 KB).
fn append_line(path: &Path, line: &str) -> Result<()> {
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("appending to {}", path.display()))?;
    f.write_all(buf.as_bytes())
        .with_context(|| format!("writing to {}", path.display()))?;
    Ok(())
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
    restrict(&tmp);
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

    #[cfg(unix)]
    #[test]
    fn written_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp_root("perms");
        let store = Store::new(&root);
        store
            .append(Observation::ok("codex", "0.1.0", "openai", SideEffect::Passive, vec![]))
            .unwrap();
        for dir in ["observations", "latest"] {
            for e in fs::read_dir(root.join(dir)).unwrap().filter_map(|e| e.ok()) {
                let mode = e.metadata().unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{:?} is {:o}", e.path(), mode);
            }
        }
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
    fn concurrent_appends_do_not_interleave_or_collide() {
        // The defect this replaces was measured at 756 intact lines out of
        // 40,000 across two processes. Threads share the process lock, so this
        // guards the single-write_all change and the sequence lock together.
        let root = tmp_root("concurrent");
        fs::create_dir_all(root.join("state")).unwrap();
        let n = 40;
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let store = Store::new(&root);
                    for _ in 0..n {
                        store
                            .append(Observation::ok(
                                "codex", "0.1.0", "openai", SideEffect::Passive, vec![],
                            ))
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let report = store_at(&root).read_all().unwrap();
        assert_eq!(report.rows.len(), 4 * n, "every line must survive intact");
        assert_eq!(report.malformed, 0, "no interleaved or truncated lines");

        let mut seqs: Vec<u64> = report.rows.iter().map(|r| r.sequence).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 4 * n, "sequences must be unique");
        let _ = fs::remove_dir_all(&root);
    }

    fn store_at(root: &PathBuf) -> Store {
        Store::new(root)
    }

    #[test]
    fn a_corrupt_sequence_counter_is_an_error_not_a_silent_restart() {
        let root = tmp_root("corruptseq");
        let store = Store::new(&root);
        store
            .append(Observation::ok("codex", "0.1.0", "openai", SideEffect::Passive, vec![]))
            .unwrap();
        let machine = Store::machine_id().unwrap();
        fs::write(root.join("state").join(format!("sequence-{machine}")), "garbage").unwrap();
        // Restarting at 1 would reissue sequences that already exist in the log.
        assert!(store
            .append(Observation::ok("codex", "0.1.0", "openai", SideEffect::Passive, vec![]))
            .is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bad_line_does_not_abort_the_whole_read() {
        use std::io::Write as _;
        let root = tmp_root("badbyte");
        let store = Store::new(&root);
        store
            .append(Observation::ok("codex", "0.1.0", "openai", SideEffect::Passive, vec![]))
            .unwrap();
        // Invalid UTF-8 mid-file, then a further valid row.
        let machine = Store::machine_id().unwrap();
        let path = root.join("observations").join(format!("{machine}.jsonl"));
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0xff, 0xfe, b'\n']).unwrap();
        }
        store
            .append(Observation::ok("grok", "0.1.0", "xai", SideEffect::Passive, vec![]))
            .unwrap();

        let report = store.read_all().expect("read must not abort");
        assert_eq!(report.rows.len(), 2, "rows either side of the bad line survive");
        assert_eq!(report.malformed, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn non_distinctive_hostnames_are_rejected() {
        // `localhost` reinstates the unknown-machine pooling under another name.
        assert!(distinctive("localhost").is_none());
        assert!(distinctive("  ").is_none());
        assert!(distinctive("unknown").is_none());
        assert_eq!(distinctive("nimbini").as_deref(), Some("nimbini"));
        // First line only, and case folded for APFS.
        assert_eq!(distinctive("Williams-MacBook-Air.local").as_deref(),
                   Some("williams-macbook-air-local"));
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
