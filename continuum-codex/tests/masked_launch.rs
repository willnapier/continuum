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
