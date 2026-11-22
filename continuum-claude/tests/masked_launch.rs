//! `continuum-claude` run as a real process: on Linux it starts Claude Code
//! through `ai-masked`, and without the launcher it starts nothing
//! (`~/Assistants/shared/AI-FOOTING-MASK.md`). Stand-in programs only; see
//! the fixture.
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use masked_fixture::{said, Scratch};
use std::path::Path;

const WRAPPER: &str = env!("CARGO_BIN_EXE_continuum-claude");
/// Set in the wrapper's environment; it must not reach Claude Code.
const SCRATCH_KEY: &str = "zz99-not-a-key";

/// None when the scratch path would make the wrapper take the stand-in for
/// itself (it treats any path containing its own name as the wrapper, and
/// would then look for a real Claude Code in the standard places).
fn scratch(name: &str) -> Option<Scratch> {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), name);
    if scratch.root.to_string_lossy().contains("continuum-claude") {
        eprintln!("skipped: the target folder's path contains `continuum-claude`");
        return None;
    }
    Some(scratch)
}

// The no-save request (`~/.continuum-nosave`) across a start that reaches no
// Claude Code: it must still be there afterwards. Each mode has the two
// failures and the control (a start that works uses the request up).

const INTERACTIVE_ARGS: [&str; 2] = ["--resume", "ZZ99"];
const PRINT_ARGS: [&str; 2] = ["--print", "hello ZZ99"];
/// Print mode sends the prompt it read from stdin, so the stand-in reads it.
const PRINT_REPLY: &str = "read -r line\nprintf '%s\\n' '{\"type\":\"other\"}'\nexit 0";

#[test]
fn interactive_a_refusing_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("cc-nosave-refused-interactive") else { return };
    masked_fixture::a_refusing_launcher_keeps_the_request(&scratch, WRAPPER, "claude", &INTERACTIVE_ARGS);
}

#[test]
fn interactive_an_unrunnable_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("cc-nosave-unrunnable-interactive") else { return };
    masked_fixture::an_unrunnable_launcher_keeps_the_request(&scratch, WRAPPER, "claude", &INTERACTIVE_ARGS);
}

#[test]
fn interactive_a_started_session_uses_the_no_save_request_up() {
    let Some(scratch) = scratch("cc-nosave-started-interactive") else { return };
    masked_fixture::a_started_session_uses_the_request_up(
        &scratch, WRAPPER, "claude", "exit 0", &INTERACTIVE_ARGS, "nosave:1",
    );
}

#[test]
fn print_a_refusing_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("cc-nosave-refused-print") else { return };
    masked_fixture::a_refusing_launcher_keeps_the_request(&scratch, WRAPPER, "claude", &PRINT_ARGS);
}

#[test]
fn print_an_unrunnable_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("cc-nosave-unrunnable-print") else { return };
    masked_fixture::an_unrunnable_launcher_keeps_the_request(&scratch, WRAPPER, "claude", &PRINT_ARGS);
}

#[test]
fn print_a_started_session_uses_the_no_save_request_up() {
    let Some(scratch) = scratch("cc-nosave-started-print") else { return };
    masked_fixture::a_started_session_uses_the_request_up(
        &scratch, WRAPPER, "claude", PRINT_REPLY, &PRINT_ARGS, "nosave:1",
    );
}

#[test]
fn another_wrappers_reservation_is_not_a_request_and_is_not_touched() {
    let Some(scratch) = scratch("cc-nosave-foreign") else { return };
    scratch.stand_in_launcher();
    scratch.harness("claude", "exit 0");
    // No marker: only a reservation that another wrapper process holds.
    scratch.leave_foreign_reservation();

    let output = scratch.command(WRAPPER).args(INTERACTIVE_ARGS).output().unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    assert_eq!(scratch.nosave_seen(), ["nosave:unset"], "this session is an ordinary, saved one");
    // Still there, with its content (checked inside `nosave_state`).
    assert_eq!(scratch.nosave_state(), [masked_fixture::FOREIGN_RESERVATION]);
}

/// A no-save start deletes the transcript of the session it ran. With
/// `--continue` that is the newest transcript there was. When the launcher
/// refuses, nothing ran, so nothing may be deleted.
#[test]
fn a_refused_no_save_start_does_not_delete_an_earlier_transcript() {
    let Some(scratch) = scratch("cc-nosave-refused-continue") else { return };
    let earlier = scratch.home.join(".claude/projects/zz99-project/zz99-earlier-session.jsonl");
    std::fs::create_dir_all(earlier.parent().unwrap()).unwrap();
    std::fs::write(&earlier, b"{}\n").unwrap();

    masked_fixture::a_refusing_launcher_keeps_the_request(&scratch, WRAPPER, "claude", &["--continue"]);

    assert!(earlier.exists(), "the earlier session's transcript must still be there");
}

// A signal that ends the child. The child is the launcher until its exec, so
// a signal alone does not show that Claude Code ran: the request is kept and
// no earlier session is worked on, unless the wrapper saw a session itself.

const EARLIER_ID: &str = "zz99-earlier-session";
const EARLIER_CONTENT: &[u8] = b"{}\n";

/// A transcript of an earlier session in the scratch home.
fn plant_earlier_transcript(scratch: &Scratch) -> std::path::PathBuf {
    let earlier = scratch.home.join(format!(".claude/projects/zz99-project/{EARLIER_ID}.jsonl"));
    std::fs::create_dir_all(earlier.parent().unwrap()).unwrap();
    std::fs::write(&earlier, EARLIER_CONTENT).unwrap();
    earlier
}

#[test]
fn interactive_a_signalled_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("cc-nosave-signalled-interactive") else { return };
    masked_fixture::a_signalled_launcher_keeps_the_request(&scratch, WRAPPER, "claude", &INTERACTIVE_ARGS);
}

#[test]
fn print_a_signalled_launcher_keeps_the_no_save_request() {
    let Some(scratch) = scratch("cc-nosave-signalled-print") else { return };
    masked_fixture::a_signalled_launcher_keeps_the_request(&scratch, WRAPPER, "claude", &PRINT_ARGS);
}

/// `--continue` and `--resume <id>` both name an earlier session. A no-save
/// start whose launcher was ended by a signal ran nothing, so that session's
/// transcript must not be deleted.
#[test]
fn a_signalled_no_save_start_does_not_delete_an_earlier_transcript() {
    let cases: [(&str, &[&str]); 2] = [
        ("cc-nosave-signalled-continue", &["--continue"]),
        ("cc-nosave-signalled-resume", &["--resume", EARLIER_ID]),
    ];
    for (name, args) in cases {
        let Some(scratch) = scratch(name) else { return };
        let earlier = plant_earlier_transcript(&scratch);

        masked_fixture::a_signalled_launcher_keeps_the_request(&scratch, WRAPPER, "claude", args);

        assert_eq!(std::fs::read(&earlier).ok().as_deref(), Some(EARLIER_CONTENT), "{name}: the earlier transcript must be as it was");
    }
}

/// The same without a no-save request: the earlier session must not be
/// imported or logged as if this start had run it.
#[test]
fn a_signalled_start_does_not_import_an_earlier_session() {
    for (name, args) in [("cc-signalled-continue", vec!["--continue"]), ("cc-signalled-resume", vec!["--resume", EARLIER_ID])] {
        let Some(scratch) = scratch(name) else { return };
        let earlier = plant_earlier_transcript(&scratch);
        scratch.signalled_launcher();
        scratch.harness("claude", "exit 0");

        let output = scratch.command(WRAPPER).args(&args).output().unwrap();

        assert_eq!(output.status.code(), Some(1), "{name}\n{}", said(&output));
        assert_eq!(scratch.log().first().map(String::as_str), Some("launcher-signalled"), "{name}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("Importing session"), "{name}: nothing ran, nothing to import\n{stderr}");
        assert!(!scratch.home.join("Assistants/continuum-logs").exists(), "{name}: no record may be written");
        assert_eq!(std::fs::read(&earlier).ok().as_deref(), Some(EARLIER_CONTENT), "{name}");
    }
}

/// The control: Claude Code ran, this wrapper saw its transcript open, and a
/// signal then ended it. The request was delivered and is used up, and the
/// session is handled as a no-save session is: its transcript is deleted.
/// (The transcript is "seen" through a stand-in `lsof`, the tool the wrapper
/// asks which files its child holds open.)
#[test]
fn interactive_a_signalled_session_that_was_seen_uses_the_no_save_request_up() {
    let Some(scratch) = scratch("cc-nosave-signalled-seen-interactive") else { return };
    let live = scratch.home.join(".claude/projects/zz99-project/zz99-live-session.jsonl");
    std::fs::create_dir_all(live.parent().unwrap()).unwrap();
    std::fs::write(&live, b"{}\n").unwrap();
    scratch.tool("lsof", &format!("printf '%s\\n' 'p1' 'n{}'", live.display()));
    scratch.stand_in_launcher();
    // Long enough for the wrapper to look once, then ended by a signal.
    let then = format!("/bin/sleep 1 2>/dev/null || /usr/bin/sleep 1\n{}", masked_fixture::SIGNAL_SELF);
    scratch.harness("claude", &then);
    scratch.request_no_save();

    let output = scratch.command(WRAPPER).args(["--model", "zz99"]).output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", said(&output));
    assert_eq!(scratch.nosave_seen(), ["nosave:1"], "{}", said(&output));
    assert_eq!(scratch.nosave_state(), masked_fixture::REQUEST_USED, "delivered: used up\n{}", said(&output));
    assert!(!live.exists(), "the no-save session's own transcript is deleted\n{}", said(&output));
}

/// The control in print mode: Claude Code printed a session event, and a
/// signal then ended it. The request was delivered and is used up.
#[test]
fn print_a_signalled_session_that_printed_an_event_uses_the_no_save_request_up() {
    let Some(scratch) = scratch("cc-nosave-signalled-seen-print") else { return };
    scratch.stand_in_launcher();
    let event = r#"{"type":"system","subtype":"init","session_id":"zz99-print","cwd":"/","tools":[],"model":"zz99"}"#;
    let then = format!("read -r line\nprintf '%s\\n' '{event}'\n{}", masked_fixture::SIGNAL_SELF);
    scratch.harness("claude", &then);
    scratch.request_no_save();

    let output = scratch.command(WRAPPER).args(PRINT_ARGS).output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", said(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("zz99-print"), "{}", said(&output));
    assert_eq!(scratch.nosave_seen(), ["nosave:1"], "{}", said(&output));
    assert_eq!(scratch.nosave_state(), masked_fixture::REQUEST_USED, "delivered: used up\n{}", said(&output));
}

/// Claude Code ran and a signal ended it, but this wrapper saw no session of
/// it (no transcript open; no event printed). The request is given back, and
/// in interactive mode the earlier session that `--continue` names is left
/// alone. This errs towards not saving.
#[test]
fn a_signalled_session_that_was_not_seen_gets_the_no_save_request_back() {
    let Some(interactive) = scratch("cc-nosave-signalled-unseen-interactive") else { return };
    let earlier = plant_earlier_transcript(&interactive);
    masked_fixture::a_signalled_harness_that_was_not_seen_gets_the_request_back(
        &interactive, WRAPPER, "claude", masked_fixture::SIGNAL_SELF, &["--continue"], "nosave:1",
    );
    assert_eq!(std::fs::read(&earlier).ok().as_deref(), Some(EARLIER_CONTENT));

    let Some(print) = scratch("cc-nosave-signalled-unseen-print") else { return };
    let then = format!("read -r line\n{}", masked_fixture::SIGNAL_SELF);
    masked_fixture::a_signalled_harness_that_was_not_seen_gets_the_request_back(
        &print, WRAPPER, "claude", &then, &PRINT_ARGS, "nosave:1",
    );
}

/// The launcher's failed exec (127: not found; 126: would not run) is a start
/// that reached nothing, like its refusal.
#[test]
fn a_launcher_that_cannot_run_claude_keeps_the_no_save_request() {
    for code in [126, 127] {
        let Some(interactive) = scratch(&format!("cc-nosave-exec-{code}-interactive")) else { return };
        masked_fixture::a_launcher_that_cannot_run_the_harness_keeps_the_request(
            &interactive, WRAPPER, "claude", &INTERACTIVE_ARGS, code,
        );
        let Some(print) = scratch(&format!("cc-nosave-exec-{code}-print")) else { return };
        masked_fixture::a_launcher_that_cannot_run_the_harness_keeps_the_request(
            &print, WRAPPER, "claude", &PRINT_ARGS, code,
        );
    }
}

#[test]
fn interactive_mode_starts_claude_through_the_launcher() {
    let Some(scratch) = scratch("cc-interactive") else { return };
    scratch.stand_in_launcher();
    let claude = scratch.harness("claude", "exit 7");
    let args = ["--resume", "ZZ99", "--model", "two words"];

    let output = scratch
        .command(WRAPPER)
        .env("ANTHROPIC_API_KEY", SCRATCH_KEY)
        .args(args)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7), "claude's exit code comes back\n{}", said(&output));
    // The wrapper resolves the real binary's path before it starts it.
    let resolved = std::fs::canonicalize(&claude).unwrap();
    assert_eq!(scratch.log(), Scratch::masked_log(&resolved, "claude", &args, "unset", "unset"));
}

#[test]
fn print_mode_starts_claude_through_the_launcher() {
    let Some(scratch) = scratch("cc-print") else { return };
    scratch.stand_in_launcher();
    // The wrapper sends the prompt it read from stdin (here an empty line).
    scratch.harness("claude", "read -r line\nprintf '%s\\n' '{\"type\":\"other\"}'\nexit 0");

    let output = scratch
        .command(WRAPPER)
        .env("ANTHROPIC_API_KEY", SCRATCH_KEY)
        .args(["--print", "hello ZZ99"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", said(&output));
    // Print mode names `claude` and leaves the lookup to the launcher.
    let sent = ["--print", "hello ZZ99", "--output-format", "stream-json", "--verbose"];
    assert_eq!(
        scratch.log(),
        Scratch::masked_log(Path::new("claude"), "claude", &sent, "unset", "unset")
    );
}

#[test]
fn without_the_launcher_nothing_starts_in_either_mode() {
    for (name, args) in [("cc-refuse-interactive", vec!["--resume", "ZZ99"]), ("cc-refuse-print", vec!["--print", "hello"])] {
        let Some(scratch) = scratch(name) else { return };
        scratch.harness("claude", "exit 0");
        let marker = scratch.home.join(".continuum-nosave");
        std::fs::write(&marker, b"").unwrap();

        let output = scratch.command(WRAPPER).args(&args).output().unwrap();

        assert!(!output.status.success(), "{name}: a missing launcher is a failure\n{}", said(&output));
        assert_eq!(scratch.log(), Vec::<String>::new(), "{name}: claude must not be started directly");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("only through `ai-masked`"), "{name}: {stderr}");
        assert!(stderr.contains("rust-redeploy --only ai-masked"), "{name}: {stderr}");
        assert!(marker.exists(), "{name}: a refused start must not use up the no-save marker");
    }
}

#[test]
fn a_launcher_refusal_comes_back_as_its_own_exit_code() {
    let Some(scratch) = scratch("cc-launcher-refused") else { return };
    scratch.refusing_launcher();
    let claude = scratch.harness("claude", "exit 0");

    let output = scratch.command(WRAPPER).args(["--model", "x"]).output().unwrap();

    assert_eq!(output.status.code(), Some(113), "{}", said(&output));
    let resolved = std::fs::canonicalize(&claude).unwrap();
    let asked = vec![
        "launcher-refused".to_string(),
        format!("arg:{}", resolved.display()),
        "arg:--model".to_string(),
        "arg:x".to_string(),
    ];
    assert_eq!(scratch.log(), asked, "the launcher was asked, and claude never ran");
    assert!(String::from_utf8_lossy(&output.stderr).contains("ai-masked: refusing to start"));
}
