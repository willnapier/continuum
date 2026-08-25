// Continuum CLI - Plain-Text Assistant Log Management
// Manages conversation logs stored as JSONL files in ~/Assistants/continuum-logs

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use color_eyre::{eyre::Context, Result};
use continuum_core::adapters::claude_code::ClaudeCodeAdapter;
use continuum_core::adapters::codex::CodexAdapter;
use continuum_core::adapters::goose::{parse_goose_content, GooseAdapter};
use continuum_core::{
    CodexLogEntry, LogAdapter, LoopDetector, LoopSeverity, MessageCompressor, PlainTextWriter,
};

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    match &cli.command {
        Command::Import(cmd) => handle_import(cmd)?,
        Command::Stats => handle_stats()?,
        Command::Usage(cmd) => handle_usage(cmd)?,
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "continuum",
    author,
    version,
    about = "Continuum: Plain-text assistant conversation logs",
    long_about = "Manage assistant conversations as plain-text JSONL files.\nUse Nushell functions for querying: continuum-search, continuum-timeline, continuum-stats"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Import sessions from assistant native logs to plain-text JSONL
    Import(ImportArgs),
    /// Show statistics about stored conversations
    Stats,
    /// Inspect or refresh assistant allocation usage
    Usage(UsageArgs),
}

#[derive(Args, Debug)]
struct UsageArgs {
    /// Assistant whose allocation should be inspected
    #[arg(default_value = "codex")]
    assistant: String,
    /// Fetch a fresh observation from the assistant
    #[arg(long)]
    refresh: bool,
    /// Deliver newly eligible desktop notifications
    #[arg(long, requires = "refresh")]
    notify: bool,
    /// Suppress the human-readable report
    #[arg(long)]
    quiet: bool,
    /// Source of this observation (manual, scheduled, session-start, session-exit)
    #[arg(long, default_value = "manual")]
    provenance: String,
}

fn handle_usage(args: &UsageArgs) -> Result<()> {
    let assistant = args.assistant.to_lowercase();
    if assistant != "codex" {
        color_eyre::eyre::bail!(
            "usage monitoring is not yet implemented for '{}'; currently supported: codex",
            args.assistant
        );
    }
    if args.refresh {
        let observation = continuum_core::usage::refresh_codex_usage(&args.provenance)?;
        if args.notify {
            continuum_core::usage::notify_transitions(&observation)?;
        }
    }
    if !args.quiet {
        println!("{}", continuum_core::usage::render_usage(&assistant)?);
    }
    Ok(())
}

#[derive(Args, Debug)]
struct ImportArgs {
    /// Assistant to import from (codex, goose)
    #[arg(short, long)]
    assistant: String,
    /// Session ID to import (uses adapter's latest if not specified)
    #[arg(short, long)]
    session: Option<String>,
    /// Output directory (default: ~/Assistants/continuum-logs)
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn handle_import(args: &ImportArgs) -> Result<()> {
    let writer = if let Some(ref output) = args.output {
        PlainTextWriter::with_base_dir(output.clone())
    } else {
        PlainTextWriter::new()?
    };

    let adapter_name = args.assistant.to_lowercase();

    match adapter_name.as_str() {
        "codex" => {
            let adapter = CodexAdapter::new();
            import_codex_session(&writer, &adapter, args)
        }
        "goose" => {
            let adapter = GooseAdapter::new()?;
            import_goose_session(&writer, &adapter, args)
        }
        "claude-code" => {
            let adapter = ClaudeCodeAdapter::new();
            import_claude_code_session(&writer, &adapter, args)
        }
        _ => {
            eprintln!(
                "Error: Unknown assistant '{}'. Supported: codex, goose, claude-code",
                args.assistant
            );
            std::process::exit(1);
        }
    }
}

fn import_codex_session(
    writer: &PlainTextWriter,
    adapter: &CodexAdapter,
    args: &ImportArgs,
) -> Result<()> {
    let session_path = if let Some(ref session) = args.session {
        PathBuf::from(session)
    } else {
        adapter.find_latest_session()?
    };

    eprintln!("Importing Codex session: {}", session_path.display());

    let session_id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let compressor = MessageCompressor::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    let start_time = chrono::Utc::now().to_rfc3339();

    // Read all messages
    for line_result in adapter.stream_session(&session_path)? {
        let line = line_result?;
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

    // Compress messages to remove noise
    let compressed = compressor.compress_batch(&messages);
    let message_count = compressed.len();

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
        eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    }

    // Extract date from start time
    let date = PlainTextWriter::extract_date(Some(&start_time));

    // Write session
    writer.write_session(
        session_id,
        "codex",
        Some(&start_time),
        None,
        "closed",
        message_count,
        &[],
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

    println!(
        "✓ Imported {} messages from Codex session: {}",
        message_count, session_id
    );
    println!(
        "  Location: {}",
        writer
            .base_dir()
            .join("codex")
            .join(&date)
            .join(session_id)
            .display()
    );

    Ok(())
}

fn import_goose_session(
    writer: &PlainTextWriter,
    adapter: &GooseAdapter,
    args: &ImportArgs,
) -> Result<()> {
    let session_path = if let Some(ref session) = args.session {
        // User provided session ID, construct pseudo-path
        let home = std::env::var("HOME").context("HOME not set")?;
        let db_path = PathBuf::from(home).join(".local/share/goose/sessions/sessions.db");
        PathBuf::from(format!("{}#{}", db_path.display(), session))
    } else {
        adapter.find_latest_session()?
    };

    // Extract session ID from pseudo-path
    let path_str = session_path.to_string_lossy();
    let session_id = if let Some(hash_pos) = path_str.rfind('#') {
        &path_str[hash_pos + 1..]
    } else {
        eprintln!("Error: Invalid Goose session path");
        std::process::exit(1);
    };

    eprintln!("Importing Goose session: {}", session_id);

    let compressor = MessageCompressor::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    let start_time = chrono::Utc::now().to_rfc3339();

    // Read all messages
    for msg_result in adapter.stream_session(&session_path)? {
        let msg_json = msg_result?;

        #[derive(serde::Deserialize)]
        struct GooseMessage {
            role: String,
            content_json: String,
        }

        let msg: GooseMessage = serde_json::from_str(&msg_json)?;
        let content = parse_goose_content(&msg.content_json)?;

        if !content.is_empty() {
            messages.push((msg.role, content));
        }
    }

    // Compress messages
    let compressed = compressor.compress_batch(&messages);
    let message_count = compressed.len();

    if message_count == 0 {
        eprintln!("⚠ No messages found in Goose session: {}", session_id);
        return Ok(());
    }

    // Extract date
    let date = PlainTextWriter::extract_date(Some(&start_time));

    // Write session
    writer.write_session(
        session_id,
        "goose",
        Some(&start_time),
        None,
        "closed",
        message_count,
        &[],
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

    println!(
        "✓ Imported {} messages from Goose session: {}",
        message_count, session_id
    );
    println!(
        "  Location: {}",
        writer
            .base_dir()
            .join("goose")
            .join(&date)
            .join(session_id)
            .display()
    );

    Ok(())
}

fn import_claude_code_session(
    writer: &PlainTextWriter,
    adapter: &ClaudeCodeAdapter,
    args: &ImportArgs,
) -> Result<()> {
    let session_path = if let Some(ref session) = args.session {
        PathBuf::from(session)
    } else {
        adapter.find_latest_session()?
    };

    ensure_claude_session_importable(&session_path, &clinical_session_marker_dir()?)?;

    eprintln!("Importing Claude Code session: {}", session_path.display());

    let session_id = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let compressor = MessageCompressor::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    let mut start_time: Option<String> = None;
    let mut skills: Vec<String> = Vec::new();

    // Read all messages
    for line_result in adapter.stream_session(&session_path)? {
        let line = line_result?;

        #[derive(serde::Deserialize)]
        struct ClaudeCodeEntry {
            #[serde(rename = "type")]
            entry_type: String,
            message: Option<serde_json::Value>,
            timestamp: Option<String>,
        }

        let entry: ClaudeCodeEntry = serde_json::from_str(&line)?;

        // Capture first timestamp as session start time
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
                    // User messages have content as a string
                    if let Some(content) = msg["content"].as_str() {
                        messages.push(("user".to_string(), content.to_string()));
                    }
                } else if role == "assistant" {
                    // Assistant messages have content as an array
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

                        let text = content_array
                            .iter()
                            .filter_map(|c| {
                                // Only include "text" type, skip "thinking"
                                if c["type"].as_str() == Some("text") {
                                    c["text"].as_str().map(String::from)
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        if !text.is_empty() {
                            messages.push(("assistant".to_string(), text));
                        }
                    }
                }
            }
        }
    }

    // Compress messages to remove noise
    let compressed = compressor.compress_batch(&messages);
    let message_count = compressed.len();

    if message_count == 0 {
        eprintln!("⚠ No messages found in Claude Code session: {}", session_id);
        return Ok(());
    }

    // Use captured timestamp or fallback to current time
    let timestamp = start_time.unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let date = PlainTextWriter::extract_date(Some(&timestamp));

    // Write session
    writer.write_session(
        session_id,
        "claude-code",
        Some(&timestamp),
        None,
        "closed",
        message_count,
        &skills,
    )?;

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

    println!(
        "✓ Imported {} messages from Claude Code session: {}",
        message_count, session_id
    );
    println!(
        "  Location: {}",
        writer
            .base_dir()
            .join("claude-code")
            .join(&date)
            .join(session_id)
            .display()
    );

    Ok(())
}

fn clinical_session_marker_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "Refusing Claude Code import: HOME is unavailable, so the clinical-session registry cannot be checked"
        )
    })?;
    Ok(PathBuf::from(home).join(".local/share/continuum/claude-clinical-sessions"))
}

fn ensure_claude_session_importable(session_path: &Path, marker_dir: &Path) -> Result<()> {
    if !marker_dir.is_dir() {
        return Err(color_eyre::eyre::eyre!(
            "Refusing Claude Code import: clinical-session registry is unavailable at {}",
            marker_dir.display()
        ));
    }

    let session_id = session_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("Refusing Claude Code import: invalid session path")
        })?;

    if marker_dir.join(session_id).is_file() {
        return Err(color_eyre::eyre::eyre!(
            "Refusing to import protected cc-clinical session {session_id}"
        ));
    }

    Ok(())
}

fn handle_stats() -> Result<()> {
    println!("\n📊 Continuum Statistics\n");
    println!("To view detailed statistics, use the Nushell function:");
    println!("  continuum-stats\n");
    println!("To search conversations:");
    println!("  continuum-search \"your query\"\n");
    println!("To view timeline:");
    println!("  continuum-timeline 2025-11-09\n");
    println!("📍 Log location: ~/Assistants/continuum-logs/\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ensure_claude_session_importable;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn claude_import_fails_closed_without_clinical_registry() {
        let temp = tempdir().unwrap();
        let missing_registry = temp.path().join("missing");
        let session = temp
            .path()
            .join("11111111-1111-1111-1111-111111111111.jsonl");

        let error = ensure_claude_session_importable(&session, &missing_registry).unwrap_err();
        assert!(error.to_string().contains("registry is unavailable"));
    }

    #[test]
    fn claude_import_refuses_registered_clinical_session() {
        let temp = tempdir().unwrap();
        let registry = temp.path().join("registry");
        fs::create_dir(&registry).unwrap();
        fs::write(registry.join("11111111-1111-1111-1111-111111111111"), []).unwrap();
        let session = temp
            .path()
            .join("11111111-1111-1111-1111-111111111111.jsonl");

        let error = ensure_claude_session_importable(&session, &registry).unwrap_err();
        assert!(error.to_string().contains("protected cc-clinical session"));
    }

    #[test]
    fn claude_import_allows_unregistered_session() {
        let temp = tempdir().unwrap();
        let registry = temp.path().join("registry");
        fs::create_dir(&registry).unwrap();
        let session = temp
            .path()
            .join("22222222-2222-2222-2222-222222222222.jsonl");

        ensure_claude_session_importable(&session, &registry).unwrap();
    }
}
