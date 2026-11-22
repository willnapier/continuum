//! `usage-probe-grok-smoke` run as a real process: on Linux its one request
//! starts Grok through `ai-masked`, and without the launcher it starts
//! nothing (`~/Assistants/shared/AI-FOOTING-MASK.md`). Stand-in programs
//! only; see the fixture.
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use masked_fixture::{said, Scratch};

const PROBE: &str = env!("CARGO_BIN_EXE_usage-probe-grok-smoke");
const REPLY: &str = "printf '%s\\n' '{\"text\":\"OK\",\"stopReason\":\"end_turn\"}'";

#[test]
fn the_request_starts_grok_through_the_launcher() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "grok-smoke");
    scratch.stand_in_launcher();
    let grok = scratch.harness("grok", REPLY);

    let output = scratch.command(PROBE).output().unwrap();

    assert!(output.status.success(), "the stand-in's reply is read through the launcher\n{}", said(&output));
    let log = scratch.log();
    assert_eq!(log[..2], ["launcher".to_string(), format!("arg:{}", grok.display())], "{log:?}");
    let harness_at = log.iter().position(|line| line == "harness:grok").expect("grok ran");
    // The launcher was given exactly the arguments grok then received.
    let received = &log[harness_at + 1..log.len() - 2];
    assert_eq!(&log[2..harness_at], received, "{log:?}");
    assert_eq!(received[0], "arg:-p", "{log:?}");
}

#[test]
fn without_the_launcher_grok_is_not_started() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "grok-smoke-refuse");
    scratch.harness("grok", REPLY);

    let output = scratch.command(PROBE).output().unwrap();

    assert!(!output.status.success(), "a missing launcher is a failed reading\n{}", said(&output));
    assert_eq!(scratch.log(), Vec::<String>::new(), "grok must not be started directly");
    let envelope = String::from_utf8_lossy(&output.stdout);
    assert!(envelope.contains("only through `ai-masked`"), "{envelope}");
}
