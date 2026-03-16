// Continuum-Claude: Transparent wrapper for Claude Code CLI
// Logs all conversations to plain-text JSONL files while maintaining normal UX

use std::process::Stdio;
use color_eyre::{eyre::Context, Result};
use continuum_core::{PlainTextWriter, NoiseFilter};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // Get all arguments passed to continuum-claude
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Check if this is a non-interactive call (has --print or uses stdin)
    let is_print_mode = args.contains(&"--print".to_string());

    if is_print_mode {
        // Already in print mode, just wrap it
        run_with_logging(&args).await?;
    } else {
        // Interactive mode - pass through all arguments to real claude
        run_interactive_mode(&args).await?;
    }

    Ok(())
}

async fn run_with_logging(original_args: &[String]) -> Result<()> {
    // Check for no-save marker file
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let marker_path = std::path::Path::new(&home).join(".continuum-nosave");
    let skip_saving = marker_path.exists();

    if skip_saving {
        // Delete marker file immediately
        let _ = std::fs::remove_file(&marker_path);
        eprintln!("⚠ This conversation will NOT be saved to continuum logs");
    }

    // Build claude command with stream-json output
    let mut args = original_args.to_vec();

    // Ensure we have stream-json output
    if !args.contains(&"--output-format".to_string()) {
        args.push("--output-format".to_string());
        args.push("stream-json".to_string());
    }

    // Ensure verbose for full output
    if !args.contains(&"--verbose".to_string()) {
        args.push("--verbose".to_string());
    }

    eprintln!("Running: claude {}", args.join(" "));

    // Capture stdin if present (for user prompt logging)
    let user_prompt = if atty::isnt(atty::Stream::Stdin) {
        use std::io::Read;
        let mut buffer = String::new();
        std::io::stdin().read_to_string(&mut buffer)?;
        Some(buffer.trim().to_string())
    } else {
        None
    };

    // Spawn claude process
    let mut child = Command::new("claude")
        .args(&args)
        .stdin(if user_prompt.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn claude process")?;

    // If we captured stdin, write it to claude's stdin
    if let Some(ref prompt) = user_prompt {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.shutdown().await?;
        }
    }

    let stdout = child.stdout.take().expect("Failed to get stdout");
    let stderr = child.stderr.take().expect("Failed to get stderr");

    // Create plain-text writer and noise filter (only if saving)
    let writer = if !skip_saving {
        Some(PlainTextWriter::new()?)
    } else {
        None
    };
    let filter = NoiseFilter::new();

    let mut session_id: Option<String> = None;
    let mut session_start_time: Option<String> = None;
    let mut message_count: usize = 0;

    // Process stdout line by line
    let mut reader = BufReader::new(stdout).lines();

    while let Some(line) = reader.next_line().await? {
        // Print the line for user
        println!("{}", line);

        // Try to parse as JSON and log to plain-text
        if let Ok(event) = serde_json::from_str::<ClaudeEvent>(&line) {
            match event {
                ClaudeEvent::System { session_id: sid, .. } => {
                    let start_time = chrono::Utc::now().to_rfc3339();
                    session_id = Some(sid.clone());
                    session_start_time = Some(start_time.clone());

                    // Only log if we're saving
                    if let Some(ref writer) = writer {
                        // Extract date from start time
                        let date = PlainTextWriter::extract_date(Some(&start_time));

                        // Create session record (will be updated with message count later)
                        writer.write_session(
                            &sid,
                            "claude-code",
                            Some(&start_time),
                            None,
                            "active",
                            0,
                            &[],
                        )?;

                        // Log user prompt if we captured it from stdin
                        if let Some(ref prompt) = user_prompt {
                            // Apply noise filtering
                            if let Some(cleaned) = filter.filter(prompt) {
                                message_count += 1;
                                writer.append_message(
                                    &sid,
                                    "claude-code",
                                    &date,
                                    message_count,
                                    "user",
                                    &cleaned,
                                    Some(&start_time),
                                )?;
                            }
                        }
                    }
                }
                ClaudeEvent::User { message, session_id: sid, .. } => {
                    // Extract text content from user message
                    let content = message.content
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    // Apply noise filtering and log if saving
                    if let Some(cleaned) = filter.filter(&content) {
                        // Only log if we're saving
                        if let Some(ref writer) = writer {
                            let sess_id = session_id.as_ref().unwrap_or(&sid);
                            let timestamp = chrono::Utc::now().to_rfc3339();
                            let date = PlainTextWriter::extract_date(session_start_time.as_deref().or(Some(&timestamp)));

                            message_count += 1;
                            writer.append_message(
                                sess_id,
                                "claude-code",
                                &date,
                                message_count,
                                "user",
                                &cleaned,
                                Some(&timestamp),
                            )?;
                        }
                    }
                }
                ClaudeEvent::Assistant { message, session_id: sid, .. } => {
                    // Extract text content from message
                    let content = message.content
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    // Apply noise filtering and only log if content passes
                    if let Some(cleaned) = filter.filter(&content) {
                        // Only log if we're saving
                        if let Some(ref writer) = writer {
                            let sess_id = session_id.as_ref().unwrap_or(&sid);
                            let timestamp = chrono::Utc::now().to_rfc3339();
                            let date = PlainTextWriter::extract_date(session_start_time.as_deref().or(Some(&timestamp)));

                            message_count += 1;
                            writer.append_message(
                                sess_id,
                                "claude-code",
                                &date,
                                message_count,
                                "assistant",
                                &cleaned,
                                Some(&timestamp),
                            )?;
                        }
                    }
                }
                ClaudeEvent::Result { session_id: sid, .. } => {
                    // Only update metadata if we're saving
                    if let Some(ref writer) = writer {
                        let sess_id = session_id.as_ref().unwrap_or(&sid);
                        let end_time = chrono::Utc::now().to_rfc3339();
                        let date = PlainTextWriter::extract_date(session_start_time.as_deref());

                        // Update session metadata with final message count and closed status
                        let updates = serde_json::json!({
                            "status": "closed",
                            "end_time": end_time,
                            "message_count": message_count,
                        });

                        writer.update_session_metadata(
                            sess_id,
                            "claude-code",
                            &date,
                            updates,
                        )?;
                    }
                }
                _ => {} // Ignore other event types for now
            }
        }
    }

    // Forward stderr
    let mut stderr_reader = BufReader::new(stderr).lines();
    while let Some(line) = stderr_reader.next_line().await? {
        eprintln!("{}", line);
    }

    // Wait for process to complete
    let status = child.wait().await?;

    // Session saved silently - no prompt needed

    std::process::exit(status.code().unwrap_or(1));
}


async fn run_interactive_mode(args: &[String]) -> Result<()> {
    // Find the real claude binary (not the wrapper)
    let claude_path = which::which("claude")
        .context("Failed to find claude binary")?;

    // Resolve symlinks to get the actual binary path
    let resolved_claude = std::fs::canonicalize(&claude_path)
        .unwrap_or_else(|_| claude_path.clone());

    let claude_path_str = resolved_claude
        .to_str()
        .ok_or_else(|| color_eyre::eyre::eyre!("Invalid path"))?
        .to_string();

    // If the found claude IS this wrapper, search for the real claude binary
    let real_claude = if claude_path_str.contains("continuum-claude") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());

        // Try to find real binary by checking standard locations
        // Ordered for cross-platform compatibility (Linux-first, then macOS)
        let fallback_paths = [
            "/usr/bin/claude".to_string(),                              // Linux standard
            "/usr/local/bin/claude".to_string(),                        // User install (both platforms)
            format!("{}/.local/bin/claude-real", home),                 // Backed up binary
            format!("{}/.local/share/claude/bin/claude", home),         // User install (version-agnostic)
            "/opt/homebrew/bin/claude".to_string(),                     // macOS Homebrew (Apple Silicon)
            "/opt/homebrew/opt/claude/bin/claude".to_string(),          // macOS Homebrew alternate
        ];

        // Also check for version-specific install by scanning directory
        let version_dir = std::path::PathBuf::from(&home).join(".local/share/claude/versions");
        let mut version_binary: Option<String> = None;
        if version_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&version_dir) {
                // Find latest version directory
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.file_name().map(|n| !n.to_string_lossy().contains("continuum")).unwrap_or(false) {
                        version_binary = Some(path.to_string_lossy().to_string());
                        break;
                    }
                }
            }
        }

        // Try version-specific path first if found, then fallbacks
        if let Some(ref vpath) = version_binary {
            if std::path::Path::new(vpath).exists() {
                vpath.clone()
            } else {
                fallback_paths
                    .iter()
                    .find(|path| std::path::Path::new(path).exists())
                    .ok_or_else(|| color_eyre::eyre::eyre!(
                        "Could not find real claude binary. Tried: {} and {:?}",
                        fallback_paths.join(", "),
                        version_binary
                    ))?
                    .to_string()
            }
        } else {
            fallback_paths
                .iter()
                .find(|path| std::path::Path::new(path).exists())
                .ok_or_else(|| color_eyre::eyre::eyre!(
                    "Could not find real claude binary. Tried: {}",
                    fallback_paths.join(", ")
                ))?
                .to_string()
        }
    } else {
        claude_path_str
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

    // Get the most recently modified session file BEFORE running claude
    let projects_dir = std::path::PathBuf::from(&home).join(".claude/projects");

    let before_session = find_latest_session_file(&projects_dir);

    // Spawn claude as a child process (not exec) so we can capture the session after
    let status = Command::new(&real_claude)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn claude process")?
        .wait()
        .await?;

    // After claude exits, find the session that was just modified
    let after_session = find_latest_session_file(&projects_dir);

    if skip_saving {
        // Ephemeral mode: delete the session from Claude's storage too
        if let Some(session_path) = after_session {
            if before_session.as_ref() != Some(&session_path) {
                if let Err(e) = std::fs::remove_file(&session_path) {
                    eprintln!("⚠ Warning: Failed to delete session file: {}", e);
                } else {
                    eprintln!("✗ Session deleted (ephemeral mode)");
                }
            }
        }
    } else {
        // Normal mode: import to continuum logs
        if let Some(session_path) = after_session {
            if before_session.as_ref() != Some(&session_path) {
                eprintln!("\n📝 Importing session to continuum logs...");
                match import_session_to_continuum(&session_path) {
                    Ok(_) => {
                        // Silently saved - no prompt needed
                    }
                    Err(e) => {
                        eprintln!("⚠ Warning: Failed to import session: {}", e);
                    }
                }
            }
        }
    }

    std::process::exit(status.code().unwrap_or(1))
}

fn find_latest_session_file(projects_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::time::SystemTime;

    if !projects_dir.exists() {
        return None;
    }

    let mut latest: Option<(std::path::PathBuf, SystemTime)> = None;

    // Walk through all project directories
    if let Ok(entries) = std::fs::read_dir(projects_dir) {
        for project_entry in entries.flatten() {
            let project_dir = project_entry.path();
            if !project_dir.is_dir() {
                continue;
            }

            // Find session files in this project
            if let Ok(files) = std::fs::read_dir(&project_dir) {
                for file_entry in files.flatten() {
                    let file_path = file_entry.path();

                    // Skip agent files
                    if let Some(filename) = file_path.file_name().and_then(|s| s.to_str()) {
                        if filename.starts_with("agent-") {
                            continue;
                        }
                    }

                    // Only process .jsonl files
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

    latest.map(|(path, _)| path)
}

fn hash_content(role: &str, content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    role.hash(&mut hasher);
    content.hash(&mut hasher);
    hasher.finish()
}

fn import_session_to_continuum(session_path: &std::path::Path) -> Result<()> {
    use continuum_core::{MessageCompressor, PlainTextWriter};
    use std::io::{BufRead, BufReader};

    let writer = PlainTextWriter::new()?;

    let session_id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let compressor = MessageCompressor::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut start_time: Option<String> = None;
    let mut skills: Vec<String> = Vec::new();

    // Read all messages from the session file, deduplicating content.
    // CC sessions with context compression re-serialize earlier messages,
    // causing massive duplication in long sessions without this check.
    let file = std::fs::File::open(session_path)
        .with_context(|| format!("Failed to open {}", session_path.display()))?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;

        #[derive(serde::Deserialize)]
        struct ClaudeCodeEntry {
            #[serde(rename = "type")]
            entry_type: String,
            message: Option<serde_json::Value>,
            timestamp: Option<String>,
        }

        let entry: ClaudeCodeEntry = serde_json::from_str(&line)?;

        // Capture first timestamp
        if start_time.is_none() {
            if let Some(ref ts) = entry.timestamp {
                start_time = Some(ts.clone());
            }
        }

        // Process user and assistant messages
        if entry.entry_type == "user" || entry.entry_type == "assistant" {
            if let Some(msg) = entry.message {
                let role = msg["role"].as_str().unwrap_or("");

                if role == "user" {
                    if let Some(content) = msg["content"].as_str() {
                        let hash = hash_content("user", content);
                        if seen.insert(hash) {
                            messages.push(("user".to_string(), content.to_string()));
                        }
                    }
                } else if role == "assistant" {
                    if let Some(content_array) = msg["content"].as_array() {
                        // Extract skills from Skill tool_use blocks
                        for block in content_array {
                            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                                && block.get("name").and_then(|v| v.as_str()) == Some("Skill")
                            {
                                if let Some(skill) =
                                    block.pointer("/input/skill").and_then(|v| v.as_str())
                                {
                                    if !skills.contains(&skill.to_string()) {
                                        skills.push(skill.to_string());
                                    }
                                }
                            }
                        }

                        // Process all content blocks: text and tool_use
                        for block in content_array {
                            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");

                            match block_type {
                                "text" => {
                                    if let Some(text) = block["text"].as_str() {
                                        if !text.is_empty() {
                                            let hash = hash_content("assistant-text", text);
                                            if seen.insert(hash) {
                                                messages.push(("assistant".to_string(), text.to_string()));
                                            }
                                        }
                                    }
                                }
                                "tool_use" => {
                                    let tool_name = block["name"].as_str().unwrap_or("unknown");
                                    let input = block.get("input")
                                        .map(|i| serde_json::to_string(i).unwrap_or_default())
                                        .unwrap_or_default();
                                    let tool_entry = format!(
                                        "TOOL_USE: {} -> {}",
                                        tool_name, input
                                    );
                                    let hash = hash_content("assistant-tool", &tool_entry);
                                    if seen.insert(hash) {
                                        messages.push(("assistant".to_string(), tool_entry));
                                    }
                                }
                                "tool_result" => {
                                    // Extract tool result content
                                    let output = match block.get("content") {
                                        Some(serde_json::Value::String(s)) => s.clone(),
                                        Some(serde_json::Value::Array(blocks)) => blocks
                                            .iter()
                                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                                            .collect::<Vec<_>>()
                                            .join("\n"),
                                        _ => String::new(),
                                    };
                                    if !output.is_empty() {
                                        // Truncate large outputs to keep logs manageable
                                        let truncated = if output.len() > 500 {
                                            format!("{}... [truncated]", &output[..500])
                                        } else {
                                            output
                                        };
                                        let result_entry = format!("TOOL_RESULT: {}", truncated);
                                        let hash = hash_content("user-result", &result_entry);
                                        if seen.insert(hash) {
                                            messages.push(("user".to_string(), result_entry));
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    // Compress messages (noise filter removes pleasantries/boilerplate)
    let compressed = compressor.compress_batch(&messages);
    let message_count = compressed.len();

    if message_count == 0 {
        return Err(color_eyre::eyre::eyre!("No messages to import"));
    }

    let timestamp = start_time.unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let date = PlainTextWriter::extract_date(Some(&timestamp));

    // Write session (overwrites session.json)
    let session_dir = writer.write_session(
        session_id,
        "claude-code",
        Some(&timestamp),
        None,
        "closed",
        message_count,
        &skills,
    )?;

    // Delete existing messages.jsonl before writing to prevent duplication
    // on session re-import (CC sessions can be resumed, triggering re-import).
    let messages_path = session_dir.join("messages.jsonl");
    if messages_path.exists() {
        std::fs::remove_file(&messages_path)
            .with_context(|| format!("Failed to remove old messages.jsonl: {}", messages_path.display()))?;
    }

    // Write messages
    for (idx, (role, content)) in compressed.iter().enumerate() {
        writer.append_message(
            session_id,
            "claude-code",
            &date,
            idx + 1,
            role,
            content,
            Some(&timestamp),
        )?;
    }

    eprintln!("✓ Saved {} messages to continuum logs", message_count);

    Ok(())
}

// Claude Code JSON event types
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
enum ClaudeEvent {
    System {
        subtype: String,
        session_id: String,
        cwd: String,
        tools: Vec<String>,
        model: String,
    },
    User {
        message: UserMessage,
        session_id: String,
        uuid: String,
    },
    Assistant {
        message: MessageData,
        session_id: String,
        uuid: String,
    },
    Result {
        subtype: String,
        is_error: bool,
        duration_ms: u64,
        session_id: String,
        total_cost_usd: f64,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize, Serialize)]
struct UserMessage {
    role: String,
    content: Vec<Content>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MessageData {
    model: String,
    id: String,
    role: String,
    content: Vec<Content>,
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
enum Content {
    Text { text: String },
    #[serde(other)]
    Other,
}
