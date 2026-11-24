//! Open no-save Codex starts: what keeps a no-save Codex session out of
//! Continuum when its hooks do not run.
//!
//! A no-save Codex session is normally kept out by its hooks, which put its
//! thread id in the no-save registry (`import::register_nosave`) as it runs.
//! A hook that fails, or that Codex has not trusted yet, leaves nothing in the
//! registry, and the Codex timer (`continuum import -a codex`) would then store
//! the session, possibly while it is still running. The wrapper cannot tell
//! which rollout is its own: it sees only the files under `sessions/`.
//!
//! So `continuum-codex` records every no-save start before Codex runs, and the
//! importer honours the record:
//!
//! 1. [`CodexNoSaveStart::open`] lists the rollouts that exist now and writes
//!    that list to `codex-nosave-starts/<pid>-<nanos>.start` beside the
//!    registry, holding an exclusive lock on the file for the wrapper's life.
//! 2. [`CodexNoSaveStart::spawn_child`] starts Codex and records its process
//!    id in `<…>.child`.
//! 3. While a start is open, [`holds`] tells `import::import_codex` to store no
//!    rollout that is not on the start's list: any rollout made since then may
//!    be the no-save session's, under any name.
//! 4. When it closes, every rollout made since the start is registered as
//!    no-save, and any stored copy of one is removed (`import::discard_session`).
//!    Rollouts that were there before are not touched.
//!
//! A start is open while its lock is held (the wrapper is alive) or its
//! recorded child is alive. The wrapper closes it itself when Codex has ended
//! ([`CodexNoSaveStart::finish`]), however it ended. A wrapper that is itself
//! killed (a closed terminal tab sends it SIGHUP) leaves the files behind; the
//! first import after its child has also ended closes the start in the same
//! way. Between the spawn and the child record the wrapper blocks SIGHUP,
//! SIGINT, SIGTERM and SIGQUIT, so only SIGKILL in that instant can leave a
//! start with no child recorded, which is then taken as closed.
//!
//! The cost, which errs towards not saving: another Codex session that begins
//! while a no-save start is open is not stored while it is open, and is
//! registered as no-save when it closes. A session that was already running
//! keeps being stored.

use crate::import::{self, discard_session, register_nosave};
use crate::PlainTextWriter;
use color_eyre::{eyre::eyre, eyre::Context, Result};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

const START_SUFFIX: &str = ".start";
const CHILD_SUFFIX: &str = ".child";

/// What a start's `.start` file holds: where its Codex writes rollouts, and
/// the rollouts (by file stem) that were there before it ran.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Record {
    sessions_dir: PathBuf,
    before: BTreeSet<String>,
}

/// What a start's `.child` file holds. `start_token` tells a reused process id
/// from the child (Linux: the start time in `/proc/<pid>/stat`).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ChildRecord {
    pid: u32,
    start_token: Option<String>,
}

/// One open no-save Codex start, held by the wrapper until Codex has ended.
#[derive(Debug)]
pub struct CodexNoSaveStart {
    /// The `.start` file, locked while this value lives.
    _lock: File,
    start: PathBuf,
    child: PathBuf,
    record: Record,
    child_recorded: bool,
    closed: bool,
}

/// What closing a start did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed {
    /// Rollouts made since the start, now registered as no-save.
    pub registered: usize,
    /// Stored copies of those rollouts that were removed.
    pub removed: usize,
}

impl CodexNoSaveStart {
    /// Record a no-save start for Codex writing under `sessions_dir`. An error
    /// means no start is recorded, and Codex must not be started.
    pub fn open(writer: &PlainTextWriter, sessions_dir: &Path) -> Result<Self> {
        let dir = starts_dir(writer)?;
        create_private_dir(&dir)?;
        let record = Record {
            sessions_dir: sessions_dir.to_path_buf(),
            before: rollout_stems(sessions_dir)?,
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let name = format!("{}-{nanos}", std::process::id());
        let start = dir.join(format!("{name}{START_SUFFIX}"));
        let child = dir.join(format!("{name}{CHILD_SUFFIX}"));
        // Locked before it has its final name, so that no importer can find
        // it unlocked while the wrapper lives.
        let tmp = dir.join(format!(".{name}.tmp"));
        let lock = (|| -> Result<File> {
            let mut file = private_file(&tmp)?;
            file.lock().with_context(|| format!("Failed to lock {}", tmp.display()))?;
            file.write_all(&serde_json::to_vec(&record)?)?;
            file.sync_all()?;
            fs::rename(&tmp, &start).with_context(|| format!("Failed to record the start at {}", start.display()))?;
            Ok(file)
        })();
        let lock = lock.inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(Self { _lock: lock, start, child, record, child_recorded: false, closed: false })
    }

    /// Start Codex with `command` and record the child. Signals that would
    /// end the wrapper are held back until the child is on record (the child
    /// starts with none held back); a child that cannot be recorded is ended
    /// again, and that is an error. `spawn_failure` describes a failed start.
    pub fn spawn_child(&mut self, command: &mut Command, spawn_failure: impl FnOnce() -> String) -> Result<Child> {
        let _held = SignalsHeld::new();
        // SAFETY: the closure only clears the signal mask, which is
        // async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                let mut none: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut none);
                libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
                Ok(())
            });
        }
        let mut child = command.spawn().with_context(spawn_failure)?;
        if let Err(error) = self.record_child(child.id()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(child)
    }

    fn record_child(&mut self, pid: u32) -> Result<()> {
        let record = ChildRecord { pid, start_token: start_token(pid) };
        let tmp = self.child.with_extension("child.tmp");
        let result = (|| -> Result<()> {
            let mut file = private_file(&tmp)?;
            file.write_all(&serde_json::to_vec(&record)?)?;
            file.sync_all()?;
            fs::rename(&tmp, &self.child).with_context(|| format!("Failed to record the child at {}", self.child.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        self.child_recorded = true;
        Ok(())
    }

    /// Codex has ended, however it ended: close the start. On an error the
    /// start stays open, and a later import closes it.
    pub fn finish(mut self, writer: &PlainTextWriter) -> Result<Closed> {
        let closed = close(writer, &self.record)?;
        remove_files(&self.start, &self.child)?;
        self.closed = true;
        Ok(closed)
    }
}

impl Drop for CodexNoSaveStart {
    /// Not finished. With no child on record nothing ran, and the start goes.
    /// With one, it stays for an import to close once the child has ended.
    fn drop(&mut self) {
        if !self.closed && !self.child_recorded {
            let _ = remove_files(&self.start, &self.child);
        }
    }
}

/// Whether an open no-save Codex start holds `stem` (a rollout's file stem)
/// back from being stored: it is not on the list of a start that is open.
/// Starts found closed (no lock held, no child alive) are closed here first.
/// Fails closed: a start that cannot be read is an error.
pub fn holds(writer: &PlainTextWriter, stem: &str) -> Result<bool> {
    Ok(open_starts(writer, true)?.iter().any(|record| !record.before.contains(stem)))
}

/// [`holds`] without closing anything: a start found closed still counts as
/// open. For a caller already holding a session's import lock, which closing
/// a start could need.
pub fn holds_without_closing(writer: &PlainTextWriter, stem: &str) -> Result<bool> {
    Ok(open_starts(writer, false)?.iter().any(|record| !record.before.contains(stem)))
}

fn open_starts(writer: &PlainTextWriter, close_closed: bool) -> Result<Vec<Record>> {
    let dir = starts_dir(writer)?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(eyre!("Refusing Codex import: cannot read the no-save starts at {}: {e}", dir.display())),
    };
    let mut open = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("Refusing Codex import: cannot read {}", dir.display()))?;
        let start = entry.path();
        let Some(name) = start.file_name().and_then(|n| n.to_str()) else { continue };
        if name.starts_with('.') || !name.ends_with(START_SUFFIX) {
            continue;
        }
        let mut file = match File::open(&start) {
            Ok(file) => file,
            // Closed since the listing.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(eyre!("Refusing Codex import: cannot open {}: {e}", start.display())),
        };
        let child = start.with_extension(&CHILD_SUFFIX[1..]);
        let unlocked = match file.try_lock() {
            Ok(()) => true,
            Err(fs::TryLockError::WouldBlock) => false,
            Err(fs::TryLockError::Error(e)) => {
                return Err(eyre!("Refusing Codex import: cannot check {}: {e}", start.display()))
            }
        };
        let record = read_record(&mut file, &start)?;
        if unlocked && !child_alive(&child)? && close_closed {
            // Its wrapper and its child are gone; still locked by us here,
            // so no other import closes it at the same time.
            close(writer, &record)?;
            remove_files(&start, &child)?;
            continue;
        }
        open.push(record);
    }
    Ok(open)
}

/// Register every rollout made since the start, and remove its stored copies.
fn close(writer: &PlainTextWriter, record: &Record) -> Result<Closed> {
    let now = rollout_stems(&record.sessions_dir)?;
    let mut closed = Closed { registered: 0, removed: 0 };
    for stem in now.difference(&record.before) {
        closed.removed += discard_session(writer, "codex", stem)?;
        closed.registered += 1;
    }
    Ok(closed)
}

/// A no-save `codex resume`: the thread ids it names, or None when the
/// arguments are not a resume. `Some(empty)` is a resume that names no thread
/// id (`--last`, the picker, a thread name), whose rollout cannot be known
/// before Codex runs.
pub fn resume_thread_ids(args: &[String]) -> Option<Vec<String>> {
    let options = || args.iter().take_while(|arg| *arg != "--");
    let at = options().position(|arg| arg == "resume")?;
    Some(options().skip(at + 1).filter(|arg| import::is_codex_thread_id(arg)).cloned().collect())
}

/// Register a resumed session as no-save before Codex runs, so the turns it
/// appends to an existing rollout are never stored: by its thread id, and by
/// the stem of every rollout file of that thread under `sessions_dir`.
/// Stored copies of earlier turns are kept: they were saved deliberately.
/// Returns how many rollout files were found.
pub fn register_resumed(writer: &PlainTextWriter, sessions_dir: &Path, thread_ids: &[String]) -> Result<usize> {
    let mut found = 0;
    for id in thread_ids {
        register_nosave(writer, id)?;
        let suffix = format!("-{id}");
        for stem in rollout_stems(sessions_dir)?.iter().filter(|stem| stem.ends_with(&suffix)) {
            register_nosave(writer, stem)?;
            found += 1;
        }
    }
    Ok(found)
}

fn starts_dir(writer: &PlainTextWriter) -> Result<PathBuf> {
    writer
        .codex_starts_dir()
        .ok_or_else(|| eyre!("HOME is unavailable, so the no-save Codex starts cannot be found"))
}

fn read_record(file: &mut File, path: &Path) -> Result<Record> {
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("Refusing Codex import: cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("Refusing Codex import: {} is not a start record", path.display()))
}

/// Whether the child recorded at `path` is still running. No record: it never
/// started, or the wrapper was killed before it could record it.
fn child_alive(path: &Path) -> Result<bool> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(eyre!("Refusing Codex import: cannot read {}: {e}", path.display())),
    };
    let record: ChildRecord = serde_json::from_str(&text)
        .with_context(|| format!("Refusing Codex import: {} is not a child record", path.display()))?;
    Ok(process_alive(record.pid, record.start_token.as_deref()))
}

fn process_alive(pid: u32, token: Option<&str>) -> bool {
    let Ok(pid_t) = libc::pid_t::try_from(pid) else { return false };
    if pid_t <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let exists = unsafe { libc::kill(pid_t, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !exists || is_zombie(pid) {
        return false;
    }
    match (token, start_token(pid)) {
        (Some(then), Some(now)) => then == now,
        // Nothing to compare: take it as the child, which keeps holding.
        _ => true,
    }
}

/// The process's start time, from `/proc/<pid>/stat` (field 22).
#[cfg(target_os = "linux")]
fn start_token(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name, field 2, may hold spaces and brackets: count from
    // after the last ')', which is field 3.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(22 - 3).map(str::to_string)
}

#[cfg(not(target_os = "linux"))]
fn start_token(_pid: u32) -> Option<String> {
    None
}

/// A child that has ended but not been reaped yet (state `Z`, field 3).
#[cfg(target_os = "linux")]
fn is_zombie(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| stat.rfind(')').map(|at| stat[at + 1..].trim_start().starts_with('Z')))
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn is_zombie(_pid: u32) -> bool {
    false
}

/// Every Codex rollout under `sessions_dir` (`YYYY/MM/DD/rollout-*.jsonl`),
/// by file stem. A folder that cannot be read is an error: a rollout missed
/// here could be stored. No `sessions_dir` at all is no rollouts.
pub fn rollout_stems(sessions_dir: &Path) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let mut dirs = vec![sessions_dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir == sessions_dir => continue,
            Err(e) => return Err(eyre!("cannot list Codex rollouts in {}: {e}", dir.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot list Codex rollouts in {}", dir.display()))?;
            let kind = entry.file_type().with_context(|| format!("cannot list Codex rollouts in {}", dir.display()))?;
            let name = entry.file_name();
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if let Some(stem) =
                name.to_str().filter(|n| n.starts_with("rollout-")).and_then(|n| n.strip_suffix(".jsonl"))
            {
                found.insert(stem.to_string());
            }
        }
    }
    Ok(found)
}

fn remove_files(start: &Path, child: &Path) -> Result<()> {
    for path in [child, start] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(eyre!("Failed to remove {}: {e}", path.display())),
        }
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).with_context(|| format!("Failed to create {}", path.display()))
}

/// SIGHUP, SIGINT, SIGTERM and SIGQUIT held back on this thread until drop;
/// a signal that came meanwhile is delivered then. A child would inherit the
/// mask (Rust's process spawning keeps it), so `spawn_child` clears it in
/// the child before its exec.
struct SignalsHeld(Option<libc::sigset_t>);

impl SignalsHeld {
    fn new() -> Self {
        // SAFETY: plain sigset manipulation on stack values.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
                libc::sigaddset(&mut set, signal);
            }
            if libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old) == 0 {
                Self(Some(old))
            } else {
                Self(None)
            }
        }
    }
}

impl Drop for SignalsHeld {
    fn drop(&mut self) {
        if let Some(old) = self.0.take() {
            // SAFETY: restores the mask saved in `new`.
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{import_codex, Imported};

    const EARLIER: &str = "rollout-2026-10-06T08-00-00-019a0000-0000-7000-8000-0000000000a1";
    const NEW: &str = "rollout-2026-10-07T08-00-00-019a0000-0000-7000-8000-0000000000b2";
    const LINE: &str = r#"{"type":"response_item","timestamp":"2026-10-07T08:00:00.000Z","payload":{"role":"user","content":[{"type":"input_text","text":"zz99 invented question"}]}}"#;

    struct Fixture {
        _root: tempfile::TempDir,
        sessions: PathBuf,
        writer: PlainTextWriter,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let writer = PlainTextWriter::with_base_dir(root.path().join("logs"))
            .with_nosave_dir(root.path().join("state/nosave-sessions"));
        Fixture { sessions, writer, _root: root }
    }

    fn rollout(f: &Fixture, stem: &str) -> PathBuf {
        let day = f.sessions.join("2026/10/07");
        fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("{stem}.jsonl"));
        fs::write(&path, format!("{LINE}\n")).unwrap();
        path
    }

    fn stored(f: &Fixture, path: &Path) -> bool {
        match import_codex(&f.writer, path).unwrap() {
            Imported::Stored(_) => true,
            Imported::NoSave | Imported::Held => false,
            Imported::Empty => panic!("the rollout has a message"),
        }
    }

    fn starts(f: &Fixture) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(f.writer.codex_starts_dir().unwrap())
            .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn an_open_start_holds_new_rollouts_and_its_close_registers_them() {
        let f = fixture();
        let earlier = rollout(&f, EARLIER);
        let start = CodexNoSaveStart::open(&f.writer, &f.sessions).unwrap();
        let new = rollout(&f, NEW);

        assert!(matches!(import_codex(&f.writer, &new).unwrap(), Imported::Held), "held while open");
        assert!(!f.writer.base_dir().join("codex").join("2026-10-07").join(NEW).exists());
        assert!(stored(&f, &earlier), "a rollout that was there before is stored");

        let closed = start.finish(&f.writer).unwrap();
        assert_eq!(closed, Closed { registered: 1, removed: 0 });
        assert_eq!(starts(&f), Vec::<String>::new(), "the start is gone");
        assert!(matches!(import_codex(&f.writer, &new).unwrap(), Imported::NoSave), "and the rollout is no-save");
        assert!(stored(&f, &earlier), "the earlier one is untouched");
    }

    #[test]
    fn without_a_start_the_same_rollout_is_stored() {
        let f = fixture();
        let new = rollout(&f, NEW);
        assert!(stored(&f, &new));
    }

    #[test]
    fn closing_removes_a_stored_copy_of_a_new_rollout() {
        let f = fixture();
        let start = CodexNoSaveStart::open(&f.writer, &f.sessions).unwrap();
        rollout(&f, NEW);
        // A copy stored by something that did not honour the start.
        let copy = f.writer.base_dir().join("codex/2026-10-07").join(NEW);
        fs::create_dir_all(&copy).unwrap();
        fs::write(copy.join("messages.jsonl"), b"{}\n").unwrap();
        let unrelated = f.writer.base_dir().join("codex/2026-10-07").join(EARLIER);
        fs::create_dir_all(&unrelated).unwrap();

        assert_eq!(start.finish(&f.writer).unwrap(), Closed { registered: 1, removed: 1 });
        assert!(!copy.exists(), "the stored copy is removed");
        assert!(unrelated.exists(), "another session's copy is not");
    }

    #[test]
    fn a_start_whose_wrapper_and_child_are_gone_is_closed_by_the_next_import() {
        let f = fixture();
        let mut start = CodexNoSaveStart::open(&f.writer, &f.sessions).unwrap();
        let mut child = start.spawn_child(&mut Command::new("true"), String::new).unwrap();
        child.wait().unwrap();
        // The wrapper "dies" unfinished: its lock goes, its files stay.
        drop(start);
        assert_eq!(starts(&f).len(), 2, "a start with a child on record stays: {:?}", starts(&f));
        let new = rollout(&f, NEW);

        assert!(matches!(import_codex(&f.writer, &new).unwrap(), Imported::NoSave), "closed, then refused");
        assert_eq!(starts(&f), Vec::<String>::new());
    }

    #[test]
    fn a_start_whose_child_lives_on_stays_open_after_its_wrapper() {
        let f = fixture();
        let mut start = CodexNoSaveStart::open(&f.writer, &f.sessions).unwrap();
        let mut child = start.spawn_child(Command::new("sleep").arg("30"), String::new).unwrap();
        drop(start);
        let new = rollout(&f, NEW);
        let held = import_codex(&f.writer, &new).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(matches!(held, Imported::Held), "held while the child lives");
        assert!(matches!(import_codex(&f.writer, &new).unwrap(), Imported::NoSave), "closed once it has ended");
    }

    #[test]
    fn a_start_that_never_ran_a_child_leaves_nothing() {
        let f = fixture();
        let start = CodexNoSaveStart::open(&f.writer, &f.sessions).unwrap();
        assert_eq!(starts(&f).len(), 1);
        drop(start);
        assert_eq!(starts(&f), Vec::<String>::new());
        assert!(stored(&f, &rollout(&f, NEW)));
    }

    #[test]
    fn an_unreadable_starts_folder_refuses_the_import() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let dir = f.writer.codex_starts_dir().unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        let result = import_codex(&f.writer, &rollout(&f, NEW));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err(), "{result:?}");
        assert!(!f.writer.base_dir().join("codex").exists());
    }

    #[test]
    fn a_resume_is_recognised_by_its_thread_id() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let id = "019a0000-0000-7000-8000-0000000000a1";
        assert_eq!(resume_thread_ids(&args(&["resume", id])), Some(vec![id.to_string()]));
        assert_eq!(resume_thread_ids(&args(&["-m", "o3", "exec", "resume", "--all", id, "go"])), Some(vec![id.to_string()]));
        assert_eq!(resume_thread_ids(&args(&["resume", "--last"])), Some(vec![]));
        assert_eq!(resume_thread_ids(&args(&["resume", "ZZ99"])), Some(vec![]));
        assert_eq!(resume_thread_ids(&args(&["exec", "ZZ99"])), None);
        assert_eq!(resume_thread_ids(&args(&["fork", id])), None, "a fork writes a new rollout");
        assert_eq!(resume_thread_ids(&args(&["exec", "--", "resume", id])), None, "a prompt word");
    }

    #[test]
    fn a_resumed_rollout_is_registered_by_its_file_and_its_thread_id() {
        let f = fixture();
        let earlier = rollout(&f, EARLIER);
        assert!(stored(&f, &earlier), "saved before the resume");
        let copy = f.writer.base_dir().join("codex/2026-10-07").join(EARLIER);
        let found = register_resumed(&f.writer, &f.sessions, &["019a0000-0000-7000-8000-0000000000a1".into()]).unwrap();
        assert_eq!(found, 1);
        let registry = f.writer.nosave_dir().unwrap();
        assert!(registry.join(EARLIER).exists() && registry.join("019a0000-0000-7000-8000-0000000000a1").exists());
        assert!(matches!(import_codex(&f.writer, &earlier).unwrap(), Imported::NoSave));
        assert!(copy.exists(), "the turns saved before the resume are kept");
    }
}
