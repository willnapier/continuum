//! What `continuum` reports about no-save Codex starts
//! (`continuum_core::codex_nosave_start`), run as a real process in a scratch
//! home: the close an import makes after a killed wrapper (its close log
//! entry and its one line), a close log that cannot be written, and the report
//! of a start on record too long (`continuum codex nosave-starts`). The
//! rollouts and thread ids are invented; no Codex runs. A start left by a
//! killed wrapper is made with the library, as the wrapper makes it, with
//! `true` as its child.
//!
//! `CONTINUUM_UNDER_TEST` runs the cases against another build of `continuum`
//! (the red run against the build before this change).
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use continuum_core::codex_nosave_start::CodexNoSaveStart;
use continuum_core::PlainTextWriter;
use masked_fixture::{said, Scratch};
use std::path::{Path, PathBuf};
use std::process::Output;

const EARLIER_THREAD: &str = "019a0000-0000-7000-8000-0000000000a1";
const NEW_THREAD: &str = "019a0000-0000-7000-8000-0000000000b2";

fn continuum() -> String {
    std::env::var("CONTINUUM_UNDER_TEST").unwrap_or_else(|_| env!("CARGO_BIN_EXE_continuum").to_string())
}

fn state(scratch: &Scratch) -> PathBuf {
    scratch.home.join(".local/share/continuum")
}

/// The writer `continuum` uses in the scratch home.
fn writer(scratch: &Scratch) -> PlainTextWriter {
    PlainTextWriter::with_base_dir(scratch.home.join("Assistants/continuum-logs"))
        .with_nosave_dir(state(scratch).join("nosave-sessions"))
}

fn sessions(scratch: &Scratch) -> PathBuf {
    scratch.home.join(".codex/sessions")
}

fn rollout(scratch: &Scratch, thread: &str) -> PathBuf {
    let dir = sessions(scratch).join("2026/10/07");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-10-07T08-00-00-{thread}.jsonl"));
    std::fs::write(
        &path,
        r#"{"type":"response_item","timestamp":"2026-10-07T08:00:00.000Z","payload":{"role":"user","content":[{"type":"input_text","text":"zz99 invented question"}]}}
"#,
    )
    .unwrap();
    path
}

fn stem(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

/// The names in `dir`, sorted; empty when it is not there.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

fn starts(scratch: &Scratch) -> Vec<String> {
    names(&state(scratch).join("codex-nosave-starts"))
}

fn close_log_path(scratch: &Scratch) -> PathBuf {
    state(scratch).join("codex-nosave-closes.jsonl")
}

fn close_log(scratch: &Scratch) -> Vec<serde_json::Value> {
    std::fs::read_to_string(close_log_path(scratch))
        .map(|text| text.lines().map(|l| serde_json::from_str(l).unwrap()).collect())
        .unwrap_or_default()
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

/// A start whose wrapper was killed after its Codex ended: its files stay,
/// no lock is held and no child lives.
fn killed_wrappers_start(scratch: &Scratch) {
    let mut start = CodexNoSaveStart::open(&writer(scratch), &[sessions(scratch)]).unwrap();
    let mut child = start.spawn_child(&mut std::process::Command::new("true"), String::new).unwrap();
    child.wait().unwrap();
    drop(start);
    assert_eq!(starts(scratch).len(), 2, "the start and its child record stay");
}

/// A stored copy of `rollout`, as left by something that did not honour the
/// start.
fn stored_copy(scratch: &Scratch, rollout: &Path) -> PathBuf {
    let copy = scratch.home.join("Assistants/continuum-logs/codex/2026-10-07").join(stem(rollout));
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("messages.jsonl"), b"{}\n").unwrap();
    copy
}

/// The timer's command for one rollout.
fn import(scratch: &Scratch, rollout: &Path) -> Output {
    scratch
        .command(&continuum())
        .args(["import", "-a", "codex", "-s"])
        .arg(rollout)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The importer's line: the counts, on one line.
fn close_lines(output: &Output) -> Vec<String> {
    stderr(output).lines().filter(|l| l.starts_with("= Closed no-save Codex start ")).map(str::to_string).collect()
}

// --- A close by the importer -------------------------------------------

#[test]
fn a_close_by_the_importer_is_logged_and_printed() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "closelog-import");
    let earlier = rollout(&scratch, EARLIER_THREAD);
    killed_wrappers_start(&scratch);
    let new = rollout(&scratch, NEW_THREAD);
    let copy = stored_copy(&scratch, &new);

    let output = import(&scratch, &earlier);
    let context = said(&output);
    assert_eq!(output.status.code(), Some(0), "{context}");
    assert_eq!(starts(&scratch), Vec::<String>::new(), "closed\n{context}");
    assert!(!copy.exists(), "{context}");

    let lines = close_lines(&output);
    assert_eq!(lines.len(), 1, "one line\n{context}");
    assert!(lines[0].ends_with("marked 1 Codex session(s) no-save, removed 1 stored copy(ies)"), "{context}");
    let log = close_log(&scratch);
    assert_eq!(log.len(), 1, "{log:?}\n{context}");
    assert_eq!(log[0]["closer"], "import", "{context}");
    assert!(log[0]["start"].as_str().is_some_and(|s| lines[0].contains(s) && s.ends_with(".start")), "{context}");
    assert_eq!(strings(&log[0]["marked"]), [stem(&new)], "{context}");
    assert_eq!(strings(&log[0]["removed"]), [copy.to_string_lossy().into_owned()], "{context}");
    assert!(log[0]["failed"].is_null(), "{context}");
    assert!(!std::fs::read_to_string(close_log_path(&scratch)).unwrap().contains("zz99"), "no content");
}

#[test]
fn a_close_by_the_importer_that_marks_nothing_is_logged_and_printed() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "closelog-import-nothing");
    let earlier = rollout(&scratch, EARLIER_THREAD);
    killed_wrappers_start(&scratch);

    let output = import(&scratch, &earlier);
    let context = said(&output);
    assert_eq!(output.status.code(), Some(0), "{context}");
    let lines = close_lines(&output);
    assert_eq!(lines.len(), 1, "{context}");
    assert!(lines[0].ends_with("marked 0 Codex session(s) no-save, removed 0 stored copy(ies)"), "{context}");
    let log = close_log(&scratch);
    assert_eq!(log.len(), 1, "{log:?}\n{context}");
    assert_eq!((strings(&log[0]["marked"]), strings(&log[0]["removed"])), (vec![], vec![]), "{context}");
}

/// The control: an import with no start to close prints no close line and
/// writes no log.
#[test]
fn control_an_import_with_no_start_logs_and_prints_no_close() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "closelog-import-control");
    let earlier = rollout(&scratch, EARLIER_THREAD);
    let output = import(&scratch, &earlier);
    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(close_lines(&output), Vec::<String>::new(), "{}", said(&output));
    assert_eq!(close_log(&scratch), Vec::<serde_json::Value>::new());
}

/// A close log that cannot be written (a folder where the file should be)
/// stops neither the marking nor the removal, and the import exits non-zero,
/// so the timer raises its alert. The next import is not held back.
#[test]
fn a_log_that_cannot_be_written_does_not_stop_the_close_and_the_import_fails() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "closelog-import-unwritable");
    let earlier = rollout(&scratch, EARLIER_THREAD);
    killed_wrappers_start(&scratch);
    let new = rollout(&scratch, NEW_THREAD);
    let copy = stored_copy(&scratch, &new);
    std::fs::create_dir_all(close_log_path(&scratch)).unwrap();

    let output = import(&scratch, &earlier);
    let context = said(&output);
    assert_ne!(output.status.code(), Some(0), "the import fails\n{context}");
    assert!(stderr(&output).contains("the close is not in the close log"), "{context}");
    assert_eq!(names(&state(&scratch).join("nosave-sessions")), [stem(&new)], "marked\n{context}");
    assert!(!copy.exists(), "removed\n{context}");
    assert_eq!(starts(&scratch), Vec::<String>::new(), "closed, not left to be closed again\n{context}");

    let again = import(&scratch, &earlier);
    assert_eq!(again.status.code(), Some(0), "{}", said(&again));
    assert!(scratch.home.join("Assistants/continuum-logs/codex/2026-10-07").join(stem(&earlier)).exists());
}

// --- A start on record too long ----------------------------------------

/// A start record named as the wrapper names one, opened `hours` ago.
fn start_record_from(scratch: &Scratch, hours: u64) -> PathBuf {
    let dir = state(scratch).join("codex-nosave-starts");
    std::fs::create_dir_all(&dir).unwrap();
    let opened = std::time::SystemTime::now() - std::time::Duration::from_secs(hours * 3600 + 60);
    let nanos = opened.duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let path = dir.join(format!("4242-{nanos}.start"));
    std::fs::write(&path, br#"{"sessions_dirs":[],"before":[]}"#).unwrap();
    path
}

fn report(scratch: &Scratch) -> Output {
    scratch.command(&continuum()).args(["codex", "nosave-starts"]).output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn an_old_start_is_reported_by_count_and_age_and_left_alone() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "oldstart");
    let old = start_record_from(&scratch, 7);
    start_record_from(&scratch, 0);
    let before = (starts(&scratch), std::fs::read(&old).unwrap(), std::fs::metadata(&old).unwrap().modified().unwrap());

    let output = report(&scratch);
    let context = said(&output);
    assert_eq!(output.status.code(), Some(1), "{context}");
    assert!(
        stdout(&output).starts_with("1 of 2 no-save Codex start(s) on record for more than 6 h; the oldest for 7 h 1 min"),
        "{context}"
    );
    let old_lines: Vec<String> = stdout(&output).lines().filter(|l| l.starts_with("old: ")).map(str::to_string).collect();
    assert_eq!(old_lines, ["old: 7 h 1 min"], "{context}");
    let after = (starts(&scratch), std::fs::read(&old).unwrap(), std::fs::metadata(&old).unwrap().modified().unwrap());
    assert_eq!(before, after, "nothing closed, changed or removed");
    assert!(!close_log_path(&scratch).exists(), "and nothing closed");
}

/// The control: a fresh start is not reported, and neither is none.
#[test]
fn control_a_fresh_start_is_not_reported() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "oldstart-fresh");
    let none = report(&scratch);
    assert_eq!(none.status.code(), Some(0), "{}", said(&none));
    assert!(stdout(&none).contains("(0 on record)"), "{}", said(&none));

    start_record_from(&scratch, 5);
    let fresh = report(&scratch);
    assert_eq!(fresh.status.code(), Some(0), "{}", said(&fresh));
    assert_eq!(stdout(&fresh).trim(), "no no-save Codex start older than 6 h (1 on record)", "{}", said(&fresh));
    assert_eq!(starts(&scratch).len(), 1);

    // The age is a setting.
    let lower = scratch.command(&continuum()).args(["codex", "nosave-starts", "--older-than-hours", "4"]).output().unwrap();
    assert_eq!(lower.status.code(), Some(1), "{}", said(&lower));
}

/// Starts that cannot be checked are not a pass: exit 3.
#[test]
fn starts_that_cannot_be_checked_are_reported_as_such() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "oldstart-unreadable");
    start_record_from(&scratch, 7);
    let dir = state(&scratch).join("codex-nosave-starts");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let output = report(&scratch);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", said(&output));
    assert!(stdout(&output).starts_with("cannot check the no-save Codex starts"), "{}", said(&output));
}
