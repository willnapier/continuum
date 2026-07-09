// Grok CLI log adapter
// Reads from ~/.grok/sessions/<url-encoded-cwd>/<session-id>/chat_history.jsonl
// (override the base with GROK_HOME, per Grok's own docs).
//
// Mirrors adapters/claude_code.rs. Two structural differences from Claude Code:
//   1. Sessions nest one level deeper: <encoded-cwd>/<uuid>/chat_history.jsonl
//      (Claude Code is <project>/<uuid>.jsonl). The transcript filename is fixed;
//      the session id is the *parent directory* name (a UUIDv7).
//   2. Each line is discriminated by a top-level `type`, not a nested message.role.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use color_eyre::{eyre::Context, Result};

use super::LogAdapter;

pub struct GrokCliAdapter;

impl GrokCliAdapter {
    pub fn new() -> Self {
        GrokCliAdapter
    }

    /// ~/.grok (or $GROK_HOME) / sessions
    fn sessions_root() -> Result<PathBuf> {
        let base = match std::env::var("GROK_HOME") {
            Ok(h) if !h.is_empty() => PathBuf::from(h),
            _ => {
                let home = std::env::var("HOME").context("HOME not set")?;
                PathBuf::from(home).join(".grok")
            }
        };
        Ok(base.join("sessions"))
    }
}

impl LogAdapter for GrokCliAdapter {
    fn name(&self) -> &'static str {
        "grok-cli"
    }

    fn find_latest_session(&self) -> Result<PathBuf> {
        let root = Self::sessions_root()?;
        if !root.exists() {
            return Err(color_eyre::eyre::eyre!(
                "Grok sessions directory not found: {}",
                root.display()
            ));
        }

        // Newest <encoded-cwd>/<uuid>/chat_history.jsonl by mtime.
        let mut latest: Option<(PathBuf, std::time::SystemTime)> = None;

        for cwd_entry in std::fs::read_dir(&root)? {
            let cwd_dir = cwd_entry?.path();
            if !cwd_dir.is_dir() {
                continue; // skips session_search.sqlite, prompt_history.jsonl
            }
            for sess_entry in std::fs::read_dir(&cwd_dir)? {
                let sess_dir = sess_entry?.path();
                if !sess_dir.is_dir() {
                    continue;
                }
                let chat = sess_dir.join("chat_history.jsonl");
                if !chat.is_file() {
                    continue;
                }
                let modified = std::fs::metadata(&chat)?.modified()?;
                if latest.as_ref().map_or(true, |(_, m)| modified > *m) {
                    latest = Some((chat, modified));
                }
            }
        }

        latest
            .map(|(path, _)| path)
            .ok_or_else(|| color_eyre::eyre::eyre!("No Grok session files found"))
    }

    fn stream_session(&self, path: &PathBuf) -> Result<Box<dyn Iterator<Item = Result<String>>>> {
        let file = File::open(path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        Ok(Box::new(reader.lines().map(|line| {
            line.map_err(|e| color_eyre::eyre::eyre!("Failed to read line: {}", e))
        })))
    }
}
