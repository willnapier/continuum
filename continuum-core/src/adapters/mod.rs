// Adapter traits and implementations for different assistant log formats

use color_eyre::Result;
use std::path::PathBuf;

pub mod claude_code;
pub mod codex;
pub mod goose;

/// Trait for adapting different assistant log formats into Continuum's format
pub trait LogAdapter {
    /// Name of the adapter (e.g., "codex", "claude", "goose")
    fn name(&self) -> &'static str;

    /// Find the latest active session for this assistant
    fn find_latest_session(&self) -> Result<PathBuf>;

    /// Stream messages from a session file
    /// Returns an iterator of parsed log entries
    fn stream_session(&self, path: &PathBuf) -> Result<Box<dyn Iterator<Item = Result<String>>>>;
}
