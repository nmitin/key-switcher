//! In-memory character buffer for the correction pipeline.
//!
//! The real daemon stores key events with timestamps and device ids; this core
//! buffer stores characters only, which is enough for segmentation, conversion
//! and tests. It never touches the disk and must be cleared on every context
//! boundary from FR-40.

use std::collections::VecDeque;

use crate::segmenter::last_word;

/// Default buffer capacity in characters (TZ §9.1).
pub const DEFAULT_CAPACITY: usize = 256;

/// A bounded ring buffer of recently typed characters.
#[derive(Debug, Clone)]
pub struct CharBuffer {
    capacity: usize,
    chars: VecDeque<char>,
}

impl CharBuffer {
    /// Create a buffer with the given capacity (zero is treated as one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            chars: VecDeque::new(),
        }
    }

    /// Create a buffer with [`DEFAULT_CAPACITY`].
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }

    /// Maximum number of characters kept.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of characters currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Append a character, returning the evicted character if the buffer was full.
    pub fn push(&mut self, ch: char) -> Option<char> {
        let evicted = if self.chars.len() == self.capacity {
            self.chars.pop_front()
        } else {
            None
        };
        self.chars.push_back(ch);
        evicted
    }

    /// Remove and return the last character.
    pub fn pop(&mut self) -> Option<char> {
        self.chars.pop_back()
    }

    /// Append a string character by character.
    pub fn push_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.push(ch);
        }
    }

    /// Drop all characters (FR-40 context reset).
    pub fn clear(&mut self) {
        self.chars.clear();
    }

    /// The buffered text.
    #[must_use]
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// The last word in the buffer.
    #[must_use]
    pub fn last_word(&self) -> Option<String> {
        let text = self.text();
        last_word(&text).map(|word| word.text.to_owned())
    }

    /// Replace the trailing `suffix` with `replacement`.
    ///
    /// Returns `false` if the buffer does not end with `suffix`. The engine
    /// uses this to keep its context in sync after a correction.
    pub fn replace_suffix(&mut self, suffix: &str, replacement: &str) -> bool {
        let text = self.text();
        if !text.ends_with(suffix) {
            return false;
        }
        let keep = &text[..text.len() - suffix.len()];
        self.clear();
        self.push_str(keep);
        self.push_str(replacement);
        true
    }
}

impl Default for CharBuffer {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_capacity_characters() {
        let mut buffer = CharBuffer::new(3);
        assert_eq!(buffer.push('a'), None);
        assert_eq!(buffer.push('b'), None);
        assert_eq!(buffer.push('c'), None);
        assert_eq!(buffer.push('d'), Some('a'));
        assert_eq!(buffer.text(), "bcd");
        assert_eq!(buffer.len(), 3);
    }

    #[test]
    fn zero_capacity_is_treated_as_one() {
        let mut buffer = CharBuffer::new(0);
        assert_eq!(buffer.capacity(), 1);
        buffer.push_str("ab");
        assert_eq!(buffer.text(), "b");
    }

    #[test]
    fn pops_the_last_character() {
        let mut buffer = CharBuffer::default();
        buffer.push_str("ab");
        assert_eq!(buffer.pop(), Some('b'));
        assert_eq!(buffer.text(), "a");
        assert_eq!(buffer.pop(), Some('a'));
        assert_eq!(buffer.pop(), None);
    }

    #[test]
    fn clear_drops_everything() {
        let mut buffer = CharBuffer::with_default_capacity();
        buffer.push_str("ghbdtn ");
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.text(), "");
        assert_eq!(buffer.last_word(), None);
    }

    #[test]
    fn returns_last_word_after_separator() {
        let mut buffer = CharBuffer::default();
        buffer.push_str("привет, мир! ");
        assert_eq!(buffer.last_word().as_deref(), Some("мир"));
    }

    #[test]
    fn replaces_a_trailing_suffix() {
        let mut buffer = CharBuffer::default();
        buffer.push_str("ghbdtn ");
        assert!(buffer.replace_suffix("ghbdtn ", "привет "));
        assert_eq!(buffer.text(), "привет ");
        assert!(!buffer.replace_suffix("missing", "x"));
    }

    #[test]
    fn truncated_word_is_still_available() {
        let mut buffer = CharBuffer::new(4);
        buffer.push_str("привет");
        assert_eq!(buffer.text(), "ивет");
        assert_eq!(buffer.last_word().as_deref(), Some("ивет"));
    }
}
