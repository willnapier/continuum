// Claude Code log adapter
// Reads from ~/.claude/projects/<project>/<sessionId>.jsonl files

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use color_eyre::{eyre::Context, Result};

use super::LogAdapter;

pub struct ClaudeCodeAdapter;

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        ClaudeCodeAdapter
    }
}

impl LogAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn find_latest_session(&self) -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let claude_dir = PathBuf::from(home).join(".claude/projects");

        if !claude_dir.exists() {
            return Err(color_eyre::eyre::eyre!(
                "Claude Code projects directory not found: {}",
                claude_dir.display()
            ));
        }

        // Find latest session file across all project directories
        let mut latest: Option<(PathBuf, std::time::SystemTime)> = None;

        for project_entry in std::fs::read_dir(&claude_dir)? {
            let project_dir = project_entry?.path();
            if !project_dir.is_dir() {
                continue;
            }

            for file_entry in std::fs::read_dir(&project_dir)? {
                let file_path = file_entry?.path();

                // Skip files that start with "agent-" (those are agent-specific logs)
                if let Some(filename) = file_path.file_name().and_then(|s| s.to_str()) {
                    if filename.starts_with("agent-") {
                        continue;
                    }
                }

                // Only process UUID.jsonl files (session files)
                if file_path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    let metadata = std::fs::metadata(&file_path)?;
                    let modified = metadata.modified()?;

                    if latest.is_none() || modified > latest.as_ref().unwrap().1 {
                        latest = Some((file_path, modified));
                    }
                }
            }
        }

        latest
            .map(|(path, _)| path)
            .ok_or_else(|| color_eyre::eyre::eyre!("No Claude Code session files found"))
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
