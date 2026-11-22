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

    /// A stand-in harness called `name` on the scratch `PATH`. It records its
    /// arguments, then two things the wrappers set in its environment, then
    /// does `then` (sh).
    pub fn harness(&self, name: &str, then: &str) -> PathBuf {
        let environment = "  printf '%s\\n' \"key:${ANTHROPIC_API_KEY-unset}\" \"depth:${CONTINUUM_CODEX_DEPTH-unset}\"\n";
        let body = format!("{}\n{then}", self.record(&format!("harness:{name}"), environment));
        self.script(name, &body)
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

/// stdout and stderr of a finished run, for assertion messages.
pub fn said(output: &std::process::Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
