//! Application profiles and profile resolution (FR-13).
//!
//! Profiles control whether automatic correction is allowed and which
//! thresholds apply. The resolver implements the order from the TZ:
//! exact `app_id` → glob by descending specificity → `*` → default.

use std::cmp::Reverse;

/// Behaviour profile for an application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Standard thresholds.
    #[default]
    Auto,
    /// Lower threshold, shorter words, optional Tab trigger.
    Aggressive,
    /// Only obvious cases, longer minimum word.
    Conservative,
    /// No automatic correction; manual hotkey still works.
    Manual,
    /// No processing at all; the buffer must be cleared.
    Off,
}

impl Profile {
    /// Whether the automatic path may run for this profile.
    #[must_use]
    pub const fn allows_auto(self) -> bool {
        matches!(self, Self::Auto | Self::Aggressive | Self::Conservative)
    }
}

/// Engine thresholds that profiles can override.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    /// Minimum alphabetic word length for `auto` (default 3).
    pub min_word_len: usize,
    /// Calibrated probability threshold for `auto` (default 0.97).
    pub confidence_threshold: f32,
    /// Minimum alphabetic word length for `aggressive` (default 2).
    pub aggressive_min_word_len: usize,
    /// Threshold for `aggressive` (default 0.90).
    pub aggressive_threshold: f32,
    /// Minimum alphabetic word length for `conservative` (default 4).
    pub conservative_min_word_len: usize,
    /// Threshold for `conservative` (default 0.995).
    pub conservative_threshold: f32,
    /// Whether Tab triggers correction in the `aggressive` profile.
    pub tab_trigger_in_aggressive: bool,
    /// Whether space is a correction trigger.
    pub space_trigger: bool,
    /// Whether closing punctuation is a correction trigger.
    pub punct_trigger: bool,
    /// Whether Enter is a correction trigger.
    pub enter_trigger: bool,
    /// Character buffer capacity (default 256).
    pub buffer_capacity: usize,
    /// Whether corrections can be undone.
    pub undo_enabled: bool,
    /// How many preceding words an automatic correction may take along
    /// (`f jy yt ghbitk` → `а он не пришел`); 0 disables the lookback.
    pub lookback_words: usize,
    /// Confidence a preceding word needs to be converted together with a
    /// confidently corrected word. Lower than the main threshold: the
    /// corrected word already proves the layout was wrong.
    pub lookback_threshold: f32,
    /// Correct a word before it is finished, as soon as its prefix is
    /// impossible in the current language and common in the other one.
    pub early_trigger: bool,
    /// Minimum letters before the early trigger may fire (default 4).
    pub early_min_len: usize,
    /// Learn from undo and manual fixes ([`crate::Learning`]).
    pub learning: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            min_word_len: 3,
            confidence_threshold: 0.97,
            aggressive_min_word_len: 2,
            aggressive_threshold: 0.90,
            conservative_min_word_len: 4,
            conservative_threshold: 0.995,
            tab_trigger_in_aggressive: false,
            space_trigger: true,
            punct_trigger: true,
            enter_trigger: false,
            buffer_capacity: 256,
            undo_enabled: true,
            lookback_words: 4,
            lookback_threshold: 0.7,
            early_trigger: false,
            early_min_len: 4,
            learning: true,
        }
    }
}

impl EngineConfig {
    /// Minimum word length for a profile.
    #[must_use]
    pub const fn min_word_len_for(self, profile: Profile) -> usize {
        match profile {
            Profile::Auto | Profile::Manual | Profile::Off => self.min_word_len,
            Profile::Aggressive => self.aggressive_min_word_len,
            Profile::Conservative => self.conservative_min_word_len,
        }
    }

    /// Confidence threshold for a profile, clamped to `[0, 1]`.
    #[must_use]
    pub fn threshold_for(self, profile: Profile) -> f32 {
        let threshold = match profile {
            Profile::Auto | Profile::Manual | Profile::Off => self.confidence_threshold,
            Profile::Aggressive => self.aggressive_threshold,
            Profile::Conservative => self.conservative_threshold,
        };
        threshold.clamp(0.0, 1.0)
    }
}

/// One application rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRule {
    /// Exact `app_id`, glob with `*`/`?`, or `*`.
    pub pattern: String,
    /// Profile to apply.
    pub profile: Profile,
}

impl AppRule {
    /// Create a rule.
    #[must_use]
    pub fn new(pattern: impl Into<String>, profile: Profile) -> Self {
        Self {
            pattern: pattern.into(),
            profile,
        }
    }

    fn is_glob(&self) -> bool {
        self.pattern.contains(['*', '?'])
    }

    fn specificity(&self) -> (usize, usize) {
        let literal = self
            .pattern
            .chars()
            .filter(|ch| !matches!(ch, '*' | '?'))
            .count();
        (literal, self.pattern.chars().count())
    }
}

/// Resolves an `app_id` to a [`Profile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileResolver {
    rules: Vec<AppRule>,
    default: Profile,
}

impl ProfileResolver {
    /// Create a resolver with a fallback profile.
    #[must_use]
    pub fn new(default: Profile) -> Self {
        Self {
            rules: Vec::new(),
            default,
        }
    }

    /// Add a rule. Later rules win only for equal specificity; exact matches
    /// always win over globs.
    #[must_use]
    pub fn with_rule(mut self, rule: AppRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Add a rule in place.
    pub fn push_rule(&mut self, rule: AppRule) {
        self.rules.push(rule);
    }

    /// Resolve the profile for `app_id`.
    #[must_use]
    pub fn resolve(&self, app_id: &str) -> Profile {
        if let Some(rule) = self
            .rules
            .iter()
            .find(|rule| !rule.is_glob() && rule.pattern.eq_ignore_ascii_case(app_id))
        {
            return rule.profile;
        }

        let mut globs: Vec<&AppRule> = self.rules.iter().filter(|rule| rule.is_glob()).collect();
        globs.sort_by_key(|rule| Reverse(rule.specificity()));
        for rule in globs {
            if glob_match_ignore_ascii_case(&rule.pattern, app_id) {
                return rule.profile;
            }
        }

        self.default
    }

    /// Fallback profile.
    #[must_use]
    pub const fn default_profile(&self) -> Profile {
        self.default
    }
}

impl Default for ProfileResolver {
    fn default() -> Self {
        Self::new(Profile::Auto)
    }
}

/// Match `text` against a glob with `*` (any sequence) and `?` (one character).
#[must_use]
pub fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_impl(pattern, text, false)
}

/// Case-insensitive ASCII variant used for `app_id` matching.
#[must_use]
pub fn glob_match_ignore_ascii_case(pattern: &str, text: &str) -> bool {
    glob_match_impl(pattern, text, true)
}

fn glob_match_impl(pattern: &str, text: &str, ignore_case: bool) -> bool {
    let fold = |ch: char| {
        if ignore_case {
            ch.to_ascii_lowercase()
        } else {
            ch
        }
    };
    let pattern: Vec<char> = pattern.chars().map(fold).collect();
    let text: Vec<char> = text.chars().map(fold).collect();
    let mut pattern_index = 0;
    let mut text_index = 0;
    let mut star: Option<usize> = None;
    let mut star_text_index = 0;

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_text_index = text_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_text_index += 1;
            text_index = star_text_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_thresholds_and_lengths() {
        let config = EngineConfig::default();
        assert_eq!(config.min_word_len_for(Profile::Auto), 3);
        assert_eq!(config.min_word_len_for(Profile::Aggressive), 2);
        assert_eq!(config.min_word_len_for(Profile::Conservative), 4);
        assert!((config.threshold_for(Profile::Auto) - 0.97).abs() < f32::EPSILON);
        assert!((config.threshold_for(Profile::Aggressive) - 0.90).abs() < f32::EPSILON);
    }

    #[test]
    fn exact_app_id_wins_over_glob() {
        let resolver = ProfileResolver::new(Profile::Auto)
            .with_rule(AppRule::new("*terminal*", Profile::Manual))
            .with_rule(AppRule::new("org.telegram.desktop", Profile::Aggressive));
        assert_eq!(
            resolver.resolve("org.telegram.desktop"),
            Profile::Aggressive
        );
        assert_eq!(
            resolver.resolve("org.telegram.desktop.extra"),
            Profile::Auto
        );
    }

    #[test]
    fn most_specific_glob_wins() {
        let resolver = ProfileResolver::new(Profile::Auto)
            .with_rule(AppRule::new("*", Profile::Auto))
            .with_rule(AppRule::new("*keepass*", Profile::Off))
            .with_rule(AppRule::new("org.keepassxc.KeePassXC", Profile::Off));
        assert_eq!(resolver.resolve("org.keepassxc.KeePassXC"), Profile::Off);
        assert_eq!(resolver.resolve("keepassx"), Profile::Off);
        assert_eq!(resolver.resolve("firefox"), Profile::Auto);
    }

    #[test]
    fn default_is_used_when_nothing_matches() {
        let resolver = ProfileResolver::new(Profile::Conservative)
            .with_rule(AppRule::new("*terminal*", Profile::Manual));
        assert_eq!(resolver.resolve("firefox"), Profile::Conservative);
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*terminal*", "org.gnome.terminal"));
        assert!(glob_match_ignore_ascii_case(
            "*terminal*",
            "org.gnome.Terminal"
        ));
        assert!(glob_match("org.*.desktop", "org.telegram.desktop"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("org.*", "org"));
        assert!(glob_match("**", ""));
    }
}
