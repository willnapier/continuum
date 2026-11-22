//! Start an assistant harness through the mask launcher, `ai-masked`.
//!
//! On Linux every start of Claude Code, Codex or Grok that Continuum makes
//! (the capture wrappers and the usage probes) runs
//! `ai-masked <harness> [args…]` in place of `<harness> [args…]`. The launcher
//! hides the protected folders from the harness, sets `AI_FOOTING=masked`,
//! drops `ANTHROPIC_API_KEY`, and then becomes the harness by exec: the same
//! PID, stdio, exit code and signals. A caller therefore waits for, kills and
//! reads its child exactly as it did before. What is hidden, how to tell that
//! a session is masked, and the health finding for one that is not, are in
//! `~/Assistants/shared/AI-FOOTING-MASK.md`.
//!
//! The wrappers do this themselves, although the shell functions that call
//! them do it too, because some launches never pass through a shell function:
//! Zellij's session resurrection re-runs `continuum-claude --resume <id>`
//! directly, and a systemd timer runs the usage probes.
//!
//! **It fails closed.** Where the mask applies and the launcher cannot be
//! found, [`harness_launch`] returns an error and nothing is started. No
//! caller may start the harness directly in its place. A launcher that is
//! found can still start nothing: it refuses, it cannot run the harness, or a
//! signal ends it before its exec. [`child_end`] says what the child's end
//! shows, and only a status that is not the launcher's own shows a harness.
//!
//! The launch on macOS is unchanged until work order `ai-footing-mask-mac`:
//! [`masked_on_this_system`] is the one place that says which systems are
//! masked.
//!
//! This file is compiled into two libraries: `continuum-core`, for the
//! wrappers, and `continuum-usage-core`, for the probes (by `#[path]` in its
//! `lib.rs`). Neither library depends on the other and the rule must not
//! differ between them. So it uses nothing outside `std`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The launcher's name on `PATH`.
pub const LAUNCHER: &str = "ai-masked";

/// The launcher's exit code when it refuses to start anything (it could not
/// hide a folder, or its list is missing). It prints
/// `ai-masked: refusing to start: …` on stderr first.
pub const REFUSED_EXIT: i32 = 113;

/// The launcher's exit codes when it could not become the harness: 127 (not
/// found) and 126 (found, but it would not run).
pub const EXEC_FAILED_EXITS: [i32; 2] = [126, 127];

/// Said with every failure to start through the launcher.
const ONLY_THROUGH: &str = "On this system assistants start only through `ai-masked`, which \
hides the protected folders from them; there is no direct start. Install it with \
`rust-redeploy --only ai-masked`. See ~/Assistants/shared/AI-FOOTING-MASK.md.";

/// Whether harness starts on this system go through the launcher. Linux only,
/// until `ai-footing-mask-mac`. No variable or flag changes the answer.
pub const fn masked_on_this_system() -> bool {
    cfg!(target_os = "linux")
}

/// The program to run and the arguments that come before the caller's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: OsString,
    pub lead_args: Vec<OsString>,
}

impl Launch {
    /// `<launcher> <harness>`: the masked start.
    pub fn through(launcher: &Path, harness: &OsStr) -> Self {
        Self {
            program: launcher.as_os_str().to_os_string(),
            lead_args: vec![harness.to_os_string()],
        }
    }

    /// `<harness>` alone: the start on a system without the mask.
    pub fn direct(harness: &OsStr) -> Self {
        Self {
            program: harness.to_os_string(),
            lead_args: Vec::new(),
        }
    }

    /// A `std` command for this start. The caller adds its own arguments.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.lead_args);
        command
    }
}

/// How to start `harness` (a bare name for the launcher to look up on `PATH`,
/// or a path) on this system: through the launcher on Linux, directly
/// elsewhere. An error means nothing may be started.
pub fn harness_launch(harness: impl AsRef<OsStr>) -> Result<Launch, String> {
    plan(masked_on_this_system(), harness.as_ref(), find_launcher)
}

/// [`harness_launch`] as a `std` command, ready for the caller's arguments.
pub fn harness_command(harness: impl AsRef<OsStr>) -> Result<Command, String> {
    harness_launch(harness).map(|launch| launch.command())
}

/// The text to report when the start itself fails (the spawn returned an
/// error). `what` names the harness, for example `claude`.
pub fn spawn_failure(what: &str) -> String {
    if masked_on_this_system() {
        format!("failed to start {what} through `{LAUNCHER}`. {ONLY_THROUGH}")
    } else {
        format!("failed to start {what}")
    }
}

/// What the end of a caller's child says about whether a harness ran.
///
/// [`harness_launch`] finding the launcher does not mean a harness started.
/// On a masked system the child is the launcher until its exec succeeds, and
/// only then the harness, in the same process. So the child's end is read
/// three ways, and only the first says that a harness ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildEnd {
    /// The child exited with a status that is not one of the launcher's own.
    /// The launcher never exits that way, so the harness ran and this is its
    /// status. Also every end on a system without the mask, where the child
    /// is the harness from the start.
    HarnessExited,
    /// The child exited with one of the launcher's own codes: its refusal
    /// ([`REFUSED_EXIT`]) or a failed exec ([`EXEC_FAILED_EXITS`]). The codes
    /// are not reserved: a harness that itself exits with one of them after a
    /// real session reads the same.
    LauncherStartedNothing,
    /// A signal ended the child. That does not say which program it ended:
    /// the launcher before its exec (nothing ran), or the harness after it.
    Signalled,
}

impl ChildEnd {
    /// Read the child's exit code (`None` when a signal ended it) on a system
    /// that is or is not masked.
    pub fn of(masked: bool, exit_code: Option<i32>) -> Self {
        match exit_code {
            _ if !masked => Self::HarnessExited,
            Some(code) if code == REFUSED_EXIT || EXEC_FAILED_EXITS.contains(&code) => {
                Self::LauncherStartedNothing
            }
            Some(_) => Self::HarnessExited,
            None => Self::Signalled,
        }
    }
}

/// [`ChildEnd::of`] for this system. Only [`ChildEnd::HarnessExited`] shows
/// that a harness ran. After either of the others a caller must not act as if
/// one did, unless it has evidence of its own (it saw that child's session):
/// a no-save request is not used up, and no earlier session is taken for the
/// one this start would have made.
pub fn child_end(exit_code: Option<i32>) -> ChildEnd {
    ChildEnd::of(masked_on_this_system(), exit_code)
}

/// The rule itself, with the system and the launcher lookup passed in so that
/// both systems can be tested on either.
fn plan(
    masked: bool,
    harness: &OsStr,
    find: impl FnOnce() -> Option<PathBuf>,
) -> Result<Launch, String> {
    if !masked {
        return Ok(Launch::direct(harness));
    }
    match find() {
        Some(launcher) => Ok(Launch::through(&launcher, harness)),
        None => Err(format!(
            "`{LAUNCHER}` was not found on PATH or in ~/.local/bin, so nothing was started. \
             {ONLY_THROUGH}"
        )),
    }
}

fn find_launcher() -> Option<PathBuf> {
    find_launcher_in(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").as_deref(),
        |candidate| candidate.is_file(),
    )
}

/// The launcher on `path`, else in `~/.local/bin` (a systemd unit or a
/// resurrected terminal does not always carry the interactive `PATH`; the
/// smoke probes find their harness the same way). Entries that are not
/// absolute are skipped: the launcher is never taken from the current folder.
fn find_launcher_in(
    path: Option<&OsStr>,
    home: Option<&OsStr>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let on_path = path
        .map(|paths| std::env::split_paths(paths).collect::<Vec<_>>())
        .unwrap_or_default();
    let fallback = home
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".local/bin"));
    on_path
        .into_iter()
        .chain(fallback)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(LAUNCHER))
        .find(|candidate| is_file(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(text: &str) -> OsString {
        OsString::from(text)
    }

    /// The Linux assertion: the launcher runs, the harness is its first
    /// argument, and the caller's arguments follow unchanged.
    fn check_masked_line(
        command: &Command,
        launcher: &str,
        harness: &str,
        user_args: &[&str],
    ) -> Result<(), String> {
        let program = command.get_program().to_os_string();
        let args: Vec<OsString> = command.get_args().map(OsStr::to_os_string).collect();
        let mut expected = vec![os(harness)];
        expected.extend(user_args.iter().map(|arg| os(arg)));
        if program != os(launcher) {
            return Err(format!("program is {program:?}, not the launcher {launcher:?}"));
        }
        if args != expected {
            return Err(format!("arguments are {args:?}, not {expected:?}"));
        }
        Ok(())
    }

    const USER_ARGS: [&str; 3] = ["--resume", "ZZ99 session", "--print"];

    #[test]
    fn on_linux_the_launcher_runs_first_then_the_harness_then_the_arguments() {
        let launch = plan(true, OsStr::new("/opt/real/claude"), || {
            Some(PathBuf::from("/opt/bin/ai-masked"))
        })
        .unwrap();
        assert_eq!(launch, Launch::through(Path::new("/opt/bin/ai-masked"), OsStr::new("/opt/real/claude")));
        let mut command = launch.command();
        command.args(USER_ARGS);
        check_masked_line(&command, "/opt/bin/ai-masked", "/opt/real/claude", &USER_ARGS).unwrap();
    }

    /// Red control for the test above: the start as it was before the mask
    /// (the harness run directly) does not pass the Linux assertion.
    #[test]
    fn the_old_direct_start_fails_the_linux_assertion() {
        let mut command = Launch::direct(OsStr::new("/opt/real/claude")).command();
        command.args(USER_ARGS);
        let refused = check_masked_line(&command, "/opt/bin/ai-masked", "/opt/real/claude", &USER_ARGS);
        assert!(refused.is_err(), "a direct start must not look like a masked one");
        assert!(refused.unwrap_err().contains("not the launcher"));
    }

    #[test]
    fn on_other_systems_the_start_is_unchanged_and_the_launcher_is_not_looked_for() {
        let launch = plan(false, OsStr::new("/opt/real/claude"), || {
            panic!("the launcher must not be looked for on a system without the mask")
        })
        .unwrap();
        let mut command = launch.command();
        command.args(USER_ARGS);
        assert_eq!(command.get_program(), OsStr::new("/opt/real/claude"));
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(args, USER_ARGS.map(OsStr::new));
    }

    #[test]
    fn a_missing_launcher_is_an_error_not_a_direct_start() {
        let error = plan(true, OsStr::new("claude"), || None).unwrap_err();
        assert!(error.contains("nothing was started"), "{error}");
        assert!(error.contains("only through `ai-masked`"), "{error}");
        assert!(error.contains("rust-redeploy --only ai-masked"), "{error}");
        assert!(error.contains("AI-FOOTING-MASK.md"), "{error}");
    }

    #[test]
    fn only_an_exit_that_is_not_the_launchers_own_shows_that_a_harness_ran() {
        for code in [113, 126, 127] {
            assert_eq!(ChildEnd::of(true, Some(code)), ChildEnd::LauncherStartedNothing, "{code}");
        }
        for code in [0, 1, 2, 130, 143] {
            assert_eq!(ChildEnd::of(true, Some(code)), ChildEnd::HarnessExited, "{code}");
        }
        // A signal may have ended the launcher before its exec: it shows no
        // harness. (Red control: reading it as `HarnessExited`, the rule
        // before, fails here.)
        assert_eq!(ChildEnd::of(true, None), ChildEnd::Signalled);
        assert_ne!(ChildEnd::of(true, None), ChildEnd::HarnessExited);
    }

    #[test]
    fn without_the_mask_the_child_is_the_harness_however_it_ends() {
        for code in [Some(0), Some(1), Some(113), Some(126), Some(127), None] {
            assert_eq!(ChildEnd::of(false, code), ChildEnd::HarnessExited, "{code:?}");
        }
        let here = if cfg!(target_os = "linux") {
            ChildEnd::Signalled
        } else {
            ChildEnd::HarnessExited
        };
        assert_eq!(child_end(None), here);
    }

    #[test]
    fn this_system_is_masked_exactly_when_it_is_linux() {
        assert_eq!(masked_on_this_system(), cfg!(target_os = "linux"));
        let through_launcher = spawn_failure("claude").contains(LAUNCHER);
        assert_eq!(through_launcher, cfg!(target_os = "linux"));
    }

    #[test]
    fn the_launcher_is_taken_from_path_in_order_then_from_local_bin() {
        let path = os("/first/bin:/second/bin");
        let home = os("/home/zz99");
        let found = |present: &'static [&'static str]| {
            find_launcher_in(Some(path.as_os_str()), Some(home.as_os_str()), move |candidate| {
                present.iter().any(|p| Path::new(p) == candidate)
            })
        };
        assert_eq!(
            found(&["/second/bin/ai-masked", "/home/zz99/.local/bin/ai-masked"]),
            Some(PathBuf::from("/second/bin/ai-masked"))
        );
        assert_eq!(
            found(&["/first/bin/ai-masked", "/second/bin/ai-masked"]),
            Some(PathBuf::from("/first/bin/ai-masked"))
        );
        assert_eq!(
            found(&["/home/zz99/.local/bin/ai-masked"]),
            Some(PathBuf::from("/home/zz99/.local/bin/ai-masked"))
        );
        assert_eq!(found(&[]), None);
        // The harness itself being present is not the launcher.
        assert_eq!(found(&["/first/bin/claude"]), None);
    }

    #[test]
    fn the_launcher_is_never_taken_from_a_relative_path_entry() {
        let path = os(":.:relative/bin");
        let anything = |_: &Path| true;
        assert_eq!(find_launcher_in(Some(path.as_os_str()), None, anything), None);
        assert_eq!(find_launcher_in(None, Some(OsStr::new("")), anything), None);
        assert_eq!(find_launcher_in(None, None, anything), None);
    }
}
