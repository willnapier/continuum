//! Scratch folders and stand-in programs for the tests that run a wrapper or
//! a probe as a real process and check what it starts
//! (`continuum_core::masked_launch`, `~/Assistants/shared/AI-FOOTING-MASK.md`).
//!
//! The real `ai-masked` is never used: the program under test gets a `PATH`
//! of one scratch folder, a scratch `HOME`, and nothing else from the
//! environment. The stand-in launcher and the stand-in harnesses are small
//! `sh` scripts that write what they were given to one log, in order. The
//! stand-in launcher then becomes its arguments by `exec`, as the real one
//! does.
//!
//! Each binary's `tests/masked_launch.rs` includes this file by `#[path]`, so
//! it uses nothing outside `std`. Everything lives under the folder passed to
//! [`Scratch::new`], which is `env!("CARGO_TARGET_TMPDIR")`: inside `target/`,
//! not `/tmp`.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The name the stand-in launcher takes: the real launcher's.
pub const LAUNCHER: &str = "ai-masked";

pub struct Scratch {
    pub root: PathBuf,
    /// The only folder on the `PATH` of the program under test.
    pub bin: PathBuf,
    pub home: PathBuf,
    log: PathBuf,
}

impl Scratch {
    pub fn new(target_tmp: &str, name: &str) -> Self {
        let root = Path::new(target_tmp).join(format!("masked-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let scratch = Self {
            bin: root.join("bin"),
            home: root.join("home"),
            log: root.join("started.log"),
            root,
        };
        for dir in [&scratch.bin, &scratch.home, &scratch.root.join("tmp")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        assert!(
            !scratch.root.to_string_lossy().contains('\''),
            "the scratch path goes into sh scripts inside single quotes"
        );
        scratch
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.bin.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Lines that append `first`, then one `arg:` line per argument, to the log.
    fn record(&self, first: &str, more: &str) -> String {
        format!(
            "{{\n  printf '%s\\n' '{first}'\n  for a in \"$@\"; do printf '%s\\n' \"arg:$a\"; done\n{more}}} >> '{}'",
            self.log.display()
        )
    }

    /// A launcher on the scratch `PATH` that records its arguments and then
    /// becomes them.
    pub fn stand_in_launcher(&self) -> PathBuf {
        let body = format!("{}\nexec \"$@\"", self.record("launcher", ""));
        self.script(LAUNCHER, &body)
    }

    /// A launcher that refuses as the real one does: a line on stderr, exit
    /// 113, nothing started.
    pub fn refusing_launcher(&self) -> PathBuf {
        let body = format!(
            "{}\nprintf '%s\\n' 'ai-masked: refusing to start: a scratch folder could not be hidden' >&2\nexit 113",
            self.record("launcher-refused", "")
        );
        self.script(LAUNCHER, &body)
    }

    /// A launcher that cannot become the harness, as the real one reports a
    /// failed exec: a line on stderr, exit `code` (127 or 126), nothing
    /// started.
    pub fn failed_exec_launcher(&self, code: i32) -> PathBuf {
        let body = format!(
            "{}\nprintf '%s\\n' 'ai-masked: cannot run the harness' >&2\nexit {code}",
            self.record("launcher-exec-failed", "")
        );
        self.script(LAUNCHER, &body)
    }

    /// A launcher that a signal ends before it becomes the harness: it
    /// records its arguments, then sends itself SIGTERM. Nothing is started,
    /// and the wrapper's child has no exit code.
    pub fn signalled_launcher(&self) -> PathBuf {
        let body = format!("{}\n{SIGNAL_SELF}", self.record("launcher-signalled", ""));
        self.script(LAUNCHER, &body)
    }

    /// Any other program on the scratch `PATH` (sh body).
    pub fn tool(&self, name: &str, body: &str) -> PathBuf {
        self.script(name, body)
    }

    /// A file with the launcher's name that cannot be run (no execute bit):
    /// it is found, and the start then fails.
    pub fn unrunnable_launcher(&self) -> PathBuf {
        let path = self.stand_in_launcher();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        path
    }

    /// A stand-in harness called `name` on the scratch `PATH`. It records its
    /// arguments, then two things the wrappers set in its environment, then
    /// (in a list of its own, see [`Self::nosave_seen`]) whether it was told
    /// not to be saved, then does `then` (sh).
    pub fn harness(&self, name: &str, then: &str) -> PathBuf {
        let environment = "  printf '%s\\n' \"key:${ANTHROPIC_API_KEY-unset}\" \"depth:${CONTINUUM_CODEX_DEPTH-unset}\"\n";
        let body = format!(
            "{}\nprintf '%s\\n' \"nosave:${{CONTINUUM_NOSAVE-unset}}\" >> '{}'\n{then}",
            self.record(&format!("harness:{name}"), environment),
            self.seen().display()
        );
        self.script(name, &body)
    }

    fn seen(&self) -> PathBuf {
        self.root.join("nosave-seen.log")
    }

    /// One line per harness that ran: `nosave:1` when the wrapper told it the
    /// session is not to be saved, `nosave:unset` otherwise. Empty when no
    /// harness ran.
    pub fn nosave_seen(&self) -> Vec<String> {
        std::fs::read_to_string(self.seen())
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// Put a no-save request in the scratch home, and beside it a reservation
    /// that belongs to another wrapper process.
    pub fn request_no_save(&self) {
        std::fs::write(self.home.join(NOSAVE_MARKER), b"").unwrap();
        self.leave_foreign_reservation();
    }

    /// A reservation held by another wrapper process (PID 1 is never the
    /// wrapper under test). No wrapper may touch it.
    pub fn leave_foreign_reservation(&self) {
        std::fs::write(self.home.join(FOREIGN_RESERVATION), FOREIGN_CONTENT).unwrap();
    }

    /// The no-save files in the scratch home, sorted: the marker and any
    /// reservation. Also checks the other wrapper's reservation is as left.
    pub fn nosave_state(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(NOSAVE_MARKER))
            .collect();
        names.sort();
        if let Ok(content) = std::fs::read(self.home.join(FOREIGN_RESERVATION)) {
            assert_eq!(content, FOREIGN_CONTENT, "another wrapper's reservation was rewritten");
        }
        names
    }

    /// `exe` with nothing inherited: the scratch `PATH` and `HOME` only, and
    /// no terminal.
    pub fn command(&self, exe: &str) -> Command {
        let mut command = Command::new(exe);
        command
            .env_clear()
            .env("PATH", &self.bin)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("CONTINUUM_HOME", self.home.join("Assistants"))
            .env("TMPDIR", self.root.join("tmp"))
            .current_dir(&self.root)
            .stdin(Stdio::null());
        command
    }

    /// What was started, in order. Empty when nothing was.
    pub fn log(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The log a masked start must leave: the launcher given the harness and
    /// the arguments, then the harness given the same arguments, with `key`
    /// and `depth` as the harness saw them.
    pub fn masked_log(
        harness_as_given: &Path,
        harness_name: &str,
        args: &[&str],
        key: &str,
        depth: &str,
    ) -> Vec<String> {
        let arg_lines = || args.iter().map(|arg| format!("arg:{arg}"));
        let mut lines = vec!["launcher".to_string(), format!("arg:{}", harness_as_given.display())];
        lines.extend(arg_lines());
        lines.push(format!("harness:{harness_name}"));
        lines.extend(arg_lines());
        lines.push(format!("key:{key}"));
        lines.push(format!("depth:{depth}"));
        lines
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// sh: end this process by a signal, so that it has no exit code. (The
/// `exit 98` is never reached; it would show up as a wrong exit code.)
pub const SIGNAL_SELF: &str = "kill -TERM $$\nexit 98";

/// The no-save marker's name in the home folder.
pub const NOSAVE_MARKER: &str = ".continuum-nosave";
/// A reservation that belongs to another wrapper process.
pub const FOREIGN_RESERVATION: &str = ".continuum-nosave.reserved-1";
const FOREIGN_CONTENT: &[u8] = b"held by another wrapper";

/// The no-save files after a start that reached no harness: the request is
/// there again under the marker's name, and no reservation of the wrapper
/// under test is left.
pub const REQUEST_KEPT: [&str; 2] = [NOSAVE_MARKER, FOREIGN_RESERVATION];
/// The no-save files after a start that reached a harness: the request is
/// used up.
pub const REQUEST_USED: [&str; 1] = [FOREIGN_RESERVATION];

/// With a no-save request waiting, the launcher refuses (exit 113): the
/// harness never runs, the wrapper exits 113, and the request is kept.
pub fn a_refusing_launcher_keeps_the_request(
    scratch: &Scratch,
    wrapper: &str,
    harness: &str,
    args: &[&str],
) {
    scratch.refusing_launcher();
    scratch.harness(harness, "exit 0");
    scratch.request_no_save();

    let output = scratch.command(wrapper).args(args).output().unwrap();

    assert_eq!(output.status.code(), Some(113), "the launcher's refusal comes back\n{}", said(&output));
    let log = scratch.log();
    assert_eq!(log.first().map(String::as_str), Some("launcher-refused"), "{log:?}");
    assert!(!log.iter().any(|line| line.starts_with("harness:")), "{harness} never ran: {log:?}");
    assert_eq!(scratch.nosave_seen(), Vec::<String>::new());
    assert_eq!(scratch.nosave_state(), REQUEST_KEPT, "the request must still be there\n{}", said(&output));
}

/// With a no-save request waiting, the launcher cannot become the harness
/// and exits `code` (127 or 126): the harness never runs, the wrapper exits
/// with that code, and the request is kept.
pub fn a_launcher_that_cannot_run_the_harness_keeps_the_request(
    scratch: &Scratch,
    wrapper: &str,
    harness: &str,
    args: &[&str],
    code: i32,
) {
    scratch.failed_exec_launcher(code);
    scratch.harness(harness, "exit 0");
    scratch.request_no_save();

    let output = scratch.command(wrapper).args(args).output().unwrap();

    assert_eq!(output.status.code(), Some(code), "the launcher's code comes back\n{}", said(&output));
    let log = scratch.log();
    assert_eq!(log.first().map(String::as_str), Some("launcher-exec-failed"), "{log:?}");
    assert!(!log.iter().any(|line| line.starts_with("harness:")), "{harness} never ran: {log:?}");
    assert_eq!(scratch.nosave_seen(), Vec::<String>::new());
    assert_eq!(scratch.nosave_state(), REQUEST_KEPT, "the request must still be there\n{}", said(&output));
}

/// With a no-save request waiting, a signal ends the launcher before it
/// becomes the harness: the harness never runs, the wrapper fails, and the
/// request is kept. The wrapper's child has no exit code here, and that alone
/// must not be taken for a harness that ran.
pub fn a_signalled_launcher_keeps_the_request(
    scratch: &Scratch,
    wrapper: &str,
    harness: &str,
    args: &[&str],
) {
    scratch.signalled_launcher();
    scratch.harness(harness, "exit 0");
    scratch.request_no_save();

    let output = scratch.command(wrapper).args(args).output().unwrap();

    assert_eq!(output.status.code(), Some(1), "a child ended by a signal is a failure\n{}", said(&output));
    let log = scratch.log();
    assert_eq!(log.first().map(String::as_str), Some("launcher-signalled"), "{log:?}");
    assert!(!log.iter().any(|line| line.starts_with("harness:")), "{harness} never ran: {log:?}");
    assert_eq!(scratch.nosave_seen(), Vec::<String>::new());
    assert_eq!(scratch.nosave_state(), REQUEST_KEPT, "the request must still be there\n{}", said(&output));
}

/// With a no-save request waiting, the harness runs (`then`, sh, must end it
/// by a signal) and the wrapper saw no session of it: the request is given
/// back, so the next session is not saved either. The mistake, if the harness
/// did run a session, is made towards not saving.
pub fn a_signalled_harness_that_was_not_seen_gets_the_request_back(
    scratch: &Scratch,
    wrapper: &str,
    harness: &str,
    then: &str,
    args: &[&str],
    seen: &str,
) {
    scratch.stand_in_launcher();
    scratch.harness(harness, then);
    scratch.request_no_save();

    let output = scratch.command(wrapper).args(args).output().unwrap();

    assert_eq!(output.status.code(), Some(1), "a child ended by a signal is a failure\n{}", said(&output));
    let log = scratch.log();
    assert!(log.iter().any(|line| line == &format!("harness:{harness}")), "{harness} ran: {log:?}");
    assert_eq!(scratch.nosave_seen(), [seen], "{}", said(&output));
    assert_eq!(scratch.nosave_state(), REQUEST_KEPT, "the request must be given back\n{}", said(&output));
}

/// With a no-save request waiting, the launcher is found but cannot be run:
/// the harness never runs, the wrapper fails, and the request is kept.
pub fn an_unrunnable_launcher_keeps_the_request(
    scratch: &Scratch,
    wrapper: &str,
    harness: &str,
    args: &[&str],
) {
    scratch.unrunnable_launcher();
    scratch.harness(harness, "exit 0");
    scratch.request_no_save();

    let output = scratch.command(wrapper).args(args).output().unwrap();

    assert!(!output.status.success(), "a launcher that cannot run is a failure\n{}", said(&output));
    assert_eq!(scratch.log(), Vec::<String>::new(), "{harness} must not be started directly");
    assert_eq!(scratch.nosave_seen(), Vec::<String>::new());
    assert!(String::from_utf8_lossy(&output.stderr).contains("through `ai-masked`"), "{}", said(&output));
    assert_eq!(scratch.nosave_state(), REQUEST_KEPT, "the request must still be there\n{}", said(&output));
}

/// The control for the two cases above: with a no-save request waiting and a
/// launcher that works, the harness runs, sees `seen` (`nosave:1` where the
/// wrapper passes the request on in the environment), and the request is used
/// up.
pub fn a_started_session_uses_the_request_up(
    scratch: &Scratch,
    wrapper: &str,
    harness: &str,
    then: &str,
    args: &[&str],
    seen: &str,
) {
    scratch.stand_in_launcher();
    scratch.harness(harness, then);
    scratch.request_no_save();

    let output = scratch.command(wrapper).args(args).output().unwrap();

    let log = scratch.log();
    assert_eq!(log.first().map(String::as_str), Some("launcher"), "{log:?}\n{}", said(&output));
    assert!(log.iter().any(|line| line == &format!("harness:{harness}")), "{harness} ran: {log:?}");
    assert_eq!(scratch.nosave_seen(), [seen], "{}", said(&output));
    assert_eq!(scratch.nosave_state(), REQUEST_USED, "the request must be used up\n{}", said(&output));
}

/// stdout and stderr of a finished run, for assertion messages.
pub fn said(output: &std::process::Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
