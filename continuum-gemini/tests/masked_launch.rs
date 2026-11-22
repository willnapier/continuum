//! `continuum-gemini` run as a real process: on Linux it starts Gemini
//! through `ai-masked`, without the launcher it starts nothing, and a start
//! that reaches no Gemini keeps the no-save request
//! (`~/Assistants/shared/AI-FOOTING-MASK.md`). Stand-in programs only; see
//! the fixture.
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use masked_fixture::{said, Scratch};

const WRAPPER: &str = env!("CARGO_BIN_EXE_continuum-gemini");
const ARGS: [&str; 3] = ["--model", "two words", "ZZ99"];

/// None when the scratch path would make the wrapper take the stand-in for
/// itself (it treats any path containing its own name as the wrapper, and
/// would then look for a real Gemini in the standard places).
fn scratch(name: &str) -> Option<Scratch> {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), name);
    if scratch.root.to_string_lossy().contains("continuum-gemini") {
        eprintln!("skipped: the target folder's path contains `continuum-gemini`");
        return None;
    }
    Some(scratch)
}

#[test]
fn gemini_starts_through_the_launcher_with_the_arguments_unchanged() {
    let Some(scratch) = scratch("gemini-wrapper") else { return };
    scratch.stand_in_launcher();
    let gemini = scratch.harness("gemini", "exit 7");

    let output = scratch.command(WRAPPER).args(ARGS).output().unwrap();

    assert_eq!(output.status.code(), Some(7), "gemini's exit code comes back\n{}", said(&output));
    // The wrapper resolves the real binary's path before it starts it.
    let resolved = std::fs::canonicalize(&gemini).unwrap();
    assert_eq!(scratch.log(), Scratch::masked_log(&resolved, "gemini", &ARGS, "unset", "unset"));
}

#[test]
fn without_the_launcher_gemini_is_not_started() {
    let Some(scratch) = scratch("gemini-wrapper-refuse") else { return };
    scratch.harness("gemini", "exit 0");
    scratch.request_no_save();

    let output = scratch.command(WRAPPER).args(ARGS).output().unwrap();

    assert!(!output.status.success(), "a missing launcher is a failure\n{}", said(&output));
    assert_eq!(scratch.log(), Vec::<String>::new(), "gemini must not be started directly");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("only through `ai-masked`"), "{stderr}");
    assert!(stderr.contains("rust-redeploy --only ai-masked"), "{stderr}");
    assert_eq!(scratch.nosave_state(), masked_fixture::REQUEST_KEPT);
}

#[test]
fn a_refusing_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("gemini-nosave-refused") else { return };
    masked_fixture::a_refusing_launcher_keeps_the_request(&scratch, WRAPPER, "gemini", &ARGS);
}

#[test]
fn an_unrunnable_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("gemini-nosave-unrunnable") else { return };
    masked_fixture::an_unrunnable_launcher_keeps_the_request(&scratch, WRAPPER, "gemini", &ARGS);
}

/// This wrapper does not pass the request on in the environment (Gemini has
/// no Continuum hooks); it only skips its own import at exit.
#[test]
fn a_started_session_uses_the_no_save_request_up() {
    let Some(scratch) = scratch("gemini-nosave-started") else { return };
    masked_fixture::a_started_session_uses_the_request_up(
        &scratch, WRAPPER, "gemini", "exit 0", &ARGS, "nosave:unset",
    );
}

/// A signal that ends the launcher before it becomes Gemini: nothing ran, so
/// the request is kept.
#[test]
fn a_signalled_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("gemini-nosave-signalled") else { return };
    masked_fixture::a_signalled_launcher_keeps_the_request(&scratch, WRAPPER, "gemini", &ARGS);
}

/// This wrapper cannot tie a session to its child, so a Gemini that ran and
/// was then ended by a signal gets its request back: the next session is not
/// saved either. That errs towards not saving.
#[test]
fn a_signalled_gemini_gets_the_no_save_request_back() {
    let Some(scratch) = scratch("gemini-nosave-signalled-harness") else { return };
    masked_fixture::a_signalled_harness_that_was_not_seen_gets_the_request_back(
        &scratch, WRAPPER, "gemini", masked_fixture::SIGNAL_SELF, &ARGS, "nosave:unset",
    );
}

/// The launcher's failed exec (127: not found; 126: would not run) is a start
/// that reached nothing, like its refusal.
#[test]
fn a_launcher_that_cannot_run_gemini_keeps_the_no_save_request() {
    for code in [126, 127] {
        let Some(scratch) = scratch(&format!("gemini-nosave-exec-{code}")) else { return };
        masked_fixture::a_launcher_that_cannot_run_the_harness_keeps_the_request(
            &scratch, WRAPPER, "gemini", &ARGS, code,
        );
    }
}
