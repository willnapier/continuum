// Compression and noise filtering for messages
// Removes boilerplate, pleasantries, and redundant content

use regex::Regex;

/// Noise filter for cleaning messages before storage or context emission
pub struct NoiseFilter {
    // Common pleasantry patterns
    pleasantries: Vec<Regex>,
    // System boilerplate patterns
    boilerplate: Vec<Regex>,
    // Empty acknowledgments
    acknowledgments: Vec<Regex>,
}

impl NoiseFilter {
    pub fn new() -> Self {
        Self {
            pleasantries: vec![
                // Simple standalone pleasantries
                Regex::new(r"(?i)^(please|thank you|thanks|sure|ok|okay|got it|understood|great|awesome|perfect|excellent|nice|good)\s*[.!]?\s*$").unwrap(),
                // Enthusiasm that adds no information
                Regex::new(r"(?i)^(this is (all )?(very )?(great|amazing|exciting|wonderful|fantastic|perfect|excellent)|how (cool|neat|nice|great)|very (cool|nice|exciting|interesting))[.!]*\s*$").unwrap(),
                // Polite prefixes that add no value
                Regex::new(r"(?i)^(if you (don't mind|could|would like)|would you like me to|let me|i'll|i will|i can)").unwrap(),
                // Polite suffixes
                Regex::new(r"(?i)(let me know if you (need|want|would like)|is there anything else|anything else i can help).*$").unwrap(),
            ],
            boilerplate: vec![
                // Environment context blocks
                Regex::new(r"<environment_context>[\s\S]*?</environment_context>").unwrap(),
                // System reminders
                Regex::new(r"<system-reminder>[\s\S]*?</system-reminder>").unwrap(),
                // Tool result wrappers (keep content, remove wrapper)
                Regex::new(r"<system>Tool ran without output or errors</system>").unwrap(),
            ],
            acknowledgments: vec![
                // Empty acknowledgments that just confirm
                Regex::new(r"(?i)^(i understand|i see|i got it|understood|noted|will do|on it|done)\s*[.!]?\s*$").unwrap(),
            ],
        }
    }

    /// Filter out noise from message content
    /// Returns cleaned content, or None if message is entirely noise
    pub fn filter(&self, content: &str) -> Option<String> {
        let mut cleaned = content.to_string();

        // Remove boilerplate blocks first
        for pattern in &self.boilerplate {
            cleaned = pattern.replace_all(&cleaned, "").to_string();
        }

        // Trim whitespace
        cleaned = cleaned.trim().to_string();

        // Check if entire message is just a pleasantry
        for pattern in &self.pleasantries {
            if pattern.is_match(&cleaned) {
                return None; // Entirely noise
            }
        }

        // Check if entire message is just an acknowledgment
        for pattern in &self.acknowledgments {
            if pattern.is_match(&cleaned) {
                return None; // Entirely noise
            }
        }

        // If nothing left after filtering, consider it noise
        if cleaned.is_empty() || cleaned.len() < 3 {
            return None;
        }

        Some(cleaned)
    }

    /// Check if a message is likely just noise
    pub fn is_noise(&self, content: &str) -> bool {
        self.filter(content).is_none()
    }

    /// Get approximate token savings from filtering
    /// Rough estimate: 1 token ~= 4 characters
    pub fn token_savings(&self, original: &str, filtered: Option<&str>) -> usize {
        let original_tokens = (original.len() + 3) / 4;
        let filtered_tokens = filtered.map(|s| (s.len() + 3) / 4).unwrap_or(0);
        original_tokens.saturating_sub(filtered_tokens)
    }
}

impl Default for NoiseFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Message compressor that combines filtering and batching
pub struct MessageCompressor {
    filter: NoiseFilter,
}

impl MessageCompressor {
    pub fn new() -> Self {
        Self {
            filter: NoiseFilter::new(),
        }
    }

    /// Compress a batch of messages by filtering noise
    /// Returns vector of (role, cleaned_content) tuples
    pub fn compress_batch(&self, messages: &[(String, String)]) -> Vec<(String, String)> {
        messages
            .iter()
            .filter_map(|(role, content)| {
                self.filter.filter(content).map(|cleaned| {
                    (role.clone(), cleaned)
                })
            })
            .collect()
    }

    /// Estimate total tokens for a batch of messages
    /// Uses: ~5 tokens for role prefix + ~4 chars per token for content
    pub fn estimate_tokens(&self, messages: &[(String, String)]) -> usize {
        messages.iter()
            .map(|(_role, content)| {
                // Role prefix adds ~5 tokens, content is ~4 chars per token
                5 + ((content.len() + 3) / 4)
            })
            .sum()
    }

    /// Calculate compression ratio as percentage
    pub fn compression_ratio(&self, original_tokens: usize, compressed_tokens: usize) -> f64 {
        if original_tokens == 0 {
            return 0.0;
        }
        (1.0 - (compressed_tokens as f64 / original_tokens as f64)) * 100.0
    }
}

impl Default for MessageCompressor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_simple_pleasantries() {
        let filter = NoiseFilter::new();

        assert_eq!(filter.filter("please"), None);
        assert_eq!(filter.filter("Thank you!"), None);
        assert_eq!(filter.filter("okay"), None);
        assert_eq!(filter.filter("Great!"), None);

        // Real content should pass through
        let result = filter.filter("Here's the code you requested");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "Here's the code you requested");
    }

    #[test]
    fn test_filter_enthusiasm() {
        let filter = NoiseFilter::new();

        // Enthusiasm without substance should be filtered
        assert_eq!(filter.filter("this is all very exciting"), None);
        assert_eq!(filter.filter("This is amazing!"), None);
        assert_eq!(filter.filter("How cool!"), None);
        assert_eq!(filter.filter("Very interesting!"), None);

        // But enthusiasm WITH content should be preserved
        let result = filter.filter("This is exciting because it reduces tokens by 20%");
        assert!(result.is_some());
        assert!(result.unwrap().contains("reduces tokens"));
    }

    #[test]
    fn test_filter_boilerplate() {
        let filter = NoiseFilter::new();

        let input = "<environment_context>\n  <cwd>/home/test</cwd>\n</environment_context>\nHello there";
        let result = filter.filter(input);
        assert_eq!(result.unwrap(), "Hello there");

        let input2 = "<system-reminder>Some reminder</system-reminder>Actual content";
        let result2 = filter.filter(input2);
        assert_eq!(result2.unwrap(), "Actual content");
    }

    #[test]
    fn test_filter_acknowledgments() {
        let filter = NoiseFilter::new();

        assert_eq!(filter.filter("I understand"), None);
        assert_eq!(filter.filter("Understood."), None);
        assert_eq!(filter.filter("Will do!"), None);
        assert_eq!(filter.filter("Done"), None);
    }

    #[test]
    fn test_compressor_batch() {
        let compressor = MessageCompressor::new();

        let messages = vec![
            ("user".to_string(), "please help me".to_string()),
            ("assistant".to_string(), "Here's how it works: step 1, step 2, step 3".to_string()),
            ("user".to_string(), "thanks".to_string()),
            ("assistant".to_string(), "Let me know if you need anything else!".to_string()),
        ];

        let compressed = compressor.compress_batch(&messages);

        // Should keep substantive message and partially filter the polite suffix
        // "Here's how it works" won't be filtered (good content)
        // "Let me know if..." will be filtered by suffix pattern
        assert!(compressed.len() >= 1);
        assert!(compressed.iter().any(|(role, content)|
            role == "assistant" && content.contains("step 1")
        ));
    }

    #[test]
    fn test_token_estimation() {
        let compressor = MessageCompressor::new();

        let messages = vec![
            ("user".to_string(), "Hello world this is a test message".to_string()),
        ];

        let tokens = compressor.estimate_tokens(&messages);
        // ~35 chars / 4 + 5 for role = ~14 tokens
        assert!(tokens >= 10 && tokens <= 20);
    }

    #[test]
    fn test_compression_ratio() {
        let compressor = MessageCompressor::new();

        let ratio = compressor.compression_ratio(100, 50);
        assert_eq!(ratio, 50.0); // 50% reduction

        let ratio2 = compressor.compression_ratio(100, 25);
        assert_eq!(ratio2, 75.0); // 75% reduction
    }

    #[test]
    fn test_preserves_code_and_technical_content() {
        let filter = NoiseFilter::new();

        let code = "fn main() {\n    println!(\"Hello\");\n}";
        assert_eq!(filter.filter(code).unwrap(), code);

        let technical = "The FTS5 virtual table uses a trigram index for fast full-text search.";
        assert_eq!(filter.filter(technical).unwrap(), technical);
    }
}
