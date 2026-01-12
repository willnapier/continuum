// Continuum Core Library
// Shared types, adapters, and plain-text storage for assistant session management

pub mod types;
pub mod adapters;
pub mod compression;
pub mod plaintext;
pub mod loop_detection;

// Re-export commonly used types
pub use types::*;
pub use adapters::LogAdapter;
pub use compression::{NoiseFilter, MessageCompressor};
pub use plaintext::PlainTextWriter;
pub use loop_detection::{LoopDetector, LoopDetection, LoopSeverity};
