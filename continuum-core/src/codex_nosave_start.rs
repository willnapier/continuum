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
//! way. The child records itself between its fork and its exec, holding a
//! copy of the start's lock until then, so Codex never runs unrecorded, even
//! when the wrapper is killed by SIGKILL at once. A child whose fuller record
//! (written by the wrapper) fails is ended, and its start is closed (or left
//! for an import to close), never dropped.
//!
//! The rollouts are listed under every place this Codex may write them: the
//! `CODEX_HOME` it was started with, and `~/.codex`. A rollout elsewhere is
//! held while the start is open and imported as usual afterwards.
//!
//! The cost, which errs towards not saving: another Codex session that begins
//! while a no-save start is open is not stored while it is open, and is
//! registered as no-save when it closes. When the wrapper closes the start,
//! that is when Codex ends. When the wrapper was killed, it is the first
//! close that succeeds after Codex ended: normally the next timer import
//! (every five minutes), later if that import fails or does not run. A
//! session begun before that close is registered too: nothing records when
//! Codex ended. A session that was already running, under the recorded
//! folders, keeps being stored.
//!
//! Not covered: a no-save Codex that goes on in an existing rollout other
//! than by `resume <uuid>` on its command line (a session chosen inside a
//! running Codex), which still relies on its hooks; and a rollout of the
//! session that a Codex app-server creates only after the Codex process has
//! ended.

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

/// What a start's `.start` file holds: where its Codex may write rollouts
/// (absolute paths), and the rollouts (by file stem) that were there before
/// it ran, in any of them.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Record {
    sessions_dirs: Vec<PathBuf>,
    before: BTreeSet<String>,
}

impl Record {
    fn rollouts_now(&self) -> Result<BTreeSet<String>> {
        let mut now = BTreeSet::new();
        for dir in &self.sessions_dirs {
            now.extend(rollout_stems(dir)?);
        }
        Ok(now)
    }
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
    /// A child was spawned, recorded or not: the start may no longer be
    /// dropped, only closed.
    child_spawned: bool,
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
    /// Record a no-save start for Codex writing under any of `sessions_dirs`.
    /// An error means no start is recorded, and Codex must not be started.
    pub fn open(writer: &PlainTextWriter, sessions_dirs: &[PathBuf]) -> Result<Self> {
        let dir = starts_dir(writer)?;
        create_private_dir(&dir)?;
        let mut dirs: Vec<PathBuf> = Vec::new();
        for sessions_dir in sessions_dirs {
            // Absolute, so that an import run from another folder lists the
            // same place.
            let absolute = std::path::absolute(sessions_dir)
                .with_context(|| format!("cannot resolve {}", sessions_dir.display()))?;
            if !dirs.contains(&absolute) {
                dirs.push(absolute);
            }
        }
        let mut record = Record { sessions_dirs: dirs, before: BTreeSet::new() };
        record.before = record.rollouts_now()?;
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
        Ok(Self { _lock: lock, start, child, record, child_spawned: false, closed: false })
    }

    /// Whether a child was spawned. After an error from [`Self::spawn_child`]
    /// with a child spawned, the start must be closed ([`Self::finish`]).
    pub fn child_spawned(&self) -> bool {
        self.child_spawned
    }

    /// Start Codex with `command` and record the child.
    ///
    /// The child records itself, between its fork and its exec, so that no
    /// moment exists in which Codex runs unrecorded, whatever happens to the
    /// wrapper. Until its exec the child also holds a copy of the start's
    /// lock, and `spawn` returns only after the exec. A child that cannot
    /// record itself does not exec: the start fails and nothing ran. The
    /// wrapper then writes the full record (with the start token) in its
    /// place. If that fails, the child is ended again and that is an error; it
    /// may have written a rollout, so the start is kept for [`Self::finish`]
    /// or a later import to close. `spawn_failure` describes a failed start.
    pub fn spawn_child(&mut self, command: &mut Command, spawn_failure: impl FnOnce() -> String) -> Result<Child> {
        let early = SelfRecord::new(&self.child)?;
        // SAFETY: the closure makes only async-signal-safe calls (getpid,
        // open, write, close, rename) on buffers prepared before the fork.
        unsafe {
            command.pre_exec(move || early.write());
        }
        let mut child = command.spawn().with_context(spawn_failure)?;
        self.child_spawned = true;
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
        result
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
    /// Not finished. With no child spawned nothing ran, and the start goes.
    /// With one, it stays for an import to close once the child has ended.
    fn drop(&mut self) {
        if !self.closed && !self.child_spawned {
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
        let file = match File::open(&start) {
            Ok(file) => file,
            // Closed since the listing.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(eyre!("Refusing Codex import: cannot open {}: {e}", start.display())),
        };
        if let Some(record) = check_start(writer, &start, file, close_closed)? {
            open.push(record);
        }
    }
    Ok(open)
}

/// One start, opened as `file` from `start`: its record when it is open (or
/// found closed with `close_closed` false), None when it is closed now.
fn check_start(writer: &PlainTextWriter, start: &Path, mut file: File, close_closed: bool) -> Result<Option<Record>> {
    let child = start.with_extension(&CHILD_SUFFIX[1..]);
    let unlocked = match file.try_lock() {
        Ok(()) => true,
        Err(fs::TryLockError::WouldBlock) => false,
        Err(fs::TryLockError::Error(e)) => {
            return Err(eyre!("Refusing Codex import: cannot check {}: {e}", start.display()))
        }
    };
    let record = read_record(&mut file, start)?;
    if !unlocked {
        return Ok(Some(record));
    }
    // A start is closed, and its files removed, under its lock. So a lock
    // taken on a file that is no longer the one at `start` was taken after
    // another import closed it; closing it again would sweep up rollouts
    // made since. Only the file still published there may be closed.
    if !still_published(&file, start)? {
        return Ok(None);
    }
    if child_alive(&child)? || !close_closed {
        return Ok(Some(record));
    }
    // Its wrapper and its child are gone; locked by us here, so no other
    // import closes it at the same time.
    close(writer, &record)?;
    remove_files(start, &child)?;
    Ok(None)
}

/// Whether `file` is still the file at `path` (same device and inode).
fn still_published(file: &File, path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file.metadata().with_context(|| format!("Refusing Codex import: cannot check {}", path.display()))?;
    match fs::symlink_metadata(path) {
        Ok(now) => Ok(now.dev() == held.dev() && now.ino() == held.ino()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(eyre!("Refusing Codex import: cannot check {}: {e}", path.display())),
    }
}

/// Register every rollout made since the start, and remove its stored copies.
fn close(writer: &PlainTextWriter, record: &Record) -> Result<Closed> {
    let now = record.rollouts_now()?;
    let mut closed = Closed { registered: 0, removed: 0 };
    for stem in now.difference(&record.before) {
        closed.removed += discard_session(writer, "codex", stem)?;
        closed.registered += 1;
    }
    Ok(closed)
}

/// What a no-save start's arguments say about a resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// Not a resume.
    No,
    /// `resume <uuid> …`: the thread id, in lower case.
    Of(String),
    /// A resume whose session cannot be known from its arguments for certain:
    /// none named (`--last`, the picker, a thread name), an option before the
    /// selector, or another thread id among the arguments.
    Unclear,
}

/// Read a no-save start's arguments for a resume (`codex resume …`, `codex
/// exec resume …`). Only one form names its session for certain: `resume`,
/// then a thread id, then nothing but prompt words: no option and no other
/// thread id until `--`. An option anywhere after `resume` makes it unclear,
/// whatever it is: one may choose another session (`--last`), and one before
/// the id may take it as its value. A second thread id may be a prompt or an
/// option's value. Arguments after `--` are a prompt. Options before `resume`
/// (`codex -m <model> resume …`) are Codex's own and choose no session.
pub fn resume_of(args: &[String]) -> Resume {
    let options: Vec<&String> = args.iter().take_while(|arg| *arg != "--").collect();
    let Some(at) = options.iter().position(|arg| *arg == "resume") else { return Resume::No };
    let rest = &options[at + 1..];
    let ids = rest.iter().filter(|arg| import::is_codex_thread_id(arg)).count();
    let an_option = rest.iter().any(|arg| arg.starts_with('-'));
    match rest.first() {
        Some(selector) if import::is_codex_thread_id(selector) && ids == 1 && !an_option => {
            Resume::Of(selector.to_ascii_lowercase())
        }
        _ => Resume::Unclear,
    }
}

/// Register a resumed session as no-save before Codex runs, so the turns it
/// appends to an existing rollout are never stored: by its thread id, and by
/// the stem of every rollout file of that thread under any of
/// `sessions_dirs`. Stored copies of earlier turns are kept: they were saved
/// deliberately. Returns how many rollout files were found.
pub fn register_resumed(writer: &PlainTextWriter, sessions_dirs: &[PathBuf], thread_id: &str) -> Result<usize> {
    let id = thread_id.to_ascii_lowercase();
    register_nosave(writer, &id)?;
    let suffix = format!("-{id}");
    let mut found = 0;
    for dir in sessions_dirs {
        for stem in rollout_stems(dir)?.iter().filter(|stem| stem.to_ascii_lowercase().ends_with(&suffix)) {
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
    // Temporary files left by a child record that was cut short go too.
    let _ = fs::remove_file(self_record_tmp(child));
    let _ = fs::remove_file(child.with_extension("child.tmp"));
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

/// The record a child writes of itself between its fork and its exec
/// (`{"pid":<n>,"start_token":null}`), by way of a temporary file and a
/// rename. Everything that allocates is done before the fork: the child only
/// makes async-signal-safe calls.
struct SelfRecord {
    tmp: std::ffi::CString,
    path: std::ffi::CString,
}

impl SelfRecord {
    fn new(child: &Path) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let c = |path: &Path| {
            std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| eyre!("unusable path for the child record: {}", path.display()))
        };
        Ok(Self { tmp: c(&self_record_tmp(child))?, path: c(child)? })
    }

    /// In the child, before its exec. An error stops the exec.
    fn write(&self) -> std::io::Result<()> {
        let mut text = [0u8; 64];
        let mut n = 0;
        let mut put = |bytes: &[u8]| {
            text[n..n + bytes.len()].copy_from_slice(bytes);
            n += bytes.len();
        };
        put(b"{\"pid\":");
        // SAFETY: getpid is async-signal-safe and cannot fail.
        let mut pid = unsafe { libc::getpid() } as u32;
        let mut digits = [0u8; 10];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (pid % 10) as u8;
            count += 1;
            pid /= 10;
            if pid == 0 {
                break;
            }
        }
        digits[..count].reverse();
        put(&digits[..count]);
        put(b",\"start_token\":null}");
        // SAFETY: async-signal-safe calls on NUL-terminated paths and a stack
        // buffer that outlive them.
        unsafe {
            let fd = libc::open(
                self.tmp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            );
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let written = libc::write(fd, text.as_ptr().cast(), n);
            libc::close(fd);
            if written != n as isize {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            if libc::rename(self.tmp.as_ptr(), self.path.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

fn self_record_tmp(child: &Path) -> PathBuf {
    child.with_extension("child.self.tmp")
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
        /// Last, so it is dropped last.
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    /// One fixture at a time. A child forked by another test thread holds a
    /// copy of every descriptor until its exec, a start's lock among them, so
    /// in parallel a start can look locked just after its wrapper let go. (The
    /// wrapper and the timer each run one thread.)
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fixture() -> Fixture {
        let serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let writer = PlainTextWriter::with_base_dir(root.path().join("logs"))
            .with_nosave_dir(root.path().join("state/nosave-sessions"));
        Fixture { sessions, writer, _root: root, _serial: serial }
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
        let start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
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
        let start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
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
        let mut start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
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
        let mut start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
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
        let start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
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
    fn only_resume_followed_at_once_by_one_thread_id_names_its_session() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let id = "019a0000-0000-7000-8000-0000000000a1";
        let other = "019a0000-0000-7000-8000-0000000000c3";
        let of = |id: &str| Resume::Of(id.to_string());
        assert_eq!(resume_of(&args(&["resume", id])), of(id));
        assert_eq!(resume_of(&args(&["-m", "o3", "exec", "resume", id, "go on"])), of(id));
        assert_eq!(resume_of(&args(&["resume", &id.to_ascii_uppercase()])), of(id), "folded to lower case");
        assert_eq!(resume_of(&args(&["resume", id, "--", other])), of(id), "after -- is a prompt");
        // Unclear: nothing named, a name, an option first, or a second id.
        for unclear in [
            &["resume"][..],
            &["resume", "--last"][..],
            &["resume", "ZZ99"][..],
            &["resume", "zz99-thread-name", other][..],
            &["resume", "--all", id][..],
            &["resume", "-m", id][..],
            &["resume", id, other][..],
            &["resume", id, "-c", other][..],
            // An option after the id: it may choose another session.
            &["resume", id, "--last"][..],
            &["exec", "resume", id, "--last"][..],
            &["resume", id, "--last=true"][..],
            &["resume", id, "-m", "o3"][..],
        ] {
            assert_eq!(resume_of(&args(unclear)), Resume::Unclear, "{unclear:?}");
        }
        assert_eq!(resume_of(&args(&["exec", "ZZ99"])), Resume::No);
        assert_eq!(resume_of(&args(&["fork", id])), Resume::No, "a fork writes a new rollout");
        assert_eq!(resume_of(&args(&["exec", "--", "resume", id])), Resume::No, "a prompt word");
    }

    #[test]
    fn a_child_whose_full_record_fails_leaves_its_start_to_be_closed() {
        let f = fixture();
        let mut start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
        // What the child wrote before the wrapper's record failed.
        let new = rollout(&f, NEW);
        // The wrapper's record cannot be written: its temporary name is taken.
        fs::write(start.child.with_extension("child.tmp"), b"taken").unwrap();
        let spawned = start.spawn_child(Command::new("sleep").arg("30"), String::new);
        let was_spawned = start.child_spawned();
        // The wrapper wrote no record, so this one is the child's own,
        // written before its exec.
        let own: ChildRecord = serde_json::from_str(&fs::read_to_string(&start.child).unwrap()).unwrap();
        drop(start);

        assert!(spawned.is_err() && was_spawned, "spawned, then the record failed");
        assert!(own.pid > 0 && own.start_token.is_none(), "{own:?}");
        assert!(starts(&f).iter().any(|n| n.ends_with(".start")), "the start is kept: {:?}", starts(&f));
        let later = import_codex(&f.writer, &new).unwrap();
        assert!(matches!(later, Imported::NoSave), "closed, then refused: {later:?}");
    }

    #[test]
    fn a_child_that_cannot_record_itself_never_runs() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let mut start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
        let ran = f._root.path().join("ran");
        let dir = f.writer.codex_starts_dir().unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let spawned = start.spawn_child(
            Command::new("/bin/sh").arg("-c").arg(format!(": > '{}'", ran.display())),
            String::new,
        );
        let was_spawned = start.child_spawned();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        drop(start);

        assert!(spawned.is_err() && !was_spawned, "the start fails: {spawned:?}");
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!ran.exists(), "the child never ran its program");
        assert_eq!(starts(&f), Vec::<String>::new(), "nothing ran, so the start goes");
    }


    /// The cost after a killed wrapper lasts until a close succeeds, not until
    /// a set time: a close that fails is tried again by the next import, which
    /// also takes in a session begun meanwhile.
    #[test]
    fn a_failed_close_is_retried_and_takes_in_sessions_begun_meanwhile() {
        let f = fixture();
        let mut start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
        let mut child = start.spawn_child(&mut Command::new("true"), String::new).unwrap();
        child.wait().unwrap();
        drop(start); // the wrapper is killed; its Codex has ended
        let new = rollout(&f, NEW);
        // The registry cannot be written: a file where its folder should be.
        let registry = f.writer.nosave_dir().unwrap().to_path_buf();
        fs::create_dir_all(registry.parent().unwrap()).unwrap();
        fs::write(&registry, b"not a folder").unwrap();
        let failed = import_codex(&f.writer, &new);
        fs::remove_file(&registry).unwrap();
        assert!(failed.is_err(), "the close fails, and nothing is stored: {failed:?}");
        assert_eq!(starts(&f).len(), 2, "the start is still there");

        // An ordinary session begun after the failed close.
        let later_day = f.sessions.join("2026/10/08");
        fs::create_dir_all(&later_day).unwrap();
        let later = later_day.join("rollout-2026-10-08T08-00-00-019a0000-0000-7000-8000-0000000000d4.jsonl");
        fs::write(&later, format!("{LINE}\n")).unwrap();

        assert!(matches!(import_codex(&f.writer, &later).unwrap(), Imported::NoSave), "taken in by the retried close");
        assert!(matches!(import_codex(&f.writer, &new).unwrap(), Imported::NoSave));
        assert_eq!(starts(&f), Vec::<String>::new());
    }

    #[test]
    fn a_start_another_import_has_closed_is_not_closed_again() {
        let f = fixture();
        let start = CodexNoSaveStart::open(&f.writer, std::slice::from_ref(&f.sessions)).unwrap();
        let path = start.start.clone();
        // A slow import has opened the start file and not yet locked it.
        let slow = File::open(&path).unwrap();
        drop(start); // no child: the start goes, as one closed by another import
        assert_eq!(starts(&f), Vec::<String>::new());
        // An ordinary session begins after that.
        let later = rollout(&f, NEW);

        let found = check_start(&f.writer, &path, slow, true).unwrap();
        assert!(found.is_none(), "a closed start is neither open nor closed again");
        assert!(stored(&f, &later), "and the later session is not registered");
    }

    #[test]
    fn rollouts_under_every_recorded_root_are_closed() {
        let f = fixture();
        let other_root = f._root.path().join("codex-home/sessions");
        let mut start = CodexNoSaveStart::open(&f.writer, &[f.sessions.clone(), other_root.clone()]).unwrap();
        let mut child = start.spawn_child(&mut Command::new("true"), String::new).unwrap();
        child.wait().unwrap();
        let day = other_root.join("2026/10/07");
        fs::create_dir_all(&day).unwrap();
        let elsewhere = day.join(format!("{NEW}.jsonl"));
        fs::write(&elsewhere, format!("{LINE}\n")).unwrap();
        assert_eq!(start.finish(&f.writer).unwrap(), Closed { registered: 1, removed: 0 });
        assert!(matches!(import_codex(&f.writer, &elsewhere).unwrap(), Imported::NoSave));
    }

    #[test]
    fn a_resumed_rollout_is_registered_by_its_file_and_its_thread_id() {
        let f = fixture();
        let earlier = rollout(&f, EARLIER);
        assert!(stored(&f, &earlier), "saved before the resume");
        let copy = f.writer.base_dir().join("codex/2026-10-07").join(EARLIER);
        let found = register_resumed(&f.writer, std::slice::from_ref(&f.sessions), "019A0000-0000-7000-8000-0000000000A1").unwrap();
        assert_eq!(found, 1);
        let registry = f.writer.nosave_dir().unwrap();
        assert!(registry.join(EARLIER).exists() && registry.join("019a0000-0000-7000-8000-0000000000a1").exists());
        assert!(matches!(import_codex(&f.writer, &earlier).unwrap(), Imported::NoSave));
        assert!(copy.exists(), "the turns saved before the resume are kept");
    }
}
