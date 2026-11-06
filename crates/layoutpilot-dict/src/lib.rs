//! Frequency dictionaries and a scored [`Detector`] for LayoutPilot.
//!
//! The crate embeds two FST dictionaries built from FrequencyWords (top 50k
//! tokens each) and a compact character-bigram model per language. The score
//! combines:
//!
//! * dictionary log-probability (with a small unknown-word floor);
//! * average character-bigram log-probability (smoothing included);
//! * optional user frequency bonus.
//!
//! The confidence is `sigmoid((score(candidate) - score(typed)) / temperature)`
//! with an extra penalty when the typed word is itself a known dictionary word
//! (TZ §9.2: do not correct words valid in both languages without a strong
//! advantage). This mapping is provisional until calibration on a corpus
//! (stage 3); the [`ScoringConfig`] exposes the weights.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;

use fst::automaton::Str;
use fst::{Automaton, IntoStreamer, Map, Streamer};
use layoutpilot_core::{DetectionContext, Detector};

#[allow(missing_docs)]
mod data {
    pub const RU_FST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ru.fst"));
    pub const EN_FST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/en.fst"));
    include!(concat!(env!("OUT_DIR"), "/ngrams.rs"));
}

/// Share of the target corpus that words starting with the candidate prefix
/// must cover for the early trigger. Calibrated with
/// `tools/calibrate_early.py`: 4 letters and `1e-5` give ~0.06% false
/// triggers on out-of-dictionary words at ~96% recall.
const EARLY_MIN_SHARE: f64 = 1e-5;

const START: char = '\u{2}';
const END: char = '\u{3}';

/// Language of a dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    /// Russian.
    Ru,
    /// English.
    En,
}

impl Language {
    /// Stable identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ru => "ru",
            Self::En => "en",
        }
    }
}

/// Detect the language of a word by its script.
///
/// Returns `None` for mixed-script or non-Cyrillic/non-ASCII words.
#[must_use]
pub fn language_of(word: &str) -> Option<Language> {
    let mut cyrillic = false;
    let mut latin = false;
    for ch in word.chars() {
        if !ch.is_alphabetic() {
            continue;
        }
        if is_cyrillic(ch) {
            cyrillic = true;
        } else if ch.is_ascii_alphabetic() {
            latin = true;
        } else {
            return None;
        }
    }
    match (cyrillic, latin) {
        (true, false) => Some(Language::Ru),
        (false, true) => Some(Language::En),
        _ => None,
    }
}

fn is_cyrillic(ch: char) -> bool {
    ('\u{0400}'..='\u{04FF}').contains(&ch)
}

/// Scoring weights. The defaults are a conservative starting point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoringConfig {
    /// Weight of dictionary log-probability.
    pub dict_weight: f64,
    /// Weight of average character-bigram log-probability.
    pub ngram_weight: f64,
    /// Logistic temperature; larger values make confidence less saturated.
    pub temperature: f64,
    /// Log-odds penalty when the typed word is itself known.
    pub known_word_bias: f64,
    /// Weight of the user-frequency bonus.
    pub user_weight: f64,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            dict_weight: 1.0,
            ngram_weight: 0.5,
            temperature: 1.0,
            known_word_bias: 2.0,
            user_weight: 0.5,
        }
    }
}

#[derive(Debug)]
struct LanguageModel {
    dict: Map<&'static [u8]>,
    bigrams: &'static [(char, char, u64)],
    total_freq: u64,
    bigram_total: u64,
}

impl LanguageModel {
    fn new(
        fst_bytes: &'static [u8],
        bigrams: &'static [(char, char, u64)],
        total_freq: u64,
        bigram_total: u64,
    ) -> Self {
        Self {
            dict: Map::new(fst_bytes).expect("embedded FST dictionary is valid"),
            bigrams,
            total_freq,
            bigram_total,
        }
    }

    fn count(&self, word: &str) -> Option<u64> {
        self.count_lower(&word.to_lowercase())
    }

    fn count_lower(&self, word: &str) -> Option<u64> {
        self.dict.get(word.as_bytes()).filter(|count| *count > 0)
    }

    /// Whether any dictionary word starts with `prefix` (lowercase).
    fn has_prefix(&self, prefix: &str) -> bool {
        let matcher = Str::new(prefix).starts_with();
        self.dict.search(matcher).into_stream().next().is_some()
    }

    /// Total frequency of words starting with `prefix` (lowercase), counting
    /// only until `enough` is reached.
    fn prefix_frequency(&self, prefix: &str, enough: u64) -> u64 {
        let matcher = Str::new(prefix).starts_with();
        let mut stream = self.dict.search(matcher).into_stream();
        let mut total = 0_u64;
        while let Some((_, count)) = stream.next() {
            total = total.saturating_add(count);
            if total >= enough {
                break;
            }
        }
        total
    }

    fn unknown_score(&self) -> f64 {
        (0.5 / self.total_freq as f64).ln()
    }

    fn score(&self, word: &str, config: &ScoringConfig) -> f64 {
        let word = word.to_lowercase();
        let dict_score = self.count(&word).map_or_else(
            || self.unknown_score(),
            |count| (count as f64 / self.total_freq as f64).ln(),
        );

        let chars: Vec<char> = word.chars().collect();
        let denominator = self.bigram_total as f64 + self.bigrams.len() as f64;
        let mut sum = 0.0;
        let mut previous = START;
        for &ch in &chars {
            sum += self.bigram_log_probability(previous, ch, denominator);
            previous = ch;
        }
        sum += self.bigram_log_probability(previous, END, denominator);
        let ngram_average = sum / (chars.len() + 1) as f64;

        config.dict_weight * dict_score + config.ngram_weight * ngram_average
    }

    fn bigram_log_probability(&self, first: char, second: char, denominator: f64) -> f64 {
        ((self.bigram_count(first, second) as f64 + 1.0) / denominator).ln()
    }

    fn bigram_count(&self, first: char, second: char) -> u64 {
        self.bigrams
            .binary_search_by_key(&(first, second), |&(a, b, _)| (a, b))
            .map_or(0, |index| self.bigrams[index].2)
    }
}

/// A [`Detector`] backed by embedded dictionaries, bigrams and user frequency.
#[derive(Debug)]
pub struct DictionaryDetector {
    ru: LanguageModel,
    en: LanguageModel,
    config: ScoringConfig,
    user: HashMap<String, u32>,
}

impl DictionaryDetector {
    /// Create a detector from the embedded dictionaries.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(ScoringConfig::default())
    }

    /// Create a detector with custom scoring weights.
    #[must_use]
    pub fn with_config(config: ScoringConfig) -> Self {
        Self {
            ru: LanguageModel::new(
                data::RU_FST,
                data::RU_BIGRAMS,
                data::RU_TOTAL_FREQ,
                data::RU_BIGRAM_TOTAL,
            ),
            en: LanguageModel::new(
                data::EN_FST,
                data::EN_BIGRAMS,
                data::EN_TOTAL_FREQ,
                data::EN_BIGRAM_TOTAL,
            ),
            config,
            user: HashMap::new(),
        }
    }

    /// Current scoring configuration.
    #[must_use]
    pub const fn config(&self) -> &ScoringConfig {
        &self.config
    }

    /// Mutable scoring configuration.
    pub const fn config_mut(&mut self) -> &mut ScoringConfig {
        &mut self.config
    }

    /// Dictionary frequency (0 if unknown).
    #[must_use]
    pub fn dict_count(&self, language: Language, word: &str) -> u64 {
        self.model(language).count(word).unwrap_or(0)
    }

    /// Combined score for `word` in `language`.
    #[must_use]
    pub fn score(&self, language: Language, word: &str) -> f64 {
        let mut score = self.model(language).score(word, &self.config);
        if let Some(count) = self.user.get(&word.to_lowercase()) {
            score += self.config.user_weight * (1.0 + f64::from(*count)).ln();
        }
        score
    }

    /// Record a user-confirmed word (FR-18).
    pub fn learn(&mut self, word: impl AsRef<str>) {
        let word = word.as_ref().to_lowercase();
        let count = self.user.entry(word).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// Remove one user confirmation; the entry is dropped at zero.
    pub fn unlearn(&mut self, word: &str) {
        let word = word.to_lowercase();
        if let Some(count) = self.user.get_mut(&word) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.user.remove(&word);
            }
        }
    }

    /// Number of user confirmations for a word.
    #[must_use]
    pub fn user_count(&self, word: &str) -> u32 {
        self.user.get(&word.to_lowercase()).copied().unwrap_or(0)
    }

    /// Confidence that `candidate` is the intended word for `typed`.
    #[must_use]
    pub fn confidence_for(&self, typed: &str, candidate: &str) -> f32 {
        if typed == candidate {
            return 0.5;
        }

        let Some(candidate_language) = language_of(candidate) else {
            return 0.0;
        };
        if self.dict_count(candidate_language, candidate) == 0 {
            return 0.0;
        }
        let Some(typed_language) = language_of(typed) else {
            return 0.0;
        };

        let typed_known = self.dict_count(typed_language, typed) > 0;
        let typed_score = self.score(typed_language, typed);
        let candidate_score = self.score(candidate_language, candidate);
        let mut difference = candidate_score - typed_score;
        if typed_known {
            difference -= self.config.known_word_bias;
        }

        let temperature = if self.config.temperature > 0.1 {
            self.config.temperature
        } else {
            0.1
        };
        let confidence = sigmoid(difference / temperature);
        if confidence.is_finite() {
            confidence as f32
        } else {
            0.0
        }
    }

    /// Early-trigger confidence for an unfinished word: no known word of the
    /// typed language starts with `typed`, while words starting with
    /// `candidate` are common in the other language.
    #[must_use]
    pub fn early_confidence_for(&self, typed: &str, candidate: &str) -> f32 {
        let Some(target) = language_of(candidate) else {
            return 0.0;
        };
        let source = match target {
            Language::Ru => Language::En,
            Language::En => Language::Ru,
        };
        let typed = typed.to_lowercase();
        let candidate = candidate.to_lowercase();
        if self.model(source).has_prefix(&typed)
            || self.user.keys().any(|word| word.starts_with(&typed))
        {
            return 0.0;
        }
        let model = self.model(target);
        let enough = (model.total_freq as f64 * EARLY_MIN_SHARE).ceil() as u64;
        let user_known = self.user.keys().any(|word| word.starts_with(&candidate));
        if user_known || model.prefix_frequency(&candidate, enough.max(1)) >= enough.max(1) {
            0.999
        } else {
            0.0
        }
    }

    fn model(&self, language: Language) -> &LanguageModel {
        match language {
            Language::Ru => &self.ru,
            Language::En => &self.en,
        }
    }
}

impl Default for DictionaryDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector for DictionaryDetector {
    fn confidence(&self, context: &DetectionContext<'_>) -> f32 {
        self.confidence_for(context.typed, context.candidate)
    }

    fn early_confidence(&self, typed: &str, candidate: &str) -> f32 {
        self.early_confidence_for(typed, candidate)
    }
}

fn sigmoid(value: f64) -> f64 {
    if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let exp = value.exp();
        exp / (1.0 + exp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_words_are_in_the_dictionaries() {
        let detector = DictionaryDetector::new();
        assert!(detector.dict_count(Language::Ru, "привет") > 0);
        assert!(detector.dict_count(Language::Ru, "команда") > 0);
        assert!(detector.dict_count(Language::En, "hello") > 0);
        assert!(detector.dict_count(Language::En, "world") > 0);
        assert_eq!(detector.dict_count(Language::En, "ghbdtn"), 0);
        assert_eq!(detector.dict_count(Language::Ru, "руддщ"), 0);
    }

    #[test]
    fn detects_language_by_script() {
        assert_eq!(language_of("привет"), Some(Language::Ru));
        assert_eq!(language_of("hello"), Some(Language::En));
        assert_eq!(language_of("приветhello"), None);
        assert_eq!(language_of("123"), None);
        assert_eq!(language_of("пончик"), Some(Language::Ru));
    }

    #[test]
    fn wrong_layout_is_confident() {
        let detector = DictionaryDetector::new();
        let confidence = detector.confidence_for("ghbdtn", "привет");
        assert!(confidence > 0.97, "confidence was {confidence}");
    }

    #[test]
    fn valid_english_is_not_converted_to_nonsense() {
        let detector = DictionaryDetector::new();
        assert_eq!(detector.confidence_for("hello", "руддщ"), 0.0);
        assert_eq!(detector.confidence_for("привет", "ghbdtn"), 0.0);
    }

    #[test]
    fn short_wrong_layout_works() {
        let detector = DictionaryDetector::new();
        let confidence = detector.confidence_for("ytn", "нет");
        assert!(confidence > 0.97, "confidence was {confidence}");
    }

    #[test]
    fn ngram_prefers_real_word_over_noise() {
        let detector = DictionaryDetector::new();
        let real = detector.score(Language::Ru, "привет");
        let noise = detector.score(Language::Ru, "прривет");
        assert!(real > noise, "real={real}, noise={noise}");
    }

    #[test]
    fn user_learning_increases_score() {
        let mut detector = DictionaryDetector::new();
        let before = detector.score(Language::Ru, "привет");
        detector.learn("привет");
        let after = detector.score(Language::Ru, "привет");
        assert!(after > before);
        assert_eq!(detector.user_count("привет"), 1);
        detector.unlearn("привет");
        assert_eq!(detector.user_count("привет"), 0);
    }
}
