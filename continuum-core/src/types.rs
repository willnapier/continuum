// Core type definitions for Continuum

use serde::{Deserialize, Serialize};

/// Role of a message in a conversation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

/// Normalized message format used internally
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub timestamp: Option<String>,
}

/// Session status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Closed,
    Compacted,
}

/// Session metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub assistant: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub status: SessionStatus,
}

// Codex-specific log format types
// These will eventually move to adapters/codex.rs

#[derive(Debug, Deserialize)]
pub struct CodexLogEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub timestamp: Option<String>,
    pub payload: Option<CodexPayload>,
}

#[derive(Debug, Deserialize)]
pub struct CodexPayload {
    pub role: Option<String>,
    pub content: Option<Vec<CodexContent>>,
}

#[derive(Debug, Deserialize)]
pub struct CodexContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: Option<String>,
}
