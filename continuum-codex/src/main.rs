// Continuum-Codex: Transparent wrapper for Codex CLI
// Automatically captures all conversations to plain-text JSONL files

use std::process::{Command, Stdio};
use color_eyre::{eyre::Context, Result};

fn main() -> Result<()> {
    color_eyre::install()?;

    // Get all arguments passed to continuum-codex
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Find the real codex binary
    let codex_path = which::which("codex")
        .context("Failed to find codex binary")?;

    // Resolve symlinks to get the actual binary path
    let resolved_codex = std::fs::canonicalize(&codex_path)
        .unwrap_or_else(|_| codex_path.clone());

    let codex_path_str = resolved_codex
        .to_str()
        .ok_or_else(|| color_eyre::eyre::eyre!("Invalid path"))?
        .to_string();

    // If the found codex IS this wrapper, search for the real codex binary
    let real_codex = if codex_path_str.contains("continuum-codex") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());

        // Try common installation locations (Linux-first for platform neutrality)
        let fallback_paths = [
            "/usr/bin/codex".to_string(),                               // Linux standard (pacman, apt)
            "/usr/local/bin/codex".to_string(),                         // User install (both platforms)
            format!("{}/.local/bin/codex-real", home),                  // Backed up binary
            "/opt/homebrew/bin/codex".to_string(),                      // macOS Homebrew
            "/opt/homebrew/opt/codex/bin/codex".to_string(),            // macOS Homebrew alternate
        ];

        fallback_paths
            .iter()
            .find(|path| std::path::Path::new(path).exists())
            .ok_or_else(|| color_eyre::eyre::eyre!(
                "Could not find real codex binary. Tried: {}",
                fallback_paths.join(", ")
            ))?
            .to_string()
    } else {
        codex_path_str
    };

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
    let status = Command::new(&real_codex)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn codex process")?
        .wait()?;

    // After codex exits, find the session that was just modified
    let after_session = find_latest_session_file(&sessions_dir);

    // Import the session if it's different from before (and we're not skipping)
    let mut session_dir: Option<std::path::PathBuf> = None;
    if !skip_saving {
        if let Some(session_path) = after_session {
            if before_session.as_ref() != Some(&session_path) {
                eprintln!("\n📝 Importing session to continuum logs...");
                match import_session_to_continuum(&session_path) {
                    Ok(dir) => {
                        session_dir = Some(dir);
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
            // User chose to discard - delete the session directory
            let _ = std::fs::remove_dir_all(dir);
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

                                    if file_path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                                        if let Ok(metadata) = std::fs::metadata(&file_path) {
                                            if let Ok(modified) = metadata.modified() {
                                                if latest.is_none() || modified > latest.as_ref().unwrap().1 {
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

fn import_session_to_continuum(session_path: &std::path::Path) -> Result<std::path::PathBuf> {
    use continuum_core::{CodexLogEntry, MessageCompressor, PlainTextWriter, LoopDetector, LoopSeverity};
    use std::io::{BufRead, BufReader};

    let writer = PlainTextWriter::new()?;

    let session_id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let compressor = MessageCompressor::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    let start_time = chrono::Utc::now().to_rfc3339();

    // Read all messages from the session file
    let file = std::fs::File::open(session_path)
        .with_context(|| format!("Failed to open {}", session_path.display()))?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        let entry: CodexLogEntry = serde_json::from_str(&line)?;

        if entry.entry_type == "response_item" {
            if let Some(ref payload) = entry.payload {
                if let Some(ref role) = payload.role {
                    if let Some(ref content_array) = payload.content {
                        let text = content_array
                            .iter()
                            .filter_map(|c| c.text.as_deref())
                            .collect::<Vec<_>>()
                            .join("");

                        messages.push((role.clone(), text));
                    }
                }
            }
        }
    }

    // Compress messages
    let compressed = compressor.compress_batch(&messages);
    let message_count = compressed.len();

    if message_count == 0 {
        return Err(color_eyre::eyre::eyre!("No messages to import"));
    }

    // Loop detection - analyze messages before writing
    let detector = LoopDetector::new();
    let detections = detector.analyze(&messages);

    // Report any detected loops
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

    let date = PlainTextWriter::extract_date(Some(&start_time));

    // Write session
    let session_dir = writer.write_session(
        session_id,
        "codex",
        Some(&start_time),
        None,
        "closed",
        message_count,
    )?;

    // Write messages
    for (idx, (role, content)) in compressed.iter().enumerate() {
        writer.append_message(
            session_id,
            "codex",
            &date,
            idx + 1,
            role,
            content,
            Some(&start_time),
        )?;
    }

    eprintln!("✓ Saved {} messages to continuum logs", message_count);

    Ok(session_dir)
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
