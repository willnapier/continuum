//! `usage-probe-antigravity` run as a real process: on Linux its `/usage` read
//! starts `agy` through `ai-masked`, and without the launcher it starts
//! nothing (`~/Assistants/shared/AI-FOOTING-MASK.md`). Stand-in programs only;
//! see the fixture.
#![cfg(target_os = "linux")]

#[path = "../../continuum-core/tests/support/masked_fixture.rs"]
mod masked_fixture;

use masked_fixture::{said, Scratch};

const PROBE: &str = env!("CARGO_BIN_EXE_usage-probe-antigravity");
/// The stand-in prints the real reply captured 23 Nov 2026 (its text holds a
/// single quote, so it is read from the file, not quoted into the script).
/// Shell builtins only: the stand-in's `PATH` is the scratch folder alone.
fn reply() -> String {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/usage-2026-11-23.json");
    assert!(!fixture.contains('\''));
    format!("while IFS= read -r l || [ -n \"$l\" ]; do printf '%s\\n' \"$l\"; done < '{fixture}'")
}

#[test]
fn the_read_starts_agy_through_the_launcher() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "antigravity");
    scratch.stand_in_launcher();
    let agy = scratch.harness("agy", &reply());

    let output = scratch.command(PROBE).output().unwrap();

    assert!(output.status.success(), "the stand-in's reply is read through the launcher\n{}", said(&output));
    let log = scratch.log();
    assert_eq!(log[..2], ["launcher".to_string(), format!("arg:{}", agy.display())], "{log:?}");
    let harness_at = log.iter().position(|line| line == "harness:agy").expect("agy ran");
    let received = &log[harness_at + 1..log.len() - 2];
    assert_eq!(&log[2..harness_at], received, "{log:?}");
    assert_eq!(&received[..2], ["arg:-p", "arg:/usage"], "{log:?}");
    assert!(received.contains(&"arg:plan".to_string()), "plan mode: {log:?}");
    let envelope = String::from_utf8_lossy(&output.stdout);
    assert!(envelope.contains("antigravity-gemini-weekly"), "{envelope}");
}

/// agy runs from a folder made new for that run, under `TMPDIR`, empty, and
/// gone afterwards; two runs never share one.
#[test]
fn each_run_starts_agy_in_a_new_empty_folder_that_is_removed() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "antigravity-cwd");
    scratch.stand_in_launcher();
    let seen = scratch.root.join("cwd.log");
    let record = format!(
        "printf 'cwd:%s\\n' \"$PWD\" >> '{s}'\nfor f in * .[!.]*; do [ -e \"$f\" ] && printf 'entry:%s\\n' \"$f\" >> '{s}'; done\n: > left-behind\n",
        s = seen.display()
    );
    scratch.harness("agy", &format!("{record}{}", reply()));

    for _ in 0..2 {
        let output = scratch.command(PROBE).output().unwrap();
        assert!(output.status.success(), "{}", said(&output));
    }

    let lines: Vec<String> = std::fs::read_to_string(&seen).unwrap().lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "two cwd lines and no entries: {lines:?}");
    let tmp = scratch.root.join("tmp");
    let dirs: Vec<&str> = lines.iter().map(|l| l.strip_prefix("cwd:").expect("only cwd lines")).collect();
    assert_ne!(dirs[0], dirs[1], "each run has its own folder");
    for dir in dirs {
        assert!(std::path::Path::new(dir).starts_with(&tmp), "{dir} under TMPDIR");
        assert!(!std::path::Path::new(dir).exists(), "{dir} removed after the run");
    }
}

#[test]
fn without_the_launcher_agy_is_not_started() {
    let scratch = Scratch::new(env!("CARGO_TARGET_TMPDIR"), "antigravity-refuse");
    scratch.harness("agy", &reply());

    let output = scratch.command(PROBE).output().unwrap();

    assert!(!output.status.success(), "a missing launcher is a failed reading\n{}", said(&output));
    assert_eq!(scratch.log(), Vec::<String>::new(), "agy must not be started directly");
    let envelope = String::from_utf8_lossy(&output.stdout);
    assert!(envelope.contains("only through `ai-masked`"), "{envelope}");
}
