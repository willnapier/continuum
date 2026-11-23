//! `usage-probe-codex` run as a real process: on Linux its `codex app-server`
//! starts through `ai-masked`, without the launcher it starts nothing, and a
//! launcher refusal is reported as one
//! (`~/Assistants/shared/AI-FOOTING-MASK.md`). Stand-in programs only; see
//! the fixture.
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use masked_fixture::{said, Scratch};

const PROBE: &str = env!("CARGO_BIN_EXE_usage-probe-codex");
/// Enough of the app-server exchange for one reading: answer `initialize`,
/// take `initialized` and the read request, answer it, then wait to be
/// killed, as the real app-server does.
const APP_SERVER: &str = "read -r line
printf '%s\\n' '{\"id\":0,\"result\":{}}'
read -r line
read -r line
printf '%s\\n' '{\"id\":1,\"result\":{\"rateLimits\":{\"planType\":\"zz99\",\"primary\":{\"usedPercent\":10,\"windowDurationMins\":300,\"resetsAt\":1}}}}'
read -r line";

#[test]
fn the_app_server_starts_through_the_launcher() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-limits");
    scratch.stand_in_launcher();
    let codex = scratch.harness("codex", APP_SERVER);

    let output = scratch.command(PROBE).output().unwrap();

    assert!(output.status.success(), "the reading comes back through the launcher\n{}", said(&output));
    assert_eq!(scratch.log(), Scratch::masked_log(&codex, "codex", &["app-server"], "unset", "unset"));
}

#[test]
fn without_the_launcher_the_app_server_is_not_started() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-limits-refuse");
    scratch.harness("codex", APP_SERVER);

    let output = scratch.command(PROBE).output().unwrap();

    assert!(!output.status.success(), "a missing launcher is a failed reading\n{}", said(&output));
    assert_eq!(scratch.log(), Vec::<String>::new(), "codex must not be started directly");
    let envelope = String::from_utf8_lossy(&output.stdout);
    assert!(envelope.contains("only through `ai-masked`"), "{envelope}");
}

#[test]
fn a_launcher_refusal_is_reported_as_one() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "codex-limits-launcher-refused");
    scratch.refusing_launcher();
    scratch.harness("codex", APP_SERVER);

    let output = scratch.command(PROBE).output().unwrap();

    assert!(!output.status.success(), "{}", said(&output));
    let log = scratch.log();
    assert_eq!(log.first().map(String::as_str), Some("launcher-refused"), "{log:?}");
    assert!(!log.iter().any(|line| line.starts_with("harness:")), "codex never ran: {log:?}");
    let envelope = String::from_utf8_lossy(&output.stdout);
    assert!(envelope.contains("`ai-masked` refused to start the Codex app-server"), "{envelope}");
}
