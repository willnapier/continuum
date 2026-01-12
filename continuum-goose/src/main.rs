// Continuum-Goose: Transparent wrapper for Goose CLI
// Automatically captures all conversations to plain-text JSONL files

use std::process::{Command, Stdio};
use color_eyre::{eyre::Context, Result};
use rusqlite::Connection;

fn main() -> Result<()> {
    color_eyre::install()?;

    // Get all arguments passed to continuum-goose
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Find the real goose binary
    let goose_path = which::which("goose")
        .context("Failed to find goose binary")?;

    // Resolve symlinks to get the actual binary path
    let resolved_goose = std::fs::canonicalize(&goose_path)
        .unwrap_or_else(|_| goose_path.clone());

    let goose_path_str = resolved_goose
        .to_str()
        .ok_or_else(|| color_eyre::eyre::eyre!("Invalid path"))?
        .to_string();

    // If the found goose IS this wrapper, search for the real goose binary
    let real_goose = if goose_path_str.contains("continuum-goose") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());

        // Try common installation locations (Linux-first for platform neutrality)
        let fallback_paths = [
            "/usr/bin/goose".to_string(),                       // Linux standard (pacman, apt)
            "/usr/local/bin/goose".to_string(),                 // User install (both platforms)
            format!("{}/.local/bin/goose-real", home),          // Backed up real binary
            format!("{}/.cargo/bin/goose", home),               // Cargo install
            "/opt/homebrew/bin/goose".to_string(),              // macOS Homebrew
            "/opt/homebrew/opt/goose/bin/goose".to_string(),    // macOS Homebrew alternate
        ];

        fallback_paths
            .iter()
            .find(|path| {
                let p = std::path::Path::new(path);
                p.exists() && !p.to_string_lossy().contains("continuum-goose")
            })
            .ok_or_else(|| color_eyre::eyre::eyre!(
                "Could not find real goose binary. Tried: {}",
                fallback_paths.join(", ")
            ))?
            .to_string()
    } else {
        goose_path_str
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

    // Get the latest session ID BEFORE running goose
    let db_path = std::path::PathBuf::from(&home).join(".local/share/goose/sessions/sessions.db");

    let before_session = find_latest_session_id(&db_path);

    // Spawn goose as a child process
    let status = Command::new(&real_goose)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn goose process")?
        .wait()?;

    // After goose exits, find the latest session ID
    let after_session = find_latest_session_id(&db_path);

    // Import the session if it's different from before (and we're not skipping)
    let mut session_dir: Option<std::path::PathBuf> = None;
    if !skip_saving {
        if let (Some(before), Some(after)) = (before_session, after_session.clone()) {
            if before != after {
                eprintln!("\n📝 Importing session to continuum logs...");
                match import_session_to_continuum(&db_path, &after) {
                    Ok(dir) => {
                        session_dir = Some(dir);
                    }
                    Err(e) => {
                        eprintln!("⚠ Warning: Failed to import session: {}", e);
                    }
                }
            }
        } else if let Some(after) = after_session {
            // First session ever
            eprintln!("\n📝 Importing session to continuum logs...");
            match import_session_to_continuum(&db_path, &after) {
                Ok(dir) => {
                    session_dir = Some(dir);
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
            // User chose to discard - delete the session directory
            let _ = std::fs::remove_dir_all(dir);
            eprintln!("✗ Conversation discarded");
        } else {
            eprintln!("✓ Conversation saved");
        }
    }

    std::process::exit(status.code().unwrap_or(1))
}

fn find_latest_session_id(db_path: &std::path::Path) -> Option<String> {
    if !db_path.exists() {
        return None;
    }

    let conn = Connection::open(db_path).ok()?;

    let session_id: String = conn
        .query_row(
            "SELECT id FROM sessions ORDER BY updated_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok()?;

    Some(session_id)
}

fn import_session_to_continuum(db_path: &std::path::Path, session_id: &str) -> Result<std::path::PathBuf> {
    use continuum_core::{MessageCompressor, PlainTextWriter};
    use continuum_core::adapters::goose::parse_goose_content;

    let writer = PlainTextWriter::new()?;
    let compressor = MessageCompressor::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    let start_time = chrono::Utc::now().to_rfc3339();

    // Query messages from database
    let conn = Connection::open(db_path)?;

    let mut stmt = conn.prepare(
        "SELECT role, content_json FROM messages
         WHERE session_id = ?1
         ORDER BY id ASC"
    )?;

    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
        ))
    })?;

    for row_result in rows {
        let (role, content_json) = row_result?;
        let content = parse_goose_content(&content_json)?;

        if !content.is_empty() {
            messages.push((role, content));
        }
    }

    // Compress messages
    let compressed = compressor.compress_batch(&messages);
    let message_count = compressed.len();

    if message_count == 0 {
        return Err(color_eyre::eyre::eyre!("No messages to import"));
    }

    let date = PlainTextWriter::extract_date(Some(&start_time));

    // Write session
    let session_dir = writer.write_session(
        session_id,
        "goose",
        Some(&start_time),
        None,
        "closed",
        message_count,
    )?;

    // Write messages
    for (idx, (role, content)) in compressed.iter().enumerate() {
        writer.append_message(
            session_id,
            "goose",
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
