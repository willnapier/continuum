// Continuum-Codex: Transparent wrapper for Codex CLI
// Automatically captures all conversations to plain-text JSONL files
//
// IMPORTANT: Never install this binary (or a symlink to it) as `codex` on PATH
// ahead of the real OpenAI CLI. Self-detection used to rely only on the path
// containing "continuum-codex", so a copy named `~/.local/bin/codex` re-spawned
// itself forever. Resolution now skips our own executable by identity.

use color_eyre::{eyre::Context, Result};
use continuum_core::nosave_request::{self, NoSaveRequest};
use std::io::IsTerminal;
use std::process::{Command, Stdio};

/// Persistent Zellij panes can acquire remote clients after launch. Codex 0.163
/// does not recognize Zellij as a reason to forward successful native copies.
/// Make its native backend unavailable so it uses its normal OSC 52 fallback.
fn configure_clipboard(command: &mut Command) {
    if cfg!(target_os = "linux")
        && std::env::var_os("ZELLIJ").is_some_and(|value| !value.is_empty())
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("CONTINUUM_CODEX_NATIVE_CLIPBOARD").as_deref() != Ok("1")
    {
        // Empty values deliberately prevent default-display discovery. Removing
        // WAYLAND_DISPLAY alone can reconnect to $XDG_RUNTIME_DIR/wayland-0.
        // Apply only to the child; never change the launcher's process environment.
        command.env("DISPLAY", "").env("WAYLAND_DISPLAY", "");
        command.env_remove("WAYLAND_SOCKET");
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;

    if std::env::var_os(continuum_core::codex_cli::CODEX_DEPTH_ENV).is_some() {
        color_eyre::eyre::bail!(
            "refusing recursive continuum-codex launch; check the resolved real Codex path"
        );
    }

    // Get all arguments passed to continuum-codex
    let args: Vec<String> = std::env::args().skip(1).collect();

    let self_exe = std::env::current_exe().ok();
    let resolution = continuum_core::codex_cli::resolve_codex(self_exe.as_deref())?;
    if !resolution.is_managed() {
        eprintln!(
            "Warning: Codex resolved from {} at {}; run `continuum codex update` to install the managed copy",
            resolution.source,
            resolution.path.display()
        );
    }
    let real_codex = resolution.path.to_string_lossy().into_owned();

    // On Linux Codex starts through the mask launcher, `ai-masked <codex> …`,
    // which hides the protected folders and then becomes Codex in the same
    // process; on macOS it starts directly, as before
    // (`continuum_core::masked_launch`, ~/Assistants/shared/AI-FOOTING-MASK.md).
    // A missing launcher is an error here: there is no direct start to fall
    // back to. The resolver above is unaffected: it has already chosen the
    // real Codex by path, and the depth guard below still reaches Codex
    // through the launcher.
    let launch = continuum_core::masked_launch::harness_launch(&real_codex)
        .map_err(|reason| color_eyre::eyre::eyre!(reason))?;

    // Reserve the no-save request, if there is one; do not use it up yet. A
    // launcher that is found may still refuse (exit 113), not be runnable, or
    // be ended by a signal before its exec. The request is used up only when
    // the child exited with a status that is not one of the launcher's own,
    // which shows that Codex ran. It is given back when the spawn fails, when
    // the child exits 113, 126 or 127, and when a signal ends the child
    // (`continuum_core::nosave_request`). A Codex that itself ends in one of
    // those ways after a real session gets its request back too, so the next
    // session is not saved either: the mistake is made in the safe direction.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let nosave = NoSaveRequest::reserve(std::path::Path::new(&home));
    let skip_saving = nosave.is_some();

    // Where Codex writes its rollouts: under CODEX_HOME when that is set, as
    // the hooks read it, else ~/.codex.
    let sessions_dir = std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(&home).join(".codex"))
        .join("sessions");

    // A no-save start does not rely on Codex's hooks to stay out of
    // Continuum (`continuum_core::codex_nosave_start`). Before Codex runs, a
    // resumed session is registered by its rollout file and thread id, and
    // the start is recorded, so that no import stores a rollout made while
    // it is open. When Codex has ended, however it ended, every rollout made
    // since the start is registered and any stored copy of it removed. If
    // any of that cannot be done, Codex is not started and the request is
    // kept.
    let mut nosave_start = None;
    if skip_saving {
        // Every place this Codex may write a rollout: the sessions folder it
        // is started with, and ~/.codex's when CODEX_HOME points elsewhere
        // (an app-server already running may have been started without it).
        let mut sessions_dirs = vec![sessions_dir.clone()];
        let default_dir = std::path::PathBuf::from(&home).join(".codex/sessions");
        if default_dir != sessions_dir {
            sessions_dirs.push(default_dir);
        }
        match open_no_save_start(&args, &sessions_dirs) {
            Ok(start) => nosave_start = Some(start),
            Err(refusal) => {
                nosave_request::settle(nosave, false);
                eprintln!("✗ Codex was not started: {refusal:#}");
                eprintln!("↩ The no-save request is kept for the next start");
                std::process::exit(2);
            }
        }
        eprintln!("⚠ This conversation will NOT be saved to continuum logs");
    }

    // Get the most recently modified session file BEFORE running codex
    let before_session = find_latest_session_file(&sessions_dir);

    // Spawn codex as a child process
    let mut command = launch.command();
    configure_clipboard(&mut command);
    if skip_saving {
        // Seen by the per-turn Stop hook through codex's environment.
        command.env(continuum_core::import::NOSAVE_ENV, "1");
    }
    command
        .args(&args)
        .env(continuum_core::codex_cli::CODEX_DEPTH_ENV, "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let spawn_failure = || continuum_core::masked_launch::spawn_failure("codex");
    let mut child = match nosave_start.as_mut() {
        Some(start) => match start.spawn_child(&mut command, spawn_failure) {
            Ok(child) => child,
            Err(error) => {
                // A Codex that was spawned and then ended again, because it
                // could not be recorded, may have written a rollout: close the
                // start (or leave it for an import to close) before failing.
                if let Some(start) = nosave_start.take().filter(|start| start.child_spawned()) {
                    if let Err(close) = continuum_core::PlainTextWriter::new().and_then(|writer| start.finish(&writer)) {
                        eprintln!("⚠ Warning: could not close this no-save start; the next import will: {close:#}");
                    }
                }
                return Err(error);
            }
        },
        None => command.spawn().with_context(spawn_failure)?,
    };
    let status = child.wait()?;

    // A no-save start is closed first, before anything is printed: the
    // terminal may be gone.
    let closed = nosave_start.map(|start| {
        continuum_core::PlainTextWriter::new().and_then(|writer| start.finish(&writer))
    });

    // Settle the no-save request and decide whether there is a session of
    // this start to work on (`nosave_request::AfterChild`). This wrapper has
    // no evidence of its own that ties a session to its child (it only knows
    // the newest session file, which may be another Codex's), so it passes
    // none. After a launcher code or a signal there is then nothing to import
    // here: the newest session file below would be some other session's.
    let after = nosave_request::settle_at_exit(nosave, status.code(), false);
    match closed {
        Some(Ok(closed)) if closed.registered > 0 => eprintln!(
            "✗ Marked {} Codex session(s) begun during this no-save start as no-save{}",
            closed.registered,
            if closed.removed > 0 { format!("; removed {} stored copy(ies)", closed.removed) } else { String::new() }
        ),
        Some(Err(e)) => eprintln!("⚠ Warning: could not close this no-save start; the next import will: {e:#}"),
        _ => {}
    }
    if !after.session_of_this_start || skip_saving {
        std::process::exit(status.code().unwrap_or(1));
    }

    // After codex exits, find the session that was just modified
    let after_session = find_latest_session_file(&sessions_dir);

    // Import the session if it's different from before
    let mut session_dir: Option<std::path::PathBuf> = None;
    if let Some(session_path) = after_session {
        if before_session.as_ref() != Some(&session_path) {
            eprintln!("\n📝 Importing session to continuum logs...");
            match import_session_to_continuum(&session_path) {
                Ok(dir) => {
                    session_dir = dir;
                }
                Err(e) => {
                    eprintln!("⚠ Warning: Failed to import session: {}", e);
                }
            }
        }
    }

    // Post-conversation review prompt (if session was saved)
    if let Some(ref dir) = session_dir {
        if !prompt_save_conversation()? {
            // User chose to discard. Register it as no-save too, so neither
            // the timer nor a Stop hook stores it again.
            if let Err(e) = discard(dir) {
                eprintln!("⚠ Warning: could not mark the session no-save: {e}");
                let _ = std::fs::remove_dir_all(dir);
            }
            eprintln!("✗ Conversation discarded");
        } else {
            eprintln!("✓ Conversation saved");
        }
    }

    std::process::exit(status.code().unwrap_or(1))
}

/// Before a no-save Codex runs: register a resumed session, then record the
/// start (`continuum_core::codex_nosave_start`). A resume whose session its
/// arguments do not name for certain is refused: the rollout it would write
/// on cannot be known.
fn open_no_save_start(
    args: &[String],
    sessions_dirs: &[std::path::PathBuf],
) -> Result<continuum_core::codex_nosave_start::CodexNoSaveStart> {
    use continuum_core::codex_nosave_start::{self as start, CodexNoSaveStart, Resume};
    let writer = continuum_core::PlainTextWriter::new()?;
    match start::resume_of(args) {
        Resume::No => {}
        Resume::Of(thread) => {
            start::register_resumed(&writer, sessions_dirs, &thread)
                .context("the resumed session could not be marked no-save")?;
        }
        Resume::Unclear => color_eyre::eyre::bail!(
            "a no-save `codex resume` must name the session's id right after `resume`, and no other id \
             (codex resume <uuid> [prompt]); otherwise the session to keep out of Continuum cannot be known"
        ),
    }
    CodexNoSaveStart::open(&writer, sessions_dirs).context("the no-save start could not be recorded")
}

fn find_latest_session_file(sessions_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::time::SystemTime;

    if !sessions_dir.exists() {
        return None;
    }

    let mut latest: Option<(std::path::PathBuf, SystemTime)> = None;

    // Walk through YYYY/MM/DD directory structure
    if let Ok(year_entries) = std::fs::read_dir(sessions_dir) {
        for year_entry in year_entries.flatten() {
            let year_dir = year_entry.path();
            if !year_dir.is_dir() {
                continue;
            }

            if let Ok(month_entries) = std::fs::read_dir(&year_dir) {
                for month_entry in month_entries.flatten() {
                    let month_dir = month_entry.path();
                    if !month_dir.is_dir() {
                        continue;
                    }

                    if let Ok(day_entries) = std::fs::read_dir(&month_dir) {
                        for day_entry in day_entries.flatten() {
                            let day_dir = day_entry.path();
                            if !day_dir.is_dir() {
                                continue;
                            }

                            if let Ok(files) = std::fs::read_dir(&day_dir) {
                                for file_entry in files.flatten() {
                                    let file_path = file_entry.path();

                                    if file_path.extension().and_then(|s| s.to_str())
                                        == Some("jsonl")
                                    {
                                        if let Ok(metadata) = std::fs::metadata(&file_path) {
                                            if let Ok(modified) = metadata.modified() {
                                                if latest.is_none()
                                                    || modified > latest.as_ref().unwrap().1
                                                {
                                                    latest = Some((file_path, modified));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    latest.map(|(path, _)| path)
}

/// Import the wrapped session into Continuum with the shared importer
/// (`continuum_core::import`), which files it under the session's own start
/// date and replaces rather than appends, like the timer and the Stop hook.
/// `Ok(None)` for a session in the no-save registry.
fn import_session_to_continuum(session_path: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    use continuum_core::import::Imported;
    use continuum_core::{LoopSeverity, PlainTextWriter};

    let writer = PlainTextWriter::new()?;
    let (outcome, detections) = match continuum_core::import::import_codex(&writer, session_path)? {
        Imported::Stored(found) => found,
        Imported::Empty => return Err(color_eyre::eyre::eyre!("No messages to import")),
        Imported::NoSave => {
            eprintln!("This session is marked no-save; not saved to continuum logs");
            return Ok(None);
        }
        Imported::Held => {
            eprintln!(
                "This session began while a no-save Codex session was open, so it is not saved to continuum logs: \
                 it may be that session's"
            );
            return Ok(None);
        }
    };

    if !detections.is_empty() {
        eprintln!("\n⚠️  LOOP DETECTION WARNINGS ⚠️");
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        for detection in &detections {
            let icon = match detection.severity {
                LoopSeverity::Warning => "⚠️ ",
                LoopSeverity::Critical => "🚨",
            };
            eprintln!("{} {}", icon, detection.message);
        }
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        eprintln!("This may indicate an automation failure or runaway process.\n");
    }

    eprintln!("✓ Saved {} messages to continuum logs", outcome.message_count);
    Ok(Some(outcome.dir))
}

/// `continuum_core::import::discard_session` for a Codex session. `path` is
/// its rollout file or its Continuum record directory; both are named by the
/// rollout stem, which the importer checks alongside the thread id.
fn discard(path: &std::path::Path) -> Result<usize> {
    continuum_core::import::discard_session(&continuum_core::PlainTextWriter::new()?, "codex", rollout_stem(path)?)
}

fn rollout_stem(path: &std::path::Path) -> Result<&str> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| color_eyre::eyre::eyre!("no session id in {}", path.display()))
}

/// Prompt user whether to save the conversation
/// Returns true to save, false to discard
fn prompt_save_conversation() -> Result<bool> {
    use std::io::{self, Write};

    eprintln!("\n─────────────────────────────────────────");
    eprint!("Save this conversation? [Y/n] ");
    io::stderr().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    match input.trim().to_lowercase().as_str() {
        "n" | "no" => Ok(false),
        _ => Ok(true), // Default to save (Y or Enter)
    }
}
