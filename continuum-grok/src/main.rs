// continuum-grok — import Grok CLI sessions into continuum-logs/grok-cli/.
//
// DESIGN NOTE: unlike `continuum-claude` (a live process wrapper that diffs
// ~/.claude/projects before/after each run), this is a SCAN-AND-IMPORT tool.
// Grok persists rich state to disk automatically (chat_history.jsonl +
// summary.json per session, per its own docs), so importing after the fact is
// simpler and more complete than wrapping the TUI — it also captures headless
// `grok -p` runs and resumed sessions. Idempotent: each import deletes+rewrites
// messages.jsonl, so re-running is safe.
//
// Invocation (resolution order):
//   continuum-grok                 # hook path: import $GROK_SESSION_ID if set, else latest
//   continuum-grok <session-id>    # import a specific session
//   continuum-grok --latest        # import the most-recently-active session
//   continuum-grok --all           # import every session (backfill)
//
// Wired as a Grok `SessionEnd` hook (~/.grok/hooks/continuum-session-end.json),
// which sets GROK_SESSION_ID so we import exactly the session that just ended.
//
// Reasoning/thinking is intentionally NOT imported — only user turns, assistant
// answers, tool calls, and tool results.

use color_eyre::{eyre::Context, Result};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use continuum_core::{MessageCompressor, PlainTextWriter};

const SOURCE: &str = "grok-cli";
const TOOL_RESULT_TRUNCATE: usize = 500;

enum Target {
    Id(String),
    Latest,
    All,
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let target = if args.iter().any(|a| a == "--all") {
        Target::All
    } else if let Some(id) = args.iter().find(|a| !a.starts_with("--")) {
        Target::Id(id.clone())
    } else if args.iter().any(|a| a == "--latest") {
        Target::Latest
    } else if let Ok(id) = std::env::var("GROK_SESSION_ID") {
        if id.is_empty() { Target::Latest } else { Target::Id(id) }
    } else {
        Target::Latest
    };

    let root = grok_sessions_root()?;
    let targets: Vec<PathBuf> = match target {
        Target::All => all_chat_histories(&root),
        Target::Latest => find_latest_chat_history(&root).into_iter().collect(),
        Target::Id(id) => find_session_by_id(&root, &id).into_iter().collect(),
    };

    if targets.is_empty() {
        eprintln!("continuum-grok: no matching Grok session under {}", root.display());
        return Ok(());
    }

    let mut imported = 0usize;
    for chat in &targets {
        match import_grok_session(chat) {
            Ok(n) => {
                imported += 1;
                eprintln!("✓ {} ({} messages)", session_id_of(chat), n);
            }
            Err(e) => eprintln!("⚠ skipped {}: {}", chat.display(), e),
        }
    }
    eprintln!("continuum-grok: imported {}/{} session(s)", imported, targets.len());
    Ok(())
}

fn grok_sessions_root() -> Result<PathBuf> {
    let base = match std::env::var("GROK_HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from(std::env::var("HOME").context("HOME not set")?).join(".grok"),
    };
    Ok(base.join("sessions"))
}

/// The session id is the parent-dir name (a UUIDv7).
fn session_id_of(chat_history: &Path) -> String {
    chat_history
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn find_latest_chat_history(root: &Path) -> Option<PathBuf> {
    all_chat_histories(root)
        .into_iter()
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?.modified().ok()?;
            Some((p, m))
        })
        .max_by_key(|(_, m)| *m)
        .map(|(p, _)| p)
}

/// Locate a session dir by id across all workspace groups. Scanning avoids
/// reimplementing Grok's cwd URL-encoding (and its >255-byte slug+hash fallback).
fn find_session_by_id(root: &Path, id: &str) -> Option<PathBuf> {
    let cwds = std::fs::read_dir(root).ok()?;
    for cwd in cwds.flatten() {
        let cwd = cwd.path();
        if !cwd.is_dir() {
            continue;
        }
        let chat = cwd.join(id).join("chat_history.jsonl");
        if chat.is_file() {
            return Some(chat);
        }
    }
    None
}

fn all_chat_histories(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(cwds) = std::fs::read_dir(root) else { return out };
    for cwd in cwds.flatten() {
        let cwd = cwd.path();
        if !cwd.is_dir() {
            continue; // skip session_search.sqlite / prompt_history.jsonl
        }
        let Ok(sessions) = std::fs::read_dir(&cwd) else { continue };
        for sess in sessions.flatten() {
            let chat = sess.path().join("chat_history.jsonl");
            if chat.is_file() {
                out.push(chat);
            }
        }
    }
    out
}

// ---- one Grok chat_history.jsonl line -------------------------------------

#[derive(serde::Deserialize)]
struct GrokLine {
    #[serde(rename = "type")]
    kind: String,
    content: Option<serde_json::Value>,
    tool_calls: Option<Vec<GrokToolCall>>,
    // `reasoning` / `model_id` / `tool_call_id` exist on some lines but are
    // deliberately not deserialized — reasoning is excluded from the transcript.
}

#[derive(serde::Deserialize)]
struct GrokToolCall {
    name: Option<String>,
    arguments: Option<String>,
}

/// summary.json — Grok chat lines carry no timestamp, so start_time comes here.
#[derive(serde::Deserialize, Default)]
struct GrokSummary {
    created_at: Option<String>,
}

fn push_msg(
    messages: &mut Vec<(String, String)>,
    seen: &mut std::collections::HashSet<u64>,
    role: &str,
    tag: &str,
    text: String,
) {
    if text.is_empty() {
        return;
    }
    if seen.insert(hash_content(tag, &text)) {
        messages.push((role.to_string(), text));
    }
}

fn import_grok_session(chat_history: &Path) -> Result<usize> {
    let session_dir = chat_history
        .parent()
        .ok_or_else(|| color_eyre::eyre::eyre!("no parent dir"))?;
    let session_id = session_id_of(chat_history);

    let summary: GrokSummary = std::fs::read_to_string(session_dir.join("summary.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let start_time = summary
        .created_at
        .unwrap_or_else(|| mtime_rfc3339(chat_history));

    // Parse messages, deduping on content (parity with the claude importer:
    // compaction can re-serialise earlier turns).
    let mut messages: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let file = std::fs::File::open(chat_history)
        .with_context(|| format!("open {}", chat_history.display()))?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<GrokLine>(&line) else { continue };

        match entry.kind.as_str() {
            "user" => {
                if let Some(serde_json::Value::Array(blocks)) = &entry.content {
                    let text = blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    push_msg(&mut messages, &mut seen, "user", "user", text);
                }
            }
            "assistant" => {
                if let Some(serde_json::Value::String(s)) = &entry.content {
                    push_msg(&mut messages, &mut seen, "assistant", "assistant-text", s.clone());
                }
                for tc in entry.tool_calls.iter().flatten() {
                    let text = format!(
                        "TOOL_USE: {} -> {}",
                        tc.name.as_deref().unwrap_or("unknown"),
                        tc.arguments.as_deref().unwrap_or("")
                    );
                    push_msg(&mut messages, &mut seen, "assistant", "assistant-tool", text);
                }
            }
            "tool_result" => {
                if let Some(serde_json::Value::String(s)) = &entry.content {
                    let text = format!("TOOL_RESULT: {}", truncate_chars(s, TOOL_RESULT_TRUNCATE));
                    push_msg(&mut messages, &mut seen, "user", "user-result", text);
                }
            }
            _ => {} // "system", "backend_tool_call" → skip
        }
    }

    let compressed = MessageCompressor::new().compress_batch(&messages);
    if compressed.is_empty() {
        return Err(color_eyre::eyre::eyre!("no messages to import"));
    }

    let writer = PlainTextWriter::new()?;
    let date = PlainTextWriter::extract_date(Some(&start_time));

    // Creates continuum-logs/grok-cli/<date>/<uuid>/ and returns that dir.
    let out_session_dir = writer.write_session(
        &session_id,
        SOURCE,
        Some(&start_time),
        None,
        "closed",
        compressed.len(),
        &[],
    )?;

    // Idempotent re-import: drop the old messages.jsonl before rewriting.
    let messages_path = out_session_dir.join("messages.jsonl");
    if messages_path.exists() {
        std::fs::remove_file(&messages_path).ok();
    }

    for (idx, (role, content)) in compressed.iter().enumerate() {
        writer.append_message(&session_id, SOURCE, &date, idx + 1, role, content, Some(&start_time))?;
    }
    Ok(compressed.len())
}

fn hash_content(tag: &str, content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut h);
    content.hash(&mut h);
    h.finish()
}

/// Char-boundary-safe truncation (the claude importer's `&s[..500]` can panic
/// on a multibyte boundary — avoided here).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}... [truncated]")
}

fn mtime_rfc3339(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        .unwrap_or_else(|_| chrono::Utc::now().to_rfc3339())
}
