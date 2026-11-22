//! `usage-probe-codex-smoke` run as a real process: on Linux its one request
//! starts Codex through `ai-masked`, and without the launcher it starts
//! nothing (`~/Assistants/shared/AI-FOOTING-MASK.md`). Stand-in programs
//! only; see the fixture.
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use masked_fixture::{said, Scratch};

const PROBE: &str = env!("CARGO_BIN_EXE_usage-probe-codex-smoke");

#[test]
fn the_request_starts_codex_through_the_launcher() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-smoke");
    scratch.stand_in_launcher();
    let codex = scratch.harness("codex", "printf '%s\\n' OK");

    let output = scratch.command(PROBE).output().unwrap();

    assert!(output.status.success(), "the stand-in's reply is read through the launcher\n{}", said(&output));
    let log = scratch.log();
    assert_eq!(log[..2], ["launcher".to_string(), format!("arg:{}", codex.display())], "{log:?}");
    let harness_at = log.iter().position(|line| line == "harness:codex").expect("codex ran");
    // The launcher was given exactly the arguments codex then received.
    let received = &log[harness_at + 1..log.len() - 2];
    assert_eq!(&log[2..harness_at], received, "{log:?}");
    assert_eq!(received[0], "arg:exec", "{log:?}");
}

#[test]
fn without_the_launcher_codex_is_not_started() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-smoke-refuse");
    scratch.harness("codex", "printf '%s\\n' OK");

    let output = scratch.command(PROBE).output().unwrap();

    assert!(!output.status.success(), "a missing launcher is a failed reading\n{}", said(&output));
    assert_eq!(scratch.log(), Vec::<String>::new(), "codex must not be started directly");
    let envelope = String::from_utf8_lossy(&output.stdout);
    assert!(envelope.contains("only through `ai-masked`"), "{envelope}");
}
