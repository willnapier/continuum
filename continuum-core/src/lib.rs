// Continuum Core Library
// Shared types, adapters, and plain-text storage for assistant session management

pub mod adapters;
pub mod codex_cli;
pub mod compression;
pub mod loop_detection;
pub mod plaintext;
pub mod types;

// Re-export commonly used types
pub use adapters::LogAdapter;
pub use compression::{MessageCompressor, NoiseFilter};
pub use loop_detection::{LoopDetection, LoopDetector, LoopSeverity};
pub use plaintext::PlainTextWriter;
pub use types::*;
