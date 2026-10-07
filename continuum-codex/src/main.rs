// Continuum-Codex: Transparent wrapper for Codex CLI
// Automatically captures all conversations to plain-text JSONL files
//
// IMPORTANT: Never install this binary (or a symlink to it) as `codex` on PATH
// ahead of the real OpenAI CLI. Self-detection used to rely only on the path
// containing "continuum-codex", so a copy named `~/.local/bin/codex` re-spawned
// itself forever. Resolution now skips our own executable by identity.

use color_eyre::{eyre::Context, Result};
use std::process::{Command, Stdio};

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

    // Check for no-save marker file
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let marker_path = std::path::Path::new(&home).join(".continuum-nosave");
    let skip_saving = marker_path.exists();

    if skip_saving {
        // Delete marker file immediately
        let _ = std::fs::remove_file(&marker_path);
        eprintln!("⚠ This conversation will NOT be saved to continuum logs");
    }

    // Get the most recently modified session file BEFORE running codex
    let sessions_dir = std::path::PathBuf::from(&home).join(".codex/sessions");

    let before_session = find_latest_session_file(&sessions_dir);

    // Spawn codex as a child process
    let mut command = Command::new(&real_codex);
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
        .context("Failed to spawn codex process")?;
    let status = child.wait()?;

    // After codex exits, find the session that was just modified
    let after_session = find_latest_session_file(&sessions_dir);

    // Import the session if it's different from before (and we're not skipping)
    let mut session_dir: Option<std::path::PathBuf> = None;
    if skip_saving {
        // The hooks register a no-save session as it runs. This backstop
        // covers a hook that failed or is not trusted yet: it registers the
        // session, so the Codex timer never stores it later, and deletes any
        // copy stored before then.
        if let Some(session_path) = after_session.filter(|p| before_session.as_ref() != Some(p)) {
            match discard(&session_path) {
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
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| color_eyre::eyre::eyre!("no session id in {}", path.display()))?;
    continuum_core::import::discard_session(&continuum_core::PlainTextWriter::new()?, "codex", stem)
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
