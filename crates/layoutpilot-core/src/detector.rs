//! Candidate confidence sources.
//!
//! The engine does not know how to score words. It asks a [`Detector`] for the
//! calibrated probability that the converted candidate is what the user meant.
//! The production implementation will be dictionary + n-gram + ONNX; this
//! module provides a small lexicon detector for development and tests.

use std::collections::HashSet;

/// Inputs for a detector decision.
#[derive(Debug, Clone, Copy)]
pub struct DetectionContext<'a> {
    /// Word exactly as typed.
    pub typed: &'a str,
    /// Word reinterpreted in the other layout.
    pub candidate: &'a str,
    /// Active application id, when known.
    pub app_id: Option<&'a str>,
    /// Previous word, when available.
    pub previous_word: Option<&'a str>,
}

/// What the user taught by reacting to a correction.
///
/// Words are lowercase cores without trailing punctuation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Learning {
    /// The user undid a correction: never convert this typed word, and do
    /// not start an early correction on a prefix of it.
    Keep(String),
    /// The user converted this word by hand: it is a real word.
    Accept(String),
    /// The user rolled back their own manual fix: forget the `Accept`.
    Retract(String),
}

/// Returns `P(candidate is intended)` in `[0, 1]`.
pub trait Detector: Send + Sync {
    /// Calibrated confidence that the candidate should replace the typed word.
    fn confidence(&self, context: &DetectionContext<'_>) -> f32;

    /// Confidence that an unfinished word was typed in the wrong layout.
    ///
    /// `typed` is the prefix as typed, `candidate` the same keys in the other
    /// layout. Used by the early trigger, which corrects before the word is
    /// complete. The default never fires.
    fn early_confidence(&self, _typed: &str, _candidate: &str) -> f32 {
        0.0
    }

    /// Apply a learning event. The default ignores it.
    fn learn(&mut self, _event: &Learning) {}
}

/// A detector with a fixed confidence, useful in tests and as a default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaticDetector {
    /// Value returned for every request.
    pub confidence: f32,
}

impl StaticDetector {
    /// Create a fixed detector.
    #[must_use]
    pub const fn new(confidence: f32) -> Self {
        Self { confidence }
    }
}

impl Detector for StaticDetector {
    fn confidence(&self, _context: &DetectionContext<'_>) -> f32 {
        self.confidence
    }
}

/// A tiny lexicon detector.
///
/// This is deliberately not the production dictionary: it is a deterministic,
/// dependency-free way to exercise the engine and to bootstrap tests. The real
/// detector will implement the same trait with FST/DAWG data and n-grams.
#[derive(Debug, Clone, Default)]
pub struct LexiconDetector {
    words: HashSet<String>,
}

impl LexiconDetector {
    /// Build a detector from known words (case-insensitive).
    #[must_use]
    pub fn from_words<I, S>(words: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut detector = Self::default();
        for word in words {
            detector.insert(word);
        }
        detector
    }

    /// Add a known word.
    pub fn insert(&mut self, word: impl AsRef<str>) {
        self.words.insert(word.as_ref().to_lowercase());
    }

    /// Whether a word is in the lexicon (case-insensitive).
    #[must_use]
    pub fn contains(&self, word: &str) -> bool {
        self.words.contains(&word.to_lowercase())
    }

    /// Number of known words.
    #[must_use]
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether the lexicon is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

impl Detector for LexiconDetector {
    fn confidence(&self, context: &DetectionContext<'_>) -> f32 {
        let typed_known = self.contains(context.typed);
        let candidate_known = self.contains(context.candidate);
        match (typed_known, candidate_known) {
            (false, true) => 0.999,
            (true, false) => 0.001,
            // Both known or both unknown: do not guess.
            _ => 0.5,
        }
    }

    /// Fires when no known word starts with `typed` but one starts with
    /// `candidate`.
    fn early_confidence(&self, typed: &str, candidate: &str) -> f32 {
        let starts = |prefix: &str| self.words.iter().any(|word| word.starts_with(prefix));
        if !starts(typed) && starts(candidate) {
            0.999
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(typed: &'a str, candidate: &'a str) -> DetectionContext<'a> {
        DetectionContext {
            typed,
            candidate,
            app_id: None,
            previous_word: None,
        }
    }

    #[test]
    fn known_candidate_beats_unknown_typed() {
        let detector = LexiconDetector::from_words(["привет"]);
        assert!((detector.confidence(&context("ghbdtn", "привет")) - 0.999).abs() < f32::EPSILON);
    }

    #[test]
    fn known_typed_beats_unknown_candidate() {
        let detector = LexiconDetector::from_words(["ghbdtn"]);
        assert!((detector.confidence(&context("ghbdtn", "привет")) - 0.001).abs() < f32::EPSILON);
    }

    #[test]
    fn ambiguity_stays_in_the_grey_zone() {
        let detector = LexiconDetector::from_words(["ghbdtn", "привет"]);
        assert!((detector.confidence(&context("ghbdtn", "привет")) - 0.5).abs() < f32::EPSILON);
        let empty = LexiconDetector::default();
        assert!((empty.confidence(&context("ghbdtn", "привет")) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn case_insensitive_lookup() {
        let detector = LexiconDetector::from_words(["Привет"]);
        assert!(detector.contains("привет"));
        assert!(detector.contains("ПРИВЕТ"));
        assert!(!detector.contains("пока"));
    }
}
