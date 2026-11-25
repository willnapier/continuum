//! A no-save Codex session stays out of Continuum when its hooks do not run
//! (`continuum_core::codex_nosave_start`). `continuum-codex` runs as a real
//! process with a stand-in launcher and a stand-in Codex that runs no hook,
//! in a scratch home; the rollouts and thread ids are invented. The later
//! import is the timer's importer (`import_codex`, which `continuum import -a
//! codex` calls), run on the scratch archive.
//!
//! Each case has its control: the same start without a no-save request, in
//! which the same rollout is stored. `CONTINUUM_CODEX_UNDER_TEST` runs the
//! cases against another build of the wrapper (the red run against the
//! wrapper before this change).
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use continuum_core::import::{import_codex, Imported};
use masked_fixture::{said, Scratch, REQUEST_KEPT, REQUEST_USED, SIGNAL_SELF};
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

const EARLIER_THREAD: &str = "019a0000-0000-7000-8000-0000000000a1";
const NEW_THREAD: &str = "019a0000-0000-7000-8000-0000000000b2";
const OTHER_THREAD: &str = "019a0000-0000-7000-8000-0000000000c3";
const LATER_THREAD: &str = "019a0000-0000-7000-8000-0000000000d4";

/// How the stand-in Codex ends, what the wrapper then exits with, and
/// whether the no-save request is used up.
struct End {
    name: &'static str,
    sh: &'static str,
    wrapper_exit: i32,
    request_used: bool,
}

const ENDS: [End; 5] = [
    End { name: "signal", sh: SIGNAL_SELF, wrapper_exit: 1, request_used: false },
    End { name: "113", sh: "exit 113", wrapper_exit: 113, request_used: false },
    End { name: "126", sh: "exit 126", wrapper_exit: 126, request_used: false },
    End { name: "127", sh: "exit 127", wrapper_exit: 127, request_used: false },
    End { name: "clean", sh: "exit 0", wrapper_exit: 0, request_used: true },
];

fn wrapper() -> String {
    std::env::var("CONTINUUM_CODEX_UNDER_TEST").unwrap_or_else(|_| env!("CARGO_BIN_EXE_continuum-codex").to_string())
}

/// One rollout line the importer would store.
fn line(text: &str) -> String {
    format!(
        r#"{{"type":"response_item","timestamp":"2026-10-07T08:00:00.000Z","payload":{{"role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
    )
}

fn rollout_path(scratch: &Scratch, day: &str, thread: &str) -> PathBuf {
    let dir = scratch.home.join(".codex/sessions").join(day);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("rollout-{}T08-00-00-{thread}.jsonl", day.replace('/', "-")))
}

/// A rollout with one invented turn.
fn existing_rollout(scratch: &Scratch, day: &str, thread: &str) -> PathBuf {
    let path = rollout_path(scratch, day, thread);
    std::fs::write(&path, format!("{}\n", line(&format!("zz99 earlier turn of {thread}")))).unwrap();
    path
}

fn stem(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

fn registry(scratch: &Scratch) -> PathBuf {
    scratch.home.join(".local/share/continuum/nosave-sessions")
}

fn logs(scratch: &Scratch) -> PathBuf {
    scratch.home.join("Assistants/continuum-logs")
}

fn starts(scratch: &Scratch) -> Vec<String> {
    names(&scratch.home.join(".local/share/continuum/codex-nosave-starts"))
}

/// The names in `dir`, sorted; empty when it is not there.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

#[derive(Debug, PartialEq, Eq)]
enum Later {
    Stored(usize),
    NoSave,
    Held,
}

/// The timer's import of one rollout, on the scratch archive.
fn import(scratch: &Scratch, rollout: &Path) -> Later {
    let writer = continuum_core::PlainTextWriter::with_base_dir(logs(scratch)).with_nosave_dir(registry(scratch));
    match import_codex(&writer, rollout).unwrap() {
        Imported::Stored((outcome, _)) => Later::Stored(outcome.message_count),
        Imported::NoSave => Later::NoSave,
        Imported::Held => Later::Held,
        Imported::Empty => panic!("the rollout has a message"),
    }
}

/// The stored copy of a rollout: its message count, or None.
fn stored_copy(scratch: &Scratch, rollout: &Path) -> Option<usize> {
    let codex = logs(scratch).join("codex");
    names(&codex).into_iter().find_map(|day| {
        let messages = codex.join(day).join(stem(rollout)).join("messages.jsonl");
        std::fs::read_to_string(messages).ok().map(|text| text.lines().count())
    })
}

/// Whether any stored file in the scratch archive holds `text`.
fn archive_holds(scratch: &Scratch, text: &str) -> bool {
    let mut dirs = vec![logs(scratch)];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if std::fs::read_to_string(&path).is_ok_and(|t| t.contains(text)) {
                return true;
            }
        }
    }
    false
}

fn expected_request(end: &End) -> &'static [&'static str] {
    if end.request_used {
        &REQUEST_USED
    } else {
        &REQUEST_KEPT
    }
}

/// sh: wait until `go` exists (at most 30 s), after saying `ready`.
fn pause(scratch: &Scratch) -> String {
    format!(
        ": > '{ready}'\ni=0\nwhile [ ! -e '{go}' ] && [ $i -lt 600 ]; do /usr/bin/sleep 0.05; i=$((i+1)); done\n",
        ready = scratch.root.join("ready").display(),
        go = scratch.root.join("go").display()
    )
}

/// Whether `pid` is running: there, and not a zombie.
fn process_running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| stat.rfind(')').map(|at| stat[at + 1..].trim_start().starts_with('Z')))
        .is_some_and(|zombie| !zombie)
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A wrapper still running; killed and reaped if a test fails first.
struct Running(Option<Child>);

impl Running {
    fn start(scratch: &Scratch, args: &[&str]) -> Self {
        let child = scratch
            .command(&wrapper())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self(Some(child))
    }

    fn finish(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    fn kill(&mut self) {
        let child = self.0.as_mut().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// --- A new rollout, however Codex ends ---------------------------------

/// The stand-in Codex writes a new rollout, then touches the earlier one, so
/// that the newest rollout is the same before and after (the clean exit's
/// old blind spot), then ends as `end` says.
fn run_new_rollout(scratch: &Scratch, no_save: bool, end: &End) -> (Output, PathBuf, PathBuf) {
    let earlier = existing_rollout(scratch, "2026/10/06", EARLIER_THREAD);
    let new = rollout_path(scratch, "2026/10/07", NEW_THREAD);
    scratch.stand_in_launcher();
    scratch.harness(
        "codex",
        &format!(
            "printf '%s\\n' '{}' > '{}'\n/usr/bin/sleep 0.05\n/usr/bin/touch '{}'\n{}",
            line("zz99 new private question"),
            new.display(),
            earlier.display(),
            end.sh
        ),
    );
    if no_save {
        scratch.request_no_save();
    }
    let output = scratch.command(&wrapper()).args(["exec", "ZZ99"]).output().unwrap();
    (output, earlier, new)
}

#[test]
fn a_new_no_save_rollout_without_hooks_is_kept_out_however_codex_ends() {
    for end in &ENDS {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), &format!("nohooks-new-{}", end.name));
        let (output, earlier, new) = run_new_rollout(&scratch, true, end);
        let context = format!("end {}\n{}", end.name, said(&output));

        assert_eq!(output.status.code(), Some(end.wrapper_exit), "{context}");
        assert_eq!(scratch.nosave_seen(), ["nosave:1"], "{context}");
        assert_eq!(scratch.nosave_state(), expected_request(end), "{context}");
        assert_eq!(names(&registry(&scratch)), [stem(&new)], "only the new rollout is registered\n{context}");
        assert_eq!(import(&scratch, &new), Later::NoSave, "{context}");
        assert!(!archive_holds(&scratch, "zz99 new private question"), "{context}");
        assert_eq!(starts(&scratch), Vec::<String>::new(), "the start is closed\n{context}");
        assert_eq!(import(&scratch, &earlier), Later::Stored(1), "an earlier session still imports\n{context}");
    }
}

#[test]
fn control_without_a_no_save_request_the_new_rollout_is_stored() {
    for end in &ENDS {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), &format!("nohooks-new-control-{}", end.name));
        let (output, _, new) = run_new_rollout(&scratch, false, end);
        let context = format!("end {}\n{}", end.name, said(&output));

        assert_eq!(scratch.nosave_seen(), ["nosave:unset"], "{context}");
        assert_eq!(names(&registry(&scratch)), Vec::<String>::new(), "{context}");
        assert_eq!(import(&scratch, &new), Later::Stored(1), "{context}");
    }
}

// --- A resume that appends to an existing rollout ----------------------

/// The stand-in Codex notes whether its rollout was registered before it
/// ran, appends a turn to it, and ends as `end` says.
fn run_resume(scratch: &Scratch, no_save: bool, end: &End) -> (Output, PathBuf) {
    let resumed = existing_rollout(scratch, "2026/10/06", EARLIER_THREAD);
    assert_eq!(import(scratch, &resumed), Later::Stored(1), "saved before the resume");
    scratch.stand_in_launcher();
    scratch.harness(
        "codex",
        &format!(
            "if [ -e '{marker}' ]; then : > '{seen}'; fi\nprintf '%s\\n' '{turn}' >> '{rollout}'\n{end}",
            marker = registry(scratch).join(stem(&resumed)).display(),
            seen = scratch.root.join("registered-before-codex").display(),
            turn = line("zz99 appended private turn"),
            rollout = resumed.display(),
            end = end.sh
        ),
    );
    if no_save {
        scratch.request_no_save();
    }
    let output = scratch.command(&wrapper()).args(["resume", EARLIER_THREAD]).output().unwrap();
    (output, resumed)
}

#[test]
fn a_no_save_resume_without_hooks_keeps_the_appended_turns_out_however_codex_ends() {
    for end in &ENDS {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), &format!("nohooks-resume-{}", end.name));
        let (output, resumed) = run_resume(&scratch, true, end);
        let context = format!("end {}\n{}", end.name, said(&output));

        assert_eq!(output.status.code(), Some(end.wrapper_exit), "{context}");
        assert!(scratch.root.join("registered-before-codex").exists(), "registered before Codex ran\n{context}");
        assert_eq!(scratch.nosave_state(), expected_request(end), "{context}");
        // Sorted: by thread id, and by the rollout file.
        assert_eq!(names(&registry(&scratch)), [EARLIER_THREAD.to_string(), stem(&resumed)], "{context}");
        assert_eq!(import(&scratch, &resumed), Later::NoSave, "{context}");
        assert!(!archive_holds(&scratch, "zz99 appended private turn"), "{context}");
        assert_eq!(stored_copy(&scratch, &resumed), Some(1), "the turns saved before are kept\n{context}");
    }
}

#[test]
fn control_without_a_no_save_request_the_appended_turns_are_stored() {
    for end in &ENDS {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), &format!("nohooks-resume-control-{}", end.name));
        let (output, resumed) = run_resume(&scratch, false, end);
        let context = format!("end {}\n{}", end.name, said(&output));

        assert!(!scratch.root.join("registered-before-codex").exists(), "{context}");
        assert_eq!(names(&registry(&scratch)), Vec::<String>::new(), "{context}");
        assert_eq!(import(&scratch, &resumed), Later::Stored(2), "{context}");
    }
}

/// A no-save resume that names no thread id cannot be kept out by file, so
/// Codex is not started, and the request is kept. The control: naming it.
#[test]
fn a_no_save_resume_that_names_no_session_is_refused() {
    for args in [
        &["resume", "--last"][..],
        &["resume"][..],
        &["resume", "ZZ99"][..],
        // A thread name with an unrelated thread id as its prompt.
        &["resume", "zz99-thread-name", OTHER_THREAD][..],
        // An option before the selector, which may take a value.
        &["resume", "--all", EARLIER_THREAD][..],
        &["resume", EARLIER_THREAD, OTHER_THREAD][..],
        // An option after the id, which may choose another session.
        &["resume", EARLIER_THREAD, "--last"][..],
        &["exec", "resume", EARLIER_THREAD, "--last"][..],
    ] {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "nohooks-resume-unnamed");
        scratch.stand_in_launcher();
        scratch.harness("codex", "exit 0");
        scratch.request_no_save();
        let output = scratch.command(&wrapper()).args(args).output().unwrap();
        let context = format!("{args:?}\n{}", said(&output));

        assert_eq!(output.status.code(), Some(2), "{context}");
        assert_eq!(scratch.log(), Vec::<String>::new(), "Codex is not started\n{context}");
        assert_eq!(scratch.nosave_state(), REQUEST_KEPT, "{context}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("must name the session's id"), "{context}");
    }

    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "nohooks-resume-named");
    scratch.stand_in_launcher();
    scratch.harness("codex", "exit 0");
    scratch.request_no_save();
    let output = scratch.command(&wrapper()).args(["resume", EARLIER_THREAD]).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(scratch.nosave_state(), REQUEST_USED, "{}", said(&output));
}

// --- The timer between start and exit ----------------------------------

/// A resumed Codex that also begins a new session (`/new`), with the timer's
/// import run while it is still running. A third rollout belongs to another
/// session that was already running.
struct Midway {
    resumed: PathBuf,
    new: PathBuf,
    other: PathBuf,
}

fn start_midway(scratch: &Scratch, no_save: bool, end: &End) -> (Running, Midway) {
    let resumed = existing_rollout(scratch, "2026/10/06", EARLIER_THREAD);
    assert_eq!(import(scratch, &resumed), Later::Stored(1));
    let other = existing_rollout(scratch, "2026/10/06", OTHER_THREAD);
    let new = rollout_path(scratch, "2026/10/07", NEW_THREAD);
    scratch.stand_in_launcher();
    scratch.harness(
        "codex",
        &format!(
            "printf '%s\\n' '{turn}' >> '{resumed}'\nprintf '%s\\n' '{first}' > '{new}'\n{pause}{end}",
            turn = line("zz99 appended private turn"),
            resumed = resumed.display(),
            first = line("zz99 new private question"),
            new = new.display(),
            pause = pause(scratch),
            end = end.sh
        ),
    );
    if no_save {
        scratch.request_no_save();
    }
    let running = Running::start(scratch, &["resume", EARLIER_THREAD]);
    wait_for("the stand-in Codex", || scratch.root.join("ready").exists());
    (running, Midway { resumed, new, other })
}

#[test]
fn the_timer_between_start_and_exit_stores_nothing_of_a_no_save_session() {
    for end in &ENDS {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), &format!("nohooks-midway-{}", end.name));
        let (running, rollouts) = start_midway(&scratch, true, end);

        // The timer runs while Codex is still running.
        let new_then = import(&scratch, &rollouts.new);
        let resumed_then = import(&scratch, &rollouts.resumed);
        // Another session's turns written meanwhile are stored as usual.
        std::fs::write(&rollouts.other, format!("{0}\n{0}\n", line("zz99 another session's turn"))).unwrap();
        let other_then = import(&scratch, &rollouts.other);
        let held_back = !archive_holds(&scratch, "zz99 new private question")
            && !archive_holds(&scratch, "zz99 appended private turn");

        std::fs::write(scratch.root.join("go"), b"").unwrap();
        let output = running.finish();
        let context = format!("end {}\n{}", end.name, said(&output));
        assert_eq!(new_then, Later::Held, "{context}");
        assert_eq!(resumed_then, Later::NoSave, "{context}");
        assert_eq!(other_then, Later::Stored(2), "{context}");
        assert!(held_back, "nothing of the no-save session was stored midway\n{context}");

        assert_eq!(output.status.code(), Some(end.wrapper_exit), "{context}");
        assert_eq!(import(&scratch, &rollouts.new), Later::NoSave, "{context}");
        assert_eq!(import(&scratch, &rollouts.resumed), Later::NoSave, "{context}");
        assert_eq!(stored_copy(&scratch, &rollouts.resumed), Some(1), "{context}");
        assert!(!archive_holds(&scratch, "zz99 new private question"), "{context}");
        assert!(!archive_holds(&scratch, "zz99 appended private turn"), "{context}");
        assert_eq!(starts(&scratch), Vec::<String>::new(), "{context}");
    }
}

#[test]
fn control_without_a_no_save_request_the_timer_stores_the_session_midway() {
    let end = &ENDS[4];
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "nohooks-midway-control");
    let (running, rollouts) = start_midway(&scratch, false, end);
    let new_then = import(&scratch, &rollouts.new);
    let resumed_then = import(&scratch, &rollouts.resumed);
    std::fs::write(scratch.root.join("go"), b"").unwrap();
    let output = running.finish();
    assert_eq!(new_then, Later::Stored(1), "{}", said(&output));
    assert_eq!(resumed_then, Later::Stored(2), "{}", said(&output));
}

// --- The wrapper killed while Codex runs --------------------------------

fn start_codex_that_outlives_its_wrapper(scratch: &Scratch, no_save: bool) -> (Running, PathBuf, u32) {
    let new = rollout_path(scratch, "2026/10/07", NEW_THREAD);
    scratch.stand_in_launcher();
    scratch.harness(
        "codex",
        &format!(
            // The mask is read by builtins only: inside a command
            // substitution bash holds SIGINT back itself.
            "printf '%s\\n' $$ > '{pid}'\n\
             while read -r key value; do [ \"$key\" = SigBlk: ] && printf '%s\\n' \"$key $value\" > '{blk}'; done < /proc/$$/status\n\
             printf '%s\\n' '{first}' > '{new}'\n{pause}exit 0",
            pid = scratch.root.join("codex.pid").display(),
            blk = scratch.root.join("codex.sigblk").display(),
            first = line("zz99 new private question"),
            new = new.display(),
            pause = pause(scratch)
        ),
    );
    if no_save {
        scratch.request_no_save();
    }
    let running = Running::start(scratch, &["exec", "ZZ99"]);
    wait_for("the stand-in Codex", || scratch.root.join("ready").exists());
    let pid = std::fs::read_to_string(scratch.root.join("codex.pid")).unwrap().trim().parse().unwrap();
    (running, new, pid)
}

#[test]
fn a_killed_wrapper_leaves_its_start_open_until_codex_ends_and_the_next_import_closes_it() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "nohooks-killed");
    let (mut running, new, pid) = start_codex_that_outlives_its_wrapper(&scratch, true);

    // As when its terminal tab is closed, but with no chance to clean up.
    running.kill();
    let while_codex_runs = import(&scratch, &new);
    std::fs::write(scratch.root.join("go"), b"").unwrap();
    wait_for("the stand-in Codex to end", || !process_running(pid));

    assert_eq!(while_codex_runs, Later::Held, "held while Codex outlives its wrapper");
    assert_eq!(import(&scratch, &new), Later::NoSave, "closed by this import, which registers it");
    assert_eq!(starts(&scratch), Vec::<String>::new());
    assert!(!archive_holds(&scratch, "zz99 new private question"));
    // Once closed, nothing is held: a session begun after it is stored.
    let later = existing_rollout(&scratch, "2026/10/08", LATER_THREAD);
    assert_eq!(import(&scratch, &later), Later::Stored(1));
    // Codex started with no signal held back from it.
    let blocked = std::fs::read_to_string(scratch.root.join("codex.sigblk")).unwrap();
    assert_eq!(blocked.split_whitespace().nth(1), Some("0000000000000000"), "{blocked}");
}

#[test]
fn control_without_a_no_save_request_a_killed_wrappers_session_is_stored() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "nohooks-killed-control");
    let (mut running, new, pid) = start_codex_that_outlives_its_wrapper(&scratch, false);
    running.kill();
    let while_codex_runs = import(&scratch, &new);
    std::fs::write(scratch.root.join("go"), b"").unwrap();
    wait_for("the stand-in Codex to end", || !process_running(pid));
    assert_eq!(while_codex_runs, Later::Stored(1));
}

// --- A copy stored before the registration ------------------------------

/// The stand-in Codex writes a new rollout, and a stored copy of it appears
/// meanwhile, written by something that did not honour the start (an older
/// importer). An unrelated session's stored copy is beside it.
fn run_with_a_stored_copy(scratch: &Scratch, no_save: bool, end: &End) -> (Output, PathBuf, PathBuf) {
    let earlier = existing_rollout(scratch, "2026/10/06", EARLIER_THREAD);
    assert_eq!(import(scratch, &earlier), Later::Stored(1));
    let new = rollout_path(scratch, "2026/10/07", NEW_THREAD);
    let copy = logs(scratch).join("codex/2026-10-07").join(stem(&new));
    scratch.stand_in_launcher();
    scratch.harness(
        "codex",
        &format!(
            "printf '%s\\n' '{first}' > '{new}'\n/usr/bin/mkdir -p '{copy}'\nprintf '%s\\n' '{{}}' > '{copy}/messages.jsonl'\n{end}",
            first = line("zz99 new private question"),
            new = new.display(),
            copy = copy.display(),
            end = end.sh
        ),
    );
    if no_save {
        scratch.request_no_save();
    }
    let output = scratch.command(&wrapper()).args(["exec", "ZZ99"]).output().unwrap();
    (output, earlier, copy)
}

#[test]
fn closing_a_no_save_start_removes_a_copy_stored_before_its_registration() {
    for end in &ENDS {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), &format!("nohooks-copy-{}", end.name));
        let (output, earlier, copy) = run_with_a_stored_copy(&scratch, true, end);
        let context = format!("end {}\n{}", end.name, said(&output));

        assert!(!copy.exists(), "the stored copy is removed\n{context}");
        assert_eq!(stored_copy(&scratch, &earlier), Some(1), "another session's copy is kept\n{context}");
    }
}

#[test]
fn control_without_a_no_save_request_the_stored_copy_stays() {
    let end = &ENDS[0];
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "nohooks-copy-control");
    let (output, _, copy) = run_with_a_stored_copy(&scratch, false, end);
    assert!(copy.join("messages.jsonl").exists(), "{}", said(&output));
}

// --- The close log ------------------------------------------------------

fn close_log_path(scratch: &Scratch) -> PathBuf {
    scratch.home.join(".local/share/continuum/codex-nosave-closes.jsonl")
}

/// Each entry of the close log; none when there is no log.
fn close_log(scratch: &Scratch) -> Vec<serde_json::Value> {
    std::fs::read_to_string(close_log_path(scratch))
        .map(|text| text.lines().map(|l| serde_json::from_str(l).unwrap()).collect())
        .unwrap_or_default()
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

/// A close by the wrapper is logged: the closer, the start record, the
/// rollout marked by file stem, the stored copy removed by path. Red against
/// the wrapper before this change, which writes no log.
#[test]
fn a_close_by_the_wrapper_is_logged_with_its_stems_and_removed_paths() {
    for end in [&ENDS[0], &ENDS[4]] {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), &format!("closelog-wrapper-{}", end.name));
        let (output, earlier, copy) = run_with_a_stored_copy(&scratch, true, end);
        let context = format!("end {}\n{}", end.name, said(&output));
        let new = rollout_path(&scratch, "2026/10/07", NEW_THREAD);

        let log = close_log(&scratch);
        assert_eq!(log.len(), 1, "one entry\n{log:?}\n{context}");
        let entry = &log[0];
        assert_eq!(entry["closer"], "wrapper", "{context}");
        assert!(entry["start"].as_str().is_some_and(|s| s.ends_with(".start")), "{entry}\n{context}");
        assert_eq!(strings(&entry["marked"]), [stem(&new)], "{context}");
        assert_eq!(strings(&entry["removed"]), [copy.to_string_lossy().into_owned()], "{context}");
        assert!(entry["failed"].is_null(), "{context}");
        assert!(entry["time"].as_str().is_some_and(|t| t.starts_with("20")), "{context}");
        // Names and paths only.
        let text = std::fs::read_to_string(close_log_path(&scratch)).unwrap();
        assert!(!text.contains("zz99"), "no session content in the log\n{context}");
        assert_eq!(stored_copy(&scratch, &earlier), Some(1), "{context}");
    }
}

/// The control: a start without a no-save request writes no close log.
#[test]
fn control_without_a_no_save_request_nothing_is_logged() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "closelog-wrapper-control");
    let (output, _, _) = run_with_a_stored_copy(&scratch, false, &ENDS[4]);
    assert_eq!(close_log(&scratch), Vec::<serde_json::Value>::new(), "{}", said(&output));
}

/// A close that marks nothing (Codex wrote no rollout) is logged too.
#[test]
fn a_close_that_marks_nothing_is_still_logged() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "closelog-nothing");
    existing_rollout(&scratch, "2026/10/06", EARLIER_THREAD);
    scratch.stand_in_launcher();
    scratch.harness("codex", "exit 0");
    scratch.request_no_save();
    let output = scratch.command(&wrapper()).args(["exec", "ZZ99"]).output().unwrap();
    let context = said(&output);

    assert_eq!(output.status.code(), Some(0), "{context}");
    let log = close_log(&scratch);
    assert_eq!(log.len(), 1, "{log:?}\n{context}");
    assert_eq!(log[0]["closer"], "wrapper", "{context}");
    assert_eq!(strings(&log[0]["marked"]), Vec::<String>::new(), "{context}");
    assert_eq!(strings(&log[0]["removed"]), Vec::<String>::new(), "{context}");
    assert_eq!(names(&registry(&scratch)), Vec::<String>::new(), "nothing marked\n{context}");
}

/// A close log that cannot be written (a folder where the file should be)
/// stops neither the marking nor the removal, and the wrapper says so.
#[test]
fn a_log_that_cannot_be_written_does_not_stop_the_wrapper_close() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "closelog-wrapper-unwritable");
    std::fs::create_dir_all(close_log_path(&scratch)).unwrap();
    let (output, earlier, copy) = run_with_a_stored_copy(&scratch, true, &ENDS[4]);
    let context = said(&output);
    let new = rollout_path(&scratch, "2026/10/07", NEW_THREAD);

    assert_eq!(output.status.code(), Some(0), "Codex's own status\n{context}");
    assert_eq!(names(&registry(&scratch)), [stem(&new)], "marked\n{context}");
    assert!(!copy.exists(), "removed\n{context}");
    assert_eq!(starts(&scratch), Vec::<String>::new(), "closed\n{context}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("the close is not in the close log"),
        "the failure is shown\n{context}"
    );
    assert_eq!(stored_copy(&scratch, &earlier), Some(1), "{context}");
}
