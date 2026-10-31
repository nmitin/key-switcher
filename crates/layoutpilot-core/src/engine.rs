//! Correction engine: the state machine that decides when to rewrite a word.
//!
//! The engine is pure: it receives already-normalized characters and a context
//! snapshot, and returns an [`Action`] for the injector. It performs no I/O, so
//! every rule is testable on any host.
//!
//! Trigger policy (FR-01, ADR-001): space and closing punctuation. Enter is
//! never a trigger. Tab is a trigger only in the `aggressive` profile when
//! explicitly enabled. The apostrophe is treated as a word character, not a
//! trigger, so `don't` stays one word.

use layoutpilot_layouts::LayoutId;

use crate::buffer::CharBuffer;
use crate::conversion::convert_word;
use crate::detector::{DetectionContext, Detector};
use crate::profile::{EngineConfig, Profile};
use crate::protection::is_protected_context;
use crate::segmenter::{is_word_char, last_word};

/// Keyboard modifiers relevant to correction policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    /// Control is held.
    pub ctrl: bool,
    /// Alt is held.
    pub alt: bool,
    /// Shift is held; does not block correction.
    pub shift: bool,
    /// Super/Meta is held.
    pub super_key: bool,
}

impl Modifiers {
    /// Whether the combination must not be processed (FR-08).
    #[must_use]
    pub const fn blocks_auto(self) -> bool {
        self.ctrl || self.alt || self.super_key
    }
}

/// A snapshot of the environment for one input event.
#[derive(Debug, Clone, Copy)]
pub struct InputContext<'a> {
    /// Resolved application profile.
    pub profile: Profile,
    /// Active layout of the physical keyboard.
    pub active_layout: LayoutId,
    /// Whether the layout state provider is trustworthy (FR-39).
    pub layout_trusted: bool,
    /// Whether an IME is composing (FR-08).
    pub ime_active: bool,
    /// Currently held modifiers.
    pub modifiers: Modifiers,
    /// Active application id, when known.
    pub app_id: Option<&'a str>,
}

impl<'a> InputContext<'a> {
    /// A trusted context with default modifiers.
    #[must_use]
    pub const fn new(profile: Profile, active_layout: LayoutId) -> Self {
        Self {
            profile,
            active_layout,
            layout_trusted: true,
            ime_active: false,
            modifiers: Modifiers {
                ctrl: false,
                alt: false,
                shift: false,
                super_key: false,
            },
            app_id: None,
        }
    }

    /// Set layout trust.
    #[must_use]
    pub const fn with_layout_trusted(mut self, trusted: bool) -> Self {
        self.layout_trusted = trusted;
        self
    }

    /// Set IME composition state.
    #[must_use]
    pub const fn with_ime_active(mut self, active: bool) -> Self {
        self.ime_active = active;
        self
    }

    /// Set modifiers.
    #[must_use]
    pub const fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    /// Set the active application id.
    #[must_use]
    pub const fn with_app_id(mut self, app_id: &'a str) -> Self {
        self.app_id = Some(app_id);
        self
    }
}

/// A word replacement instruction for the injector.
///
/// The injector decides whether the separator has already reached the
/// application; [`Self::delete_chars`] and [`Self::insert_text`] accept that
/// choice explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// Word as typed.
    pub original: String,
    /// Word after conversion.
    pub converted: String,
    /// Separator that triggered the correction, if any.
    pub separator: Option<char>,
    /// Number of characters in the original word.
    pub word_chars: usize,
}

impl Correction {
    fn new(original: String, converted: String, separator: Option<char>) -> Self {
        let word_chars = original.chars().count();
        Self {
            original,
            converted,
            separator,
            word_chars,
        }
    }

    /// Characters to delete before inserting the replacement.
    #[must_use]
    pub fn delete_chars(&self, separator_already_typed: bool) -> usize {
        self.word_chars + usize::from(separator_already_typed && self.separator.is_some())
    }

    /// Text to insert for the forward correction.
    #[must_use]
    pub fn insert_text(&self, include_separator: bool) -> String {
        let mut text = self.converted.clone();
        if include_separator {
            if let Some(separator) = self.separator {
                text.push(separator);
            }
        }
        text
    }

    /// The same correction in the opposite direction.
    #[must_use]
    pub fn reversed(&self) -> Self {
        let mut reversed = self.clone();
        std::mem::swap(&mut reversed.original, &mut reversed.converted);
        reversed
    }
}

/// An action for the injector / correction manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Replace the typed word with the converted one.
    Replace(Correction),
    /// Undo the last correction (converted → original).
    Undo(Correction),
    /// Re-apply the last correction after an undo.
    Redo(Correction),
}

impl Action {
    /// The correction this action refers to.
    #[must_use]
    pub fn correction(&self) -> &Correction {
        match self {
            Self::Replace(correction) | Self::Undo(correction) | Self::Redo(correction) => {
                correction
            }
        }
    }
}

/// Coarse engine state for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    /// No buffered input.
    Idle,
    /// A word is being typed.
    Typing,
    /// A correction is applied and can still be undone.
    Corrected,
}

/// Why the context was reset (FR-40).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// Focus or window changed.
    FocusChange,
    /// Pointer click or scroll.
    PointerActivity,
    /// Screen locked.
    ScreenLock,
    /// Layout changed.
    LayoutChange,
    /// Input device changed.
    DeviceChange,
    /// Idle timeout.
    IdleTimeout,
    /// Daemon paused.
    Pause,
    /// Application profile is `off`.
    Disabled,
    /// A modifier combination was pressed.
    Shortcut,
    /// Caller requested a reset.
    Manual,
}

#[derive(Debug, Clone)]
struct UndoRecord {
    correction: Correction,
    applied: bool,
}

/// The correction state machine.
#[derive(Debug, Clone)]
pub struct Engine<D: Detector> {
    config: EngineConfig,
    detector: D,
    buffer: CharBuffer,
    current_word: String,
    word_tainted: bool,
    next_word_attached: bool,
    paused: bool,
    undo: Option<UndoRecord>,
    last_reset: Option<ResetReason>,
}

impl<D: Detector> Engine<D> {
    /// Create an engine.
    #[must_use]
    pub fn new(config: EngineConfig, detector: D) -> Self {
        Self {
            buffer: CharBuffer::new(config.buffer_capacity),
            config,
            detector,
            current_word: String::new(),
            word_tainted: false,
            next_word_attached: false,
            paused: false,
            undo: None,
            last_reset: None,
        }
    }

    /// Engine configuration.
    #[must_use]
    pub const fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Detector reference.
    #[must_use]
    pub const fn detector(&self) -> &D {
        &self.detector
    }

    /// Mutable detector reference (for hot-swapping a dictionary).
    pub const fn detector_mut(&mut self) -> &mut D {
        &mut self.detector
    }

    /// Current buffered text.
    #[must_use]
    pub fn buffer_text(&self) -> String {
        self.buffer.text()
    }

    /// Word currently being typed.
    #[must_use]
    pub fn current_word(&self) -> &str {
        &self.current_word
    }

    /// Coarse state for diagnostics.
    #[must_use]
    pub fn state(&self) -> EngineState {
        if !self.current_word.is_empty() {
            EngineState::Typing
        } else if self.undo.is_some() {
            EngineState::Corrected
        } else {
            EngineState::Idle
        }
    }

    /// Whether an undo/redo is currently possible.
    #[must_use]
    pub fn undo_available(&self) -> bool {
        self.undo.is_some()
    }

    /// Last reason the engine dropped its context.
    #[must_use]
    pub const fn last_reset(&self) -> Option<ResetReason> {
        self.last_reset
    }

    /// Whether the engine is paused.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.paused
    }

    /// Process one character.
    ///
    /// Returns an action only when the character is a trigger and all policy
    /// checks pass. The separator is added to the buffer in either case.
    pub fn on_char(&mut self, ch: char, context: &InputContext<'_>) -> Option<Action> {
        if self.paused || context.profile == Profile::Off {
            self.reset(if self.paused {
                ResetReason::Pause
            } else {
                ResetReason::Disabled
            });
            return None;
        }

        if context.modifiers.blocks_auto() {
            self.reset(ResetReason::Shortcut);
            return None;
        }

        // Any new input makes a previous correction non-undoable (FR-04).
        self.undo = None;

        if is_word_char(ch) {
            if self.current_word.is_empty() {
                self.word_tainted = self.next_word_attached;
                self.next_word_attached = false;
            }
            self.current_word.push(ch);
            self.buffer.push(ch);
            return None;
        }

        if self.is_trigger(ch, context.profile) {
            let action = self.evaluate(context, ch);
            self.finish_token(ch);
            return action;
        }

        self.finish_token(ch);
        self.next_word_attached = ch.is_ascii_digit();
        None
    }

    /// Process a Backspace: drop the last character and invalidate undo.
    pub fn on_backspace(&mut self) {
        self.undo = None;
        self.current_word.pop();
        self.buffer.pop();
        self.next_word_attached = false;
        if self.current_word.is_empty() {
            self.word_tainted = false;
        }
    }

    /// Manually convert the word currently being typed (FR-03).
    ///
    /// This bypasses the minimum length and confidence checks, but still needs
    /// a trustworthy layout and a reversible word. It is available in the
    /// `manual` profile and while an IME is active.
    pub fn fix_last_word(&mut self, context: &InputContext<'_>) -> Option<Correction> {
        if self.paused
            || context.profile == Profile::Off
            || !context.layout_trusted
            || self.current_word.is_empty()
            || self.word_tainted
        {
            return None;
        }

        let target = context.active_layout.other();
        let converted = convert_word(
            &self.current_word,
            context.active_layout.table(),
            target.table(),
        )
        .ok()?;
        if converted == self.current_word {
            return None;
        }

        self.buffer.replace_suffix(&self.current_word, &converted);
        let correction = Correction::new(self.current_word.clone(), converted, None);
        self.current_word.clear();
        self.word_tainted = false;
        self.next_word_attached = false;
        self.undo = self.config.undo_enabled.then(|| UndoRecord {
            correction: correction.clone(),
            applied: true,
        });
        Some(correction)
    }

    /// Undo the last correction or redo it after an undo.
    ///
    /// Returns `None` if there is nothing to undo or if input/focus reset the
    /// record (FR-04).
    pub fn undo(&mut self) -> Option<Action> {
        if self.paused {
            return None;
        }
        let correction = self.undo.as_ref()?.correction.clone();
        let applied = self.undo.as_ref()?.applied;
        let (from, to) = if applied {
            (&correction.converted, &correction.original)
        } else {
            (&correction.original, &correction.converted)
        };
        let mut from = from.clone();
        let mut to = to.clone();
        if let Some(separator) = correction.separator {
            from.push(separator);
            to.push(separator);
        }
        self.buffer.replace_suffix(&from, &to);

        if let Some(record) = self.undo.as_mut() {
            record.applied = !applied;
        }
        if applied {
            Some(Action::Undo(correction))
        } else {
            Some(Action::Redo(correction))
        }
    }

    /// Drop all context and undo state (FR-40).
    pub fn reset(&mut self, reason: ResetReason) {
        self.last_reset = Some(reason);
        self.buffer.clear();
        self.current_word.clear();
        self.word_tainted = false;
        self.next_word_attached = false;
        self.undo = None;
    }

    /// Pause processing; clears the buffer and undo.
    pub fn pause(&mut self) {
        self.paused = true;
        self.reset(ResetReason::Pause);
    }

    /// Resume processing.
    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Set paused state.
    pub fn set_paused(&mut self, paused: bool) {
        if paused {
            self.pause();
        } else {
            self.resume();
        }
    }

    fn is_trigger(&self, ch: char, profile: Profile) -> bool {
        (ch == ' ' && self.config.space_trigger)
            || (self.config.punct_trigger
                && matches!(
                    ch,
                    '.' | ',' | '!' | '?' | ';' | ':' | ')' | ']' | '}' | '"'
                ))
            || (ch == '\t'
                && profile == Profile::Aggressive
                && self.config.tab_trigger_in_aggressive)
    }

    fn finish_token(&mut self, ch: char) {
        self.buffer.push(ch);
        self.current_word.clear();
        self.word_tainted = false;
        self.next_word_attached = false;
    }

    fn evaluate(&mut self, context: &InputContext<'_>, separator: char) -> Option<Action> {
        if self.current_word.is_empty() || self.word_tainted {
            return None;
        }
        if !context.profile.allows_auto() || !context.layout_trusted || context.ime_active {
            return None;
        }

        let letters = self
            .current_word
            .chars()
            .filter(|ch| ch.is_alphabetic())
            .count();
        if letters < self.config.min_word_len_for(context.profile) {
            return None;
        }

        let target = context.active_layout.other();
        let converted = convert_word(
            &self.current_word,
            context.active_layout.table(),
            target.table(),
        )
        .ok()?;
        if converted == self.current_word {
            return None;
        }

        let text = self.buffer.text();
        if is_protected_context(&text) {
            return None;
        }

        let start = text.rfind(self.current_word.as_str()).unwrap_or(0);
        let previous_word = last_word(&text[..start]).map(|word| word.text);
        let confidence = self
            .detector
            .confidence(&DetectionContext {
                typed: &self.current_word,
                candidate: &converted,
                app_id: context.app_id,
                previous_word,
            })
            .clamp(0.0, 1.0);
        if confidence < self.config.threshold_for(context.profile) {
            return None;
        }

        self.buffer.replace_suffix(&self.current_word, &converted);
        let correction = Correction::new(self.current_word.clone(), converted, Some(separator));
        self.undo = self.config.undo_enabled.then(|| UndoRecord {
            correction: correction.clone(),
            applied: true,
        });
        Some(Action::Replace(correction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::LexiconDetector;

    fn detector() -> LexiconDetector {
        LexiconDetector::from_words(["привет", "прив", "пр", "мир", "vbh", "hello", "руддщ"])
    }

    fn auto() -> InputContext<'static> {
        InputContext::new(Profile::Auto, LayoutId::Us)
    }

    fn type_text<D: Detector>(
        engine: &mut Engine<D>,
        text: &str,
        context: &InputContext<'_>,
    ) -> Option<Action> {
        let mut action = None;
        for ch in text.chars() {
            if let Some(next) = engine.on_char(ch, context) {
                action = Some(next);
            }
        }
        action
    }

    fn replace(action: Option<Action>) -> Correction {
        match action {
            Some(Action::Replace(correction)) => correction,
            other => panic!("expected Replace, got {other:?}"),
        }
    }

    #[test]
    fn corrects_on_space() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let correction = replace(type_text(&mut engine, "ghbdtn ", &auto()));
        assert_eq!(correction.original, "ghbdtn");
        assert_eq!(correction.converted, "привет");
        assert_eq!(correction.separator, Some(' '));
        assert_eq!(correction.word_chars, 6);
        assert_eq!(correction.delete_chars(true), 7);
        assert_eq!(correction.insert_text(true), "привет ");
        assert_eq!(engine.state(), EngineState::Corrected);
    }

    #[test]
    fn corrects_on_punctuation_and_preserves_it() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let correction = replace(type_text(&mut engine, "ghbdtn,", &auto()));
        assert_eq!(correction.separator, Some(','));
        assert_eq!(correction.insert_text(true), "привет,");
    }

    #[test]
    fn respects_min_word_len() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "gh ", &auto()), None);

        let aggressive = InputContext::new(Profile::Aggressive, LayoutId::Us);
        assert!(matches!(
            type_text(&mut engine, "gh ", &aggressive),
            Some(Action::Replace(_))
        ));
    }

    #[test]
    fn conservative_needs_a_longer_word() {
        let conservative = InputContext::new(Profile::Conservative, LayoutId::Us);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghb ", &conservative), None);
    }

    #[test]
    fn confidence_threshold_is_profile_specific() {
        let mut engine = Engine::new(
            EngineConfig::default(),
            crate::detector::StaticDetector::new(0.96),
        );
        assert_eq!(type_text(&mut engine, "ghbdtn ", &auto()), None);

        let aggressive = InputContext::new(Profile::Aggressive, LayoutId::Us);
        assert!(matches!(
            type_text(&mut engine, "ghbdtn ", &aggressive),
            Some(Action::Replace(_))
        ));
    }

    #[test]
    fn manual_profile_does_not_auto_correct_but_fix_works() {
        let context = InputContext::new(Profile::Manual, LayoutId::Us);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghbdtn ", &context), None);

        type_text(&mut engine, "ghbdtn", &context);
        let correction = engine.fix_last_word(&context).unwrap();
        assert_eq!(correction.converted, "привет");
        assert_eq!(correction.separator, None);
        assert!(engine.undo_available());
    }

    #[test]
    fn untrusted_layout_disables_auto_and_manual() {
        let context = auto().with_layout_trusted(false);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghbdtn ", &context), None);
        type_text(&mut engine, "ghbdtn", &context);
        assert_eq!(engine.fix_last_word(&context), None);
    }

    #[test]
    fn ime_disables_auto_but_allows_manual() {
        let context = auto().with_ime_active(true);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghbdtn ", &context), None);
        type_text(&mut engine, "ghbdtn", &context);
        assert!(engine.fix_last_word(&context).is_some());
    }

    #[test]
    fn modifier_shortcut_resets_context() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn", &auto());
        let context = auto().with_modifiers(Modifiers {
            ctrl: true,
            ..Modifiers::default()
        });
        assert_eq!(engine.on_char('c', &context), None);
        assert!(engine.buffer_text().is_empty());
        assert_eq!(engine.current_word(), "");
    }

    #[test]
    fn digits_prevent_correction_of_adjacent_words() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghbdtn1 ", &auto()), None);

        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "1ghbdtn ", &auto()), None);

        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert!(matches!(
            type_text(&mut engine, "1 ghbdtn ", &auto()),
            Some(Action::Replace(_))
        ));
    }

    #[test]
    fn urls_and_paths_are_not_corrected() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(
            type_text(&mut engine, "https://example.com ", &auto()),
            None
        );

        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "/usr/ghbdtn ", &auto()), None);
    }

    #[test]
    fn triggers_can_be_disabled_by_config() {
        let config = EngineConfig {
            space_trigger: false,
            ..EngineConfig::default()
        };
        let mut engine = Engine::new(config, detector());
        assert_eq!(type_text(&mut engine, "ghbdtn ", &auto()), None);

        let config = EngineConfig {
            punct_trigger: false,
            ..EngineConfig::default()
        };
        let mut engine = Engine::new(config, detector());
        assert_eq!(type_text(&mut engine, "ghbdtn,", &auto()), None);
    }

    #[test]
    fn enter_and_tab_are_not_triggers() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghbdtn\n", &auto()), None);

        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghbdtn\t", &auto()), None);
    }

    #[test]
    fn tab_is_a_trigger_only_in_aggressive_with_opt_in() {
        let config = EngineConfig {
            tab_trigger_in_aggressive: true,
            ..EngineConfig::default()
        };
        let aggressive = InputContext::new(Profile::Aggressive, LayoutId::Us);
        let mut engine = Engine::new(config, detector());
        assert!(matches!(
            type_text(&mut engine, "ghbdtn\t", &aggressive),
            Some(Action::Replace(_))
        ));
    }

    #[test]
    fn undo_toggles_between_undo_and_redo() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn ", &auto());
        assert!(engine.undo_available());

        assert!(matches!(engine.undo(), Some(Action::Undo(_))));
        assert!(engine.undo_available());
        assert!(matches!(engine.undo(), Some(Action::Redo(_))));
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn", &auto());
        engine.on_backspace();
        assert_eq!(engine.current_word(), "ghbdt");
        assert_eq!(engine.buffer_text(), "ghbdt");
        engine.on_backspace();
        assert_eq!(engine.current_word(), "ghbd");
    }

    #[test]
    fn backspace_invalidates_undo() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn ", &auto());
        assert!(engine.undo_available());
        engine.on_backspace();
        assert!(!engine.undo_available());
    }

    #[test]
    fn new_input_invalidates_undo() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn ", &auto());
        engine.on_char('a', &auto());
        assert!(!engine.undo_available());
        assert_eq!(engine.undo(), None);
    }

    #[test]
    fn reset_clears_buffer_and_undo() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn ", &auto());
        engine.reset(ResetReason::FocusChange);
        assert_eq!(engine.buffer_text(), "");
        assert!(!engine.undo_available());
        assert_eq!(engine.last_reset(), Some(ResetReason::FocusChange));
    }

    #[test]
    fn off_profile_drops_everything() {
        let context = InputContext::new(Profile::Off, LayoutId::Us);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghbdtn ", &context), None);
        assert_eq!(engine.buffer_text(), "");
        assert_eq!(engine.last_reset(), Some(ResetReason::Disabled));
    }

    #[test]
    fn pause_and_resume() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn", &auto());
        engine.pause();
        assert!(engine.is_paused());
        assert_eq!(engine.buffer_text(), "");
        assert_eq!(engine.on_char(' ', &auto()), None);
        engine.resume();
        assert!(matches!(
            type_text(&mut engine, "ghbdtn ", &auto()),
            Some(Action::Replace(_))
        ));
    }

    #[test]
    fn correction_reversal_swaps_words() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let correction = replace(type_text(&mut engine, "ghbdtn ", &auto()));
        let reversed = correction.reversed();
        assert_eq!(reversed.original, "привет");
        assert_eq!(reversed.converted, "ghbdtn");
        assert_eq!(reversed.insert_text(true), "ghbdtn ");
    }

    #[test]
    fn case_is_preserved_in_correction() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let correction = replace(type_text(&mut engine, "Ghbdtn ", &auto()));
        assert_eq!(correction.converted, "Привет");
    }
}
