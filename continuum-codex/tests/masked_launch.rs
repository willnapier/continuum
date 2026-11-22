//! `continuum-codex` run as a real process: on Linux it starts Codex through
//! `ai-masked`, and without the launcher it starts nothing
//! (`~/Assistants/shared/AI-FOOTING-MASK.md`). Stand-in programs only; see
//! the fixture.
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use masked_fixture::{said, Scratch};

const WRAPPER: &str = env!("CARGO_BIN_EXE_continuum-codex");
const ARGS: [&str; 2] = ["resume", "ZZ99"];

// A no-save Codex session that ended by a signal or with one of the
// launcher's codes, and whose hooks never ran. The wrapper cannot tell which
// session was its own, so it registers every rollout made since it started
// as no-save, and the later import (the timer's importer) refuses them. A
// rollout that was there before is not registered and still imports.

const EARLIER_THREAD: &str = "019a0000-0000-7000-8000-0000000000a1";
const NEW_THREAD: &str = "019a0000-0000-7000-8000-0000000000b2";
/// One rollout line the importer would store.
const ROLLOUT_LINE: &str = r#"{"type":"response_item","timestamp":"2026-10-07T08:00:00.000Z","payload":{"role":"user","content":[{"type":"input_text","text":"zz99 private question"}]}}"#;

struct Ended {
    output: std::process::Output,
    /// A rollout that was there before the start.
    earlier: std::path::PathBuf,
    /// The rollout the stand-in Codex wrote.
    new: std::path::PathBuf,
}

fn rollout_path(scratch: &Scratch, day: &str, thread: &str) -> std::path::PathBuf {
    let dir = scratch.home.join(".codex/sessions").join(day);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("rollout-{}T08-00-00-{thread}.jsonl", day.replace('/', "-")))
}

/// Start a stand-in Codex that writes a new rollout, runs no hook, and then
/// ends as `end` (sh) says.
fn start_codex_that_writes_a_rollout(scratch: &Scratch, no_save: bool, end: &str) -> Ended {
    let earlier = rollout_path(scratch, "2026/10/06", EARLIER_THREAD);
    std::fs::write(&earlier, format!("{ROLLOUT_LINE}\n")).unwrap();
    let new = rollout_path(scratch, "2026/10/07", NEW_THREAD);
    scratch.stand_in_launcher();
    scratch.harness("codex", &format!("printf '%s\\n' '{ROLLOUT_LINE}' > '{}'\n{end}", new.display()));
    if no_save {
        scratch.request_no_save();
    }
    let output = scratch.command(WRAPPER).args(["exec", "ZZ99"]).output().unwrap();
    Ended { output, earlier, new }
}

fn no_save_registry(scratch: &Scratch) -> std::path::PathBuf {
    scratch.home.join(".local/share/continuum/nosave-sessions")
}

/// The ids in the no-save registry, sorted. Empty when there is no registry.
fn registered(scratch: &Scratch) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(no_save_registry(scratch))
        .map(|entries| entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    ids.sort();
    ids
}

/// The later import of one rollout, as the timer does it: whether it was
/// stored.
fn a_later_import_stores(scratch: &Scratch, rollout: &std::path::Path) -> bool {
    use continuum_core::import::{import_codex, Imported};
    let writer = continuum_core::PlainTextWriter::with_base_dir(scratch.home.join("Assistants/continuum-logs"))
        .with_nosave_dir(no_save_registry(scratch));
    match import_codex(&writer, rollout).unwrap() {
        Imported::Stored(_) => true,
        Imported::NoSave => false,
        Imported::Empty => panic!("the rollout has a message"),
    }
}

/// What a no-save start that ended without showing its session must leave.
fn assert_the_new_rollout_is_kept_out(scratch: &Scratch, ended: &Ended) {
    let output = &ended.output;
    assert_eq!(scratch.nosave_seen(), ["nosave:1"], "{}", said(output));
    assert_eq!(scratch.nosave_state(), masked_fixture::REQUEST_KEPT, "{}", said(output));
    let new_stem = ended.new.file_stem().unwrap().to_string_lossy().into_owned();
    assert_eq!(registered(scratch), [new_stem], "only the rollout made since the start\n{}", said(output));
    // Nothing is deleted or changed, the earlier rollout least of all.
    for rollout in [&ended.new, &ended.earlier] {
        assert_eq!(std::fs::read_to_string(rollout).unwrap(), format!("{ROLLOUT_LINE}\n"));
    }
    assert!(!a_later_import_stores(scratch, &ended.new), "the no-save session must be refused");
    assert!(!scratch.home.join("Assistants/continuum-logs/codex").exists(), "nothing of it is in Continuum");
    assert!(a_later_import_stores(scratch, &ended.earlier), "an unrelated earlier session still imports");
}

#[test]
fn a_signalled_no_save_codex_session_without_hooks_is_kept_out_of_a_later_import() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-nosave-rollout-signalled");
    let ended = start_codex_that_writes_a_rollout(&scratch, true, masked_fixture::SIGNAL_SELF);
    assert_eq!(ended.output.status.code(), Some(1), "{}", said(&ended.output));
    assert_the_new_rollout_is_kept_out(&scratch, &ended);
}

#[test]
fn a_no_save_codex_session_that_exits_113_without_hooks_is_kept_out_of_a_later_import() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-nosave-rollout-113");
    let ended = start_codex_that_writes_a_rollout(&scratch, true, "exit 113");
    assert_eq!(ended.output.status.code(), Some(113), "{}", said(&ended.output));
    assert_the_new_rollout_is_kept_out(&scratch, &ended);
}

/// The control: with no no-save request nothing is registered, and the same
/// session is stored by the later import.
#[test]
fn without_a_no_save_request_a_signalled_codex_session_is_not_registered() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-rollout-signalled");
    let ended = start_codex_that_writes_a_rollout(&scratch, false, masked_fixture::SIGNAL_SELF);
    assert_eq!(ended.output.status.code(), Some(1), "{}", said(&ended.output));
    assert_eq!(scratch.nosave_seen(), ["nosave:unset"], "{}", said(&ended.output));
    assert_eq!(registered(&scratch), Vec::<String>::new(), "{}", said(&ended.output));
    assert!(a_later_import_stores(&scratch, &ended.new), "an ordinary session is stored");
}

// The no-save request (`~/.continuum-nosave`) across a start that reaches no
// Codex: it must still be there afterwards. The two failures, and the control
// (a start that works uses the request up).

#[test]
fn a_refusing_launcher_keeps_the_no_save_request() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-nosave-refused");
    masked_fixture::a_refusing_launcher_keeps_the_request(&scratch, WRAPPER, "codex", &ARGS);
}

#[test]
fn an_unrunnable_launcher_keeps_the_no_save_request() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-nosave-unrunnable");
    masked_fixture::an_unrunnable_launcher_keeps_the_request(&scratch, WRAPPER, "codex", &ARGS);
}

#[test]
fn a_started_session_uses_the_no_save_request_up() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-nosave-started");
    masked_fixture::a_started_session_uses_the_request_up(
        &scratch, WRAPPER, "codex", "exit 0", &ARGS, "nosave:1",
    );
}

/// A signal that ends the launcher before it becomes Codex: nothing ran, so
/// the request is kept, and an earlier session's file is left alone.
#[test]
fn a_signalled_launcher_keeps_the_no_save_request() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-nosave-signalled");
    let earlier = scratch.home.join(".codex/sessions/2026/01/01/rollout-zz99-earlier.jsonl");
    std::fs::create_dir_all(earlier.parent().unwrap()).unwrap();
    std::fs::write(&earlier, b"{}\n").unwrap();

    masked_fixture::a_signalled_launcher_keeps_the_request(&scratch, WRAPPER, "codex", &ARGS);

    assert_eq!(std::fs::read(&earlier).ok().as_deref(), Some(&b"{}\n"[..]));
}

/// This wrapper cannot tie a session to its child, so a Codex that ran and
/// was then ended by a signal gets its request back: the next session is not
/// saved either. That errs towards not saving.
#[test]
fn a_signalled_codex_gets_the_no_save_request_back() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-nosave-signalled-harness");
    masked_fixture::a_signalled_harness_that_was_not_seen_gets_the_request_back(
        &scratch, WRAPPER, "codex", masked_fixture::SIGNAL_SELF, &ARGS, "nosave:1",
    );
}

/// The launcher's failed exec (127: not found; 126: would not run) is a start
/// that reached nothing, like its refusal.
#[test]
fn a_launcher_that_cannot_run_codex_keeps_the_no_save_request() {
    for code in [126, 127] {
        let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), &format!("codex-nosave-exec-{code}"));
        masked_fixture::a_launcher_that_cannot_run_the_harness_keeps_the_request(
            &scratch, WRAPPER, "codex", &ARGS, code,
        );
    }
}

#[test]
fn codex_starts_through_the_launcher_with_the_depth_guard_set() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-wrapper");
    scratch.stand_in_launcher();
    // Found on the scratch PATH by the shared resolver, as a real Codex is.
    let codex = scratch.harness("codex", "exit 7");
    let args = ["resume", "ZZ99", "--no-alt-screen"];

    let output = scratch.command(WRAPPER).args(args).output().unwrap();

    assert_eq!(output.status.code(), Some(7), "codex's exit code comes back\n{}", said(&output));
    assert_eq!(scratch.log(), Scratch::masked_log(&codex, "codex", &args, "unset", "1"));
}

#[test]
fn without_the_launcher_codex_is_not_started() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-wrapper-refuse");
    scratch.harness("codex", "exit 0");
    let marker = scratch.home.join(".continuum-nosave");
    std::fs::write(&marker, b"").unwrap();

    let output = scratch.command(WRAPPER).args(["resume", "ZZ99"]).output().unwrap();

    assert!(!output.status.success(), "a missing launcher is a failure\n{}", said(&output));
    assert_eq!(scratch.log(), Vec::<String>::new(), "codex must not be started directly");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("only through `ai-masked`"), "{stderr}");
    assert!(stderr.contains("rust-redeploy --only ai-masked"), "{stderr}");
    assert!(marker.exists(), "a refused start must not use up the no-save marker");
}
