// Loop detection for identifying runaway conversation patterns
// Detects repeated message patterns that indicate automation failures

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

/// Warning levels for detected loops
#[derive(Debug, Clone, PartialEq)]
pub enum LoopSeverity {
    /// Suspicious pattern detected but not conclusive
    Warning,
    /// Clear loop pattern detected
    Critical,
}

/// Information about a detected loop
#[derive(Debug, Clone)]
pub struct LoopDetection {
    pub severity: LoopSeverity,
    pub message: String,
    pub repetition_count: usize,
    pub pattern_size: usize,
}

/// Detector for conversation loops and automation failures
pub struct LoopDetector {
    /// Maximum messages before warning (absolute threshold)
    max_messages_warning: usize,
    /// Maximum messages before critical alert
    max_messages_critical: usize,
    /// Minimum repetitions to consider a loop
    min_repetitions: usize,
    /// Maximum pattern size to check (in messages)
    max_pattern_size: usize,
}

impl LoopDetector {
    pub fn new() -> Self {
        Self {
            max_messages_warning: 100,
            max_messages_critical: 200,
            min_repetitions: 10,
            max_pattern_size: 10,
        }
    }

    /// Analyze a message batch for loop patterns
    pub fn analyze(&self, messages: &[(String, String)]) -> Vec<LoopDetection> {
        let mut detections = Vec::new();

        // Check 1: Absolute message count
        let message_count = messages.len();
        if message_count >= self.max_messages_critical {
            detections.push(LoopDetection {
                severity: LoopSeverity::Critical,
                message: format!(
                    "Extremely high message count: {} messages (threshold: {})",
                    message_count, self.max_messages_critical
                ),
                repetition_count: 0,
                pattern_size: 0,
            });
        } else if message_count >= self.max_messages_warning {
            detections.push(LoopDetection {
                severity: LoopSeverity::Warning,
                message: format!(
                    "High message count: {} messages (threshold: {})",
                    message_count, self.max_messages_warning
                ),
                repetition_count: 0,
                pattern_size: 0,
            });
        }

        // Check 2: Content hash-based repetition detection
        if let Some(detection) = self.detect_content_repetition(messages) {
            detections.push(detection);
        }

        // Check 3: Pattern-based loop detection (sequences of messages)
        if let Some(detection) = self.detect_message_pattern_loops(messages) {
            detections.push(detection);
        }

        detections
    }

    /// Detect if the same content appears repeatedly
    fn detect_content_repetition(&self, messages: &[(String, String)]) -> Option<LoopDetection> {
        let mut content_counts: HashMap<u64, usize> = HashMap::new();

        for (_, content) in messages {
            let hash = self.hash_content(content);
            *content_counts.entry(hash).or_insert(0) += 1;
        }

        // Find the most repeated content
        if let Some((_, &max_count)) = content_counts.iter().max_by_key(|(_, &count)| count) {
            if max_count >= self.min_repetitions * 2 {
                return Some(LoopDetection {
                    severity: LoopSeverity::Critical,
                    message: format!(
                        "Identical content repeated {} times (threshold: {})",
                        max_count, self.min_repetitions * 2
                    ),
                    repetition_count: max_count,
                    pattern_size: 1,
                });
            } else if max_count >= self.min_repetitions {
                return Some(LoopDetection {
                    severity: LoopSeverity::Warning,
                    message: format!(
                        "Content repeated {} times (threshold: {})",
                        max_count, self.min_repetitions
                    ),
                    repetition_count: max_count,
                    pattern_size: 1,
                });
            }
        }

        None
    }

    /// Detect repeating patterns of message sequences
    fn detect_message_pattern_loops(&self, messages: &[(String, String)]) -> Option<LoopDetection> {
        // Try different pattern sizes (2-message, 3-message, 4-message patterns, etc.)
        for pattern_size in 2..=self.max_pattern_size.min(messages.len() / 4) {
            if let Some(detection) = self.find_repeating_pattern(messages, pattern_size) {
                return Some(detection);
            }
        }

        None
    }

    /// Find if a pattern of N messages repeats
    fn find_repeating_pattern(&self, messages: &[(String, String)], pattern_size: usize) -> Option<LoopDetection> {
        if messages.len() < pattern_size * self.min_repetitions {
            return None;
        }

        let mut pattern_counts: HashMap<Vec<u64>, usize> = HashMap::new();

        // Create sliding windows of pattern_size
        for window in messages.windows(pattern_size) {
            let pattern_hash: Vec<u64> = window
                .iter()
                .map(|(role, content)| {
                    let mut hasher = DefaultHasher::new();
                    role.hash(&mut hasher);
                    content.hash(&mut hasher);
                    hasher.finish()
                })
                .collect();

            *pattern_counts.entry(pattern_hash).or_insert(0) += 1;
        }

        // Find the most repeated pattern
        if let Some((_, &max_count)) = pattern_counts.iter().max_by_key(|(_, &count)| count) {
            if max_count >= self.min_repetitions * 2 {
                return Some(LoopDetection {
                    severity: LoopSeverity::Critical,
                    message: format!(
                        "Message pattern of {} messages repeated {} times (threshold: {})",
                        pattern_size, max_count, self.min_repetitions * 2
                    ),
                    repetition_count: max_count,
                    pattern_size,
                });
            } else if max_count >= self.min_repetitions {
                return Some(LoopDetection {
                    severity: LoopSeverity::Warning,
                    message: format!(
                        "Message pattern of {} messages repeated {} times (threshold: {})",
                        pattern_size, max_count, self.min_repetitions
                    ),
                    repetition_count: max_count,
                    pattern_size,
                });
            }
        }

        None
    }

    /// Hash content for comparison (normalize whitespace)
    fn hash_content(&self, content: &str) -> u64 {
        let normalized = content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let mut hasher = DefaultHasher::new();
        normalized.hash(&mut hasher);
        hasher.finish()
    }
}

impl Default for LoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_high_message_count() {
        let detector = LoopDetector::new();

        // Create 150 messages (over warning threshold)
        let messages: Vec<(String, String)> = (0..150)
            .map(|i| ("user".to_string(), format!("Message {}", i)))
            .collect();

        let detections = detector.analyze(&messages);
        assert!(!detections.is_empty());
        assert!(detections.iter().any(|d| d.severity == LoopSeverity::Warning));
    }

    #[test]
    fn test_content_repetition() {
        let detector = LoopDetector::new();

        // Create 20 identical messages
        let messages: Vec<(String, String)> = (0..20)
            .map(|_| ("user".to_string(), "Please read documentation".to_string()))
            .collect();

        let detections = detector.analyze(&messages);
        assert!(!detections.is_empty());
        let repetition_detection = detections.iter().find(|d| d.pattern_size == 1);
        assert!(repetition_detection.is_some());
        assert_eq!(repetition_detection.unwrap().repetition_count, 20);
    }

    #[test]
    fn test_pattern_loop() {
        let detector = LoopDetector::new();

        // Create a repeating 2-message pattern
        let mut messages = Vec::new();
        for _ in 0..15 {
            messages.push(("user".to_string(), "Question A".to_string()));
            messages.push(("assistant".to_string(), "Answer A".to_string()));
        }

        let detections = detector.analyze(&messages);
        assert!(!detections.is_empty());
        let pattern_detection = detections.iter().find(|d| d.pattern_size > 1);
        assert!(pattern_detection.is_some());
    }

    #[test]
    fn test_no_false_positives() {
        let detector = LoopDetector::new();

        // Normal conversation with varied content
        let messages = vec![
            ("user".to_string(), "How do I fix this error?".to_string()),
            ("assistant".to_string(), "Let me help you debug that.".to_string()),
            ("user".to_string(), "Thanks, that worked!".to_string()),
            ("assistant".to_string(), "Great! Anything else?".to_string()),
        ];

        let detections = detector.analyze(&messages);
        // Should not detect any loops in normal conversation
        assert!(detections.is_empty());
    }

    #[test]
    fn test_four_message_loop() {
        let detector = LoopDetector::new();

        // The actual pattern from the runaway session (4 messages)
        let mut messages = Vec::new();
        for _ in 0..50 {
            messages.push(("user".to_string(), "Please read documentation in ~/Assistants/shared".to_string()));
            messages.push(("assistant".to_string(), "Key points after reading the shared docs: Universal knowledge base...".to_string()));
            messages.push(("user".to_string(), "Please read through all documentation relevant to continuum".to_string()));
            messages.push(("assistant".to_string(), "Continuum documentation highlights (all read): Current state...".to_string()));
        }

        let detections = detector.analyze(&messages);
        assert!(!detections.is_empty());

        // Should detect both high message count AND pattern repetition
        assert!(detections.iter().any(|d| d.severity == LoopSeverity::Critical));
        assert!(detections.iter().any(|d| d.pattern_size == 4 || d.pattern_size == 2));
    }
}
