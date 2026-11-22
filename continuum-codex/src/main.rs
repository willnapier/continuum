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

    if skip_saving {
        eprintln!("⚠ This conversation will NOT be saved to continuum logs");
    }

    // Get the most recently modified session file BEFORE running codex
    let sessions_dir = std::path::PathBuf::from(&home).join(".codex/sessions");

    let before_session = find_latest_session_file(&sessions_dir);

    // Spawn codex as a child process
    let mut command = launch.command();
    configure_clipboard(&mut command);
    if skip_saving {
        // Seen by the per-turn Stop hook through codex's environment.
        command.env(continuum_core::import::NOSAVE_ENV, "1");
    }
    let mut child = command
        .args(&args)
        .env(continuum_core::codex_cli::CODEX_DEPTH_ENV, "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| continuum_core::masked_launch::spawn_failure("codex"))?;
    let status = child.wait()?;

    // Settle the no-save request and decide whether there is a session of
    // this start to work on (`nosave_request::AfterChild`). This wrapper has
    // no evidence of its own that ties a session to its child (it only knows
    // the newest session file, which may be another Codex's), so it passes
    // none. After a launcher code or a signal there is then nothing to import
    // or discard here: the newest session file below would be some other
    // session's. A real Codex session that ended that way is still recorded
    // by the hooks and the timer.
    let after = nosave_request::settle_at_exit(nosave, status.code(), false);
    if !after.session_of_this_start {
        std::process::exit(status.code().unwrap_or(1));
    }

    // After codex exits, find the session that was just modified
    let after_session = find_latest_session_file(&sessions_dir);

    // Import the session if it's different from before (and we're not skipping)
    let mut session_dir: Option<std::path::PathBuf> = None;
    if skip_saving {
        // The hooks register a no-save session as it runs. This backstop
        // covers a hook that failed or is not trusted yet: it registers the
        // session, so the Codex timer never stores it later, and for a
        // session started here deletes any copy stored before then. A resumed
        // or forked session's earlier record was saved deliberately.
        let resumed = args.iter().any(|a| a == "resume" || a == "fork");
        if let Some(session_path) = after_session.filter(|p| before_session.as_ref() != Some(p)) {
            let result = if resumed { register_only(&session_path) } else { discard(&session_path) };
            match result {
                Ok(0) => {}
                Ok(n) => eprintln!("✗ Removed {n} stored copy(ies) of this no-save session"),
                Err(e) => eprintln!("⚠ Warning: could not mark the session no-save: {e}"),
            }
        }
    } else {
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

/// Register a Codex session as no-save without touching stored copies.
fn register_only(path: &std::path::Path) -> Result<usize> {
    continuum_core::import::register_nosave(&continuum_core::PlainTextWriter::new()?, rollout_stem(path)?)?;
    Ok(0)
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
