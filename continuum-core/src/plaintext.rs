// Plain-text JSONL export functionality
// Writes sessions and messages to ~/Assistants/continuum-logs directory structure

use color_eyre::{eyre::Context, Result};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Plain-text session writer
pub struct PlainTextWriter {
    base_dir: PathBuf,
}

impl PlainTextWriter {
    /// Create a new writer with default base directory
    pub fn new() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let base_dir = PathBuf::from(home).join("Assistants").join("continuum-logs");
        Ok(PlainTextWriter { base_dir })
    }

    /// Create a new writer with custom base directory
    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        PlainTextWriter { base_dir }
    }

    /// Get the directory path for a session
    fn session_dir(&self, assistant: &str, date: &str, session_id: &str) -> PathBuf {
        self.base_dir.join(assistant).join(date).join(session_id)
    }

    /// Extract date from timestamp (handles both ISO8601 and SQLite formats)
    pub fn extract_date(timestamp: Option<&str>) -> String {
        if let Some(ts) = timestamp {
            // Handle ISO8601 format (YYYY-MM-DDTHH:MM:SS...)
            if ts.contains('T') {
                if let Some(date) = ts.split('T').next() {
                    return date.to_string();
                }
            }

            // Handle SQLite format (YYYY-MM-DD HH:MM:SS)
            if ts.contains(' ') {
                if let Some(date) = ts.split(' ').next() {
                    return date.to_string();
                }
            }

            // If no separators, return as-is
            ts.to_string()
        } else {
            // Default to today
            chrono::Utc::now().format("%Y-%m-%d").to_string()
        }
    }

    /// Write session metadata
    pub fn write_session(
        &self,
        session_id: &str,
        assistant: &str,
        start_time: Option<&str>,
        end_time: Option<&str>,
        status: &str,
        message_count: usize,
    ) -> Result<PathBuf> {
        let date = Self::extract_date(start_time);
        let session_dir = self.session_dir(assistant, &date, session_id);

        // Create directory
        fs::create_dir_all(&session_dir)
            .with_context(|| format!("Failed to create directory: {}", session_dir.display()))?;

        // Write session.json
        let session_json_path = session_dir.join("session.json");
        let created_at = chrono::Utc::now().to_rfc3339();

        let metadata = json!({
            "id": session_id,
            "assistant": assistant,
            "start_time": start_time,
            "end_time": end_time,
            "status": status,
            "message_count": message_count,
            "created_at": created_at,
        });

        let mut file = fs::File::create(&session_json_path)
            .with_context(|| format!("Failed to create {}", session_json_path.display()))?;
        serde_json::to_writer_pretty(&mut file, &metadata)?;

        Ok(session_dir)
    }

    /// Append a message to the messages.jsonl file
    pub fn append_message(
        &self,
        session_id: &str,
        assistant: &str,
        date: &str,
        message_id: usize,
        role: &str,
        content: &str,
        timestamp: Option<&str>,
    ) -> Result<()> {
        let session_dir = self.session_dir(assistant, date, session_id);
        let messages_path = session_dir.join("messages.jsonl");

        // Create directory if it doesn't exist
        fs::create_dir_all(&session_dir)
            .with_context(|| format!("Failed to create directory: {}", session_dir.display()))?;

        // Open file in append mode
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&messages_path)
            .with_context(|| format!("Failed to open {}", messages_path.display()))?;

        // Write message as JSONL
        let message = json!({
            "id": message_id,
            "role": role,
            "content": content,
            "timestamp": timestamp,
        });

        serde_json::to_writer(&mut file, &message)?;
        writeln!(file)?;

        Ok(())
    }

    /// Update session metadata (useful for updating message count, end time, etc.)
    pub fn update_session_metadata(
        &self,
        session_id: &str,
        assistant: &str,
        date: &str,
        updates: serde_json::Value,
    ) -> Result<()> {
        let session_dir = self.session_dir(assistant, date, session_id);
        let session_json_path = session_dir.join("session.json");

        // Read existing metadata
        let existing: serde_json::Value = if session_json_path.exists() {
            let content = fs::read_to_string(&session_json_path)?;
            serde_json::from_str(&content)?
        } else {
            json!({})
        };

        // Merge updates
        let mut merged = existing.as_object().unwrap().clone();
        if let Some(updates_obj) = updates.as_object() {
            for (key, value) in updates_obj {
                merged.insert(key.clone(), value.clone());
            }
        }

        // Write back
        let mut file = fs::File::create(&session_json_path)?;
        serde_json::to_writer_pretty(&mut file, &merged)?;

        Ok(())
    }

    /// Get the base directory
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_extract_date_iso8601() {
        assert_eq!(
            PlainTextWriter::extract_date(Some("2025-11-09T14:30:00Z")),
            "2025-11-09"
        );
    }

    #[test]
    fn test_extract_date_sqlite() {
        assert_eq!(
            PlainTextWriter::extract_date(Some("2025-11-09 14:30:00")),
            "2025-11-09"
        );
    }

    #[test]
    fn test_write_session() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let writer = PlainTextWriter::with_base_dir(temp_dir.path().to_path_buf());

        let session_dir = writer.write_session(
            "test-session-001",
            "test-assistant",
            Some("2025-11-09T14:00:00Z"),
            None,
            "active",
            0,
        )?;

        assert!(session_dir.join("session.json").exists());
        Ok(())
    }

    #[test]
    fn test_append_message() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let writer = PlainTextWriter::with_base_dir(temp_dir.path().to_path_buf());

        writer.append_message(
            "test-session-001",
            "test-assistant",
            "2025-11-09",
            1,
            "user",
            "Test message",
            Some("2025-11-09T14:00:00Z"),
        )?;

        let messages_path = temp_dir
            .path()
            .join("test-assistant/2025-11-09/test-session-001/messages.jsonl");
        assert!(messages_path.exists());

        let content = fs::read_to_string(messages_path)?;
        assert!(content.contains("Test message"));
        Ok(())
    }
}
