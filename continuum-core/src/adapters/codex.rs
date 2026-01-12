// Codex log adapter

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use color_eyre::{eyre::Context, Result};

use super::LogAdapter;

pub struct CodexAdapter;

impl CodexAdapter {
    pub fn new() -> Self {
        CodexAdapter
    }
}

impl LogAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn find_latest_session(&self) -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let sessions_dir = PathBuf::from(home).join(".codex/sessions");

        if !sessions_dir.exists() {
            return Err(color_eyre::eyre::eyre!(
                "Codex sessions directory not found: {}",
                sessions_dir.display()
            ));
        }

        // Find latest session file by scanning date directories
        let mut latest: Option<(PathBuf, std::time::SystemTime)> = None;

        for year_entry in std::fs::read_dir(&sessions_dir)? {
            let year_dir = year_entry?.path();
            if !year_dir.is_dir() {
                continue;
            }

            for month_entry in std::fs::read_dir(&year_dir)? {
                let month_dir = month_entry?.path();
                if !month_dir.is_dir() {
                    continue;
                }

                for day_entry in std::fs::read_dir(&month_dir)? {
                    let day_dir = day_entry?.path();
                    if !day_dir.is_dir() {
                        continue;
                    }

                    for file_entry in std::fs::read_dir(&day_dir)? {
                        let file_path = file_entry?.path();
                        if file_path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                            let metadata = std::fs::metadata(&file_path)?;
                            let modified = metadata.modified()?;

                            if latest.is_none() || modified > latest.as_ref().unwrap().1 {
                                latest = Some((file_path, modified));
                            }
                        }
                    }
                }
            }
        }

        latest
            .map(|(path, _)| path)
            .ok_or_else(|| color_eyre::eyre::eyre!("No Codex session files found"))
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
