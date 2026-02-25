// Continuum-Gemini: Transparent wrapper for Gemini CLI
// Automatically captures all conversations to plain-text JSONL files

use std::process::{Command, Stdio};
use color_eyre::{eyre::Context, Result};

fn main() -> Result<()> {
    color_eyre::install()?;

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Find the real gemini binary
    let gemini_path = which::which("gemini")
        .context("Failed to find gemini binary")?;

    let resolved_gemini = std::fs::canonicalize(&gemini_path)
        .unwrap_or_else(|_| gemini_path.clone());

    let gemini_path_str = resolved_gemini
        .to_str()
        .ok_or_else(|| color_eyre::eyre::eyre!("Invalid path"))?
        .to_string();

    // If the found gemini IS this wrapper, search for the real binary
    let real_gemini = if gemini_path_str.contains("continuum-gemini") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());

        let fallback_paths = [
            format!("{}/.local/bin/gemini-real", home),
            "/usr/local/bin/gemini".to_string(),
            "/usr/bin/gemini".to_string(),
            "/opt/homebrew/bin/gemini".to_string(),
            format!("{}/.npm-global/bin/gemini", home),
            // npm global install locations
            "/usr/local/lib/node_modules/@google/gemini-cli/bin/gemini".to_string(),
            format!("{}/.nvm/versions/node/current/bin/gemini", home),
        ];

        fallback_paths
            .iter()
            .find(|path| std::path::Path::new(path).exists())
            .ok_or_else(|| color_eyre::eyre::eyre!(
                "Could not find real gemini binary. Tried: {}",
                fallback_paths.join(", ")
            ))?
            .to_string()
    } else {
        gemini_path_str
    };

    // Check for no-save marker file
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let marker_path = std::path::Path::new(&home).join(".continuum-nosave");
    let skip_saving = marker_path.exists();

    if skip_saving {
        let _ = std::fs::remove_file(&marker_path);
        eprintln!("\u{26a0} This conversation will NOT be saved to continuum logs");
    }

    // Snapshot session files BEFORE running gemini
    let gemini_tmp = std::path::PathBuf::from(&home).join(".gemini/tmp");
    let before_sessions = snapshot_session_files(&gemini_tmp);

    // Spawn gemini as a child process
    let status = Command::new(&real_gemini)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn gemini process")?
        .wait()?;

    // After gemini exits, find new or modified session files
    if !skip_saving {
        let after_sessions = snapshot_session_files(&gemini_tmp);
        let new_sessions = find_changed_sessions(&before_sessions, &after_sessions);

        for session_path in new_sessions {
            eprintln!("\n\u{1f4dd} Importing session to continuum logs...");
            match import_session_to_continuum(&session_path) {
                Ok(dir) => {
                    if !prompt_save_conversation()? {
                        let _ = std::fs::remove_dir_all(dir);
                        eprintln!("\u{2717} Conversation discarded");
                    } else {
                        eprintln!("\u{2713} Conversation saved");
                    }
                }
                Err(e) => {
                    eprintln!("\u{26a0} Warning: Failed to import session: {}", e);
                }
            }
        }
    }

    std::process::exit(status.code().unwrap_or(1))
}

/// Snapshot of session files with their modification times
type SessionSnapshot = Vec<(std::path::PathBuf, std::time::SystemTime)>;

/// Collect all session JSON files under ~/.gemini/tmp/*/chats/
fn snapshot_session_files(gemini_tmp: &std::path::Path) -> SessionSnapshot {
    let mut sessions = Vec::new();

    if !gemini_tmp.exists() {
        return sessions;
    }

    // Walk through project directories
    if let Ok(projects) = std::fs::read_dir(gemini_tmp) {
        for project_entry in projects.flatten() {
            let chats_dir = project_entry.path().join("chats");
            if !chats_dir.is_dir() {
                continue;
            }

            if let Ok(files) = std::fs::read_dir(&chats_dir) {
                for file_entry in files.flatten() {
                    let path = file_entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("json") {
                        if let Ok(meta) = std::fs::metadata(&path) {
                            if let Ok(modified) = meta.modified() {
                                sessions.push((path, modified));
                            }
                        }
                    }
                }
            }
        }
    }

    sessions
}

/// Find sessions that are new or modified compared to the before snapshot
fn find_changed_sessions(
    before: &SessionSnapshot,
    after: &SessionSnapshot,
) -> Vec<std::path::PathBuf> {
    let before_map: std::collections::HashMap<&std::path::Path, &std::time::SystemTime> =
        before.iter().map(|(p, t)| (p.as_path(), t)).collect();

    after
        .iter()
        .filter(|(path, modified)| {
            match before_map.get(path.as_path()) {
                Some(old_modified) => modified > old_modified, // Modified
                None => true,                                   // New
            }
        })
        .map(|(path, _)| path.clone())
        .collect()
}

/// Gemini session JSON structure
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiSession {
    session_id: String,
    start_time: Option<String>,
    last_updated: Option<String>,
    messages: Vec<GeminiMessage>,
}

#[derive(serde::Deserialize)]
struct GeminiMessage {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    msg_type: String,
    content: serde_json::Value, // String for gemini/info, array for user
}

fn import_session_to_continuum(session_path: &std::path::Path) -> Result<std::path::PathBuf> {
    use continuum_core::{MessageCompressor, PlainTextWriter};

    let writer = PlainTextWriter::new()?;
    let compressor = MessageCompressor::new();

    // Read and parse the session JSON
    let raw = std::fs::read_to_string(session_path)
        .with_context(|| format!("Failed to read {}", session_path.display()))?;
    let session: GeminiSession = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", session_path.display()))?;

    // Extract user and gemini messages
    let mut messages: Vec<(String, String)> = Vec::new();

    for msg in &session.messages {
        match msg.msg_type.as_str() {
            "user" => {
                // Content is array of {text: "..."}
                if let Some(arr) = msg.content.as_array() {
                    let text: String = arr
                        .iter()
                        .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        messages.push(("user".to_string(), text));
                    }
                }
            }
            "gemini" => {
                // Content is a plain string
                if let Some(text) = msg.content.as_str() {
                    if !text.is_empty() {
                        messages.push(("assistant".to_string(), text.to_string()));
                    }
                }
            }
            // Skip "info" and other types
            _ => {}
        }
    }

    // Compress messages
    let compressed = compressor.compress_batch(&messages);
    let message_count = compressed.len();

    if message_count == 0 {
        return Err(color_eyre::eyre::eyre!("No messages to import"));
    }

    let start_time = session.start_time.as_deref()
        .unwrap_or_else(|| session.messages.first()
            .and_then(|m| m.timestamp.as_deref())
            .unwrap_or("unknown"));
    let end_time = session.last_updated.as_deref();
    let date = PlainTextWriter::extract_date(Some(start_time));

    // Write session metadata
    let session_dir = writer.write_session(
        &session.session_id,
        "gemini-cli",
        Some(start_time),
        end_time,
        "closed",
        message_count,
    )?;

    // Clear any existing messages.jsonl so resumed sessions don't duplicate
    let messages_path = session_dir.join("messages.jsonl");
    if messages_path.exists() {
        std::fs::remove_file(&messages_path)?;
    }

    // Write messages
    for (idx, (role, content)) in compressed.iter().enumerate() {
        let timestamp = session.messages
            .iter()
            .filter(|m| m.msg_type == "user" || m.msg_type == "gemini")
            .nth(idx)
            .and_then(|m| m.timestamp.as_deref());

        writer.append_message(
            &session.session_id,
            "gemini-cli",
            &date,
            idx + 1,
            role,
            content,
            timestamp,
        )?;
    }

    eprintln!("\u{2713} Saved {} messages to continuum logs", message_count);

    Ok(session_dir)
}

/// Prompt user whether to save the conversation
fn prompt_save_conversation() -> Result<bool> {
    use std::io::{self, Write};

    eprintln!("\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
    eprint!("Save this conversation? [Y/n] ");
    io::stderr().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    match input.trim().to_lowercase().as_str() {
        "n" | "no" => Ok(false),
        _ => Ok(true),
    }
}
