// Goose adapter - reads from Goose's SQLite database

use std::path::PathBuf;
use color_eyre::{eyre::Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::LogAdapter;

pub struct GooseAdapter {
    db_path: PathBuf,
}

impl GooseAdapter {
    pub fn new() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let db_path = PathBuf::from(home).join(".local/share/goose/sessions/sessions.db");

        if !db_path.exists() {
            return Err(color_eyre::eyre::eyre!(
                "Goose database not found: {}",
                db_path.display()
            ));
        }

        Ok(GooseAdapter { db_path })
    }
}

impl LogAdapter for GooseAdapter {
    fn name(&self) -> &'static str {
        "goose"
    }

    fn find_latest_session(&self) -> Result<PathBuf> {
        // For Goose, we return the database path and session ID as a pseudo-path
        // The session ID will be extracted later
        let conn = Connection::open(&self.db_path)?;

        let session_id: String = conn.query_row(
            "SELECT id FROM sessions ORDER BY updated_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )?;

        // Return a pseudo-path that encodes both db and session
        // Format: /path/to/sessions.db#session_id
        let pseudo_path = format!("{}#{}", self.db_path.display(), session_id);
        Ok(PathBuf::from(pseudo_path))
    }

    fn stream_session(&self, path: &PathBuf) -> Result<Box<dyn Iterator<Item = Result<String>>>> {
        // Parse the pseudo-path to get session ID
        let path_str = path.to_string_lossy();
        let session_id = if let Some(hash_pos) = path_str.rfind('#') {
            &path_str[hash_pos + 1..]
        } else {
            return Err(color_eyre::eyre::eyre!("Invalid Goose session path"));
        };

        let conn = Connection::open(&self.db_path)?;

        let mut stmt = conn.prepare(
            "SELECT role, content_json, timestamp FROM messages
             WHERE session_id = ?1
             ORDER BY id ASC"
        )?;

        let messages: Vec<GooseMessage> = stmt.query_map([session_id], |row| {
            Ok(GooseMessage {
                role: row.get(0)?,
                content_json: row.get(1)?,
                timestamp: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

        // Convert to iterator of JSON strings (compatible with LogAdapter interface)
        let json_messages: Vec<Result<String>> = messages.into_iter()
            .map(|msg| {
                serde_json::to_string(&msg)
                    .map_err(|e| color_eyre::eyre::eyre!("JSON serialization error: {}", e))
            })
            .collect();

        Ok(Box::new(json_messages.into_iter()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GooseMessage {
    role: String,
    content_json: String,
    timestamp: Option<String>,
}

/// Parse Goose content_json to extract text
/// Goose stores content as JSON array with various content types
pub fn parse_goose_content(content_json: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct ContentItem {
        #[serde(rename = "type")]
        _content_type: Option<String>,
        text: Option<String>,
    }

    let items: Vec<ContentItem> = serde_json::from_str(content_json)
        .unwrap_or_else(|_| vec![]);

    let text = items
        .iter()
        .filter_map(|item| item.text.as_ref())
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_parse_goose_content() {
        let json = r#"[{"type":"text","text":"Hello world"}]"#;
        let result = parse_goose_content(json).unwrap();
        assert_eq!(result, "Hello world");

        let json_multi = r#"[{"type":"text","text":"Line 1"},{"type":"text","text":"Line 2"}]"#;
        let result_multi = parse_goose_content(json_multi).unwrap();
        assert_eq!(result_multi, "Line 1\nLine 2");
    }

    #[test]
    fn test_goose_adapter_with_mock_db() -> Result<()> {
        let temp_dir = tempdir()?;
        let db_path = temp_dir.path().join("test_goose.db");

        // Create mock Goose database
        let conn = Connection::open(&db_path)?;
        conn.execute(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                updated_at TEXT
            )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                session_id TEXT,
                role TEXT,
                content_json TEXT,
                timestamp TEXT
            )",
            [],
        )?;

        // Insert test session
        conn.execute(
            "INSERT INTO sessions (id, updated_at) VALUES ('test_session', '2025-11-09 12:00:00')",
            [],
        )?;

        // Insert test messages
        conn.execute(
            "INSERT INTO messages (session_id, role, content_json, timestamp)
             VALUES ('test_session', 'user', '[{\"type\":\"text\",\"text\":\"Hello Goose\"}]', '2025-11-09 12:00:01')",
            [],
        )?;
        conn.execute(
            "INSERT INTO messages (session_id, role, content_json, timestamp)
             VALUES ('test_session', 'assistant', '[{\"type\":\"text\",\"text\":\"Hello! How can I help you?\"}]', '2025-11-09 12:00:02')",
            [],
        )?;

        drop(conn);

        // Create adapter pointing to test DB
        let adapter = GooseAdapter {
            db_path: db_path.clone(),
        };

        // Test find_latest_session
        let session_path = adapter.find_latest_session()?;
        let path_str = session_path.to_string_lossy();
        assert!(path_str.contains("test_goose.db"));
        assert!(path_str.contains("#test_session"));

        // Test stream_session
        let messages: Vec<String> = adapter
            .stream_session(&session_path)?
            .collect::<Result<Vec<_>>>()?;

        assert_eq!(messages.len(), 2);

        // Parse first message
        let msg1: GooseMessage = serde_json::from_str(&messages[0])?;
        assert_eq!(msg1.role, "user");
        let text1 = parse_goose_content(&msg1.content_json)?;
        assert_eq!(text1, "Hello Goose");

        // Parse second message
        let msg2: GooseMessage = serde_json::from_str(&messages[1])?;
        assert_eq!(msg2.role, "assistant");
        let text2 = parse_goose_content(&msg2.content_json)?;
        assert_eq!(text2, "Hello! How can I help you?");

        Ok(())
    }
}
