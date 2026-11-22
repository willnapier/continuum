//! `continuum-goose` run as a real process: on Linux it starts Goose through
//! `ai-masked`, without the launcher it starts nothing, and a start that
//! reaches no Goose keeps the no-save request
//! (`~/Assistants/shared/AI-FOOTING-MASK.md`). Stand-in programs only; see
//! the fixture.
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use masked_fixture::{said, Scratch};

const WRAPPER: &str = env!("CARGO_BIN_EXE_continuum-goose");
const ARGS: [&str; 3] = ["session", "--name", "ZZ99 two words"];

/// None when the scratch path would make the wrapper take the stand-in for
/// itself (it treats any path containing its own name as the wrapper, and
/// would then look for a real Goose in the standard places).
fn scratch(name: &str) -> Option<Scratch> {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), name);
    if scratch.root.to_string_lossy().contains("continuum-goose") {
        eprintln!("skipped: the target folder's path contains `continuum-goose`");
        return None;
    }
    Some(scratch)
}

#[test]
fn goose_starts_through_the_launcher_with_the_arguments_unchanged() {
    let Some(scratch) = scratch("goose-wrapper") else { return };
    scratch.stand_in_launcher();
    let goose = scratch.harness("goose", "exit 7");

    let output = scratch.command(WRAPPER).args(ARGS).output().unwrap();

    assert_eq!(output.status.code(), Some(7), "goose's exit code comes back\n{}", said(&output));
    // The wrapper resolves the real binary's path before it starts it.
    let resolved = std::fs::canonicalize(&goose).unwrap();
    assert_eq!(scratch.log(), Scratch::masked_log(&resolved, "goose", &ARGS, "unset", "unset"));
}

#[test]
fn without_the_launcher_goose_is_not_started() {
    let Some(scratch) = scratch("goose-wrapper-refuse") else { return };
    scratch.harness("goose", "exit 0");
    scratch.request_no_save();

    let output = scratch.command(WRAPPER).args(ARGS).output().unwrap();

    assert!(!output.status.success(), "a missing launcher is a failure\n{}", said(&output));
    assert_eq!(scratch.log(), Vec::<String>::new(), "goose must not be started directly");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("only through `ai-masked`"), "{stderr}");
    assert!(stderr.contains("rust-redeploy --only ai-masked"), "{stderr}");
    assert_eq!(scratch.nosave_state(), masked_fixture::REQUEST_KEPT);
}

#[test]
fn a_refusing_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("goose-nosave-refused") else { return };
    masked_fixture::a_refusing_launcher_keeps_the_request(&scratch, WRAPPER, "goose", &ARGS);
}

#[test]
fn an_unrunnable_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("goose-nosave-unrunnable") else { return };
    masked_fixture::an_unrunnable_launcher_keeps_the_request(&scratch, WRAPPER, "goose", &ARGS);
}

/// This wrapper does not pass the request on in the environment (Goose has
/// no Continuum hooks); it only skips its own import at exit.
#[test]
fn a_started_session_uses_the_no_save_request_up() {
    let Some(scratch) = scratch("goose-nosave-started") else { return };
    masked_fixture::a_started_session_uses_the_request_up(
        &scratch, WRAPPER, "goose", "exit 0", &ARGS, "nosave:unset",
    );
}
