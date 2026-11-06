//! Correction engine: the state machine that decides when to rewrite a word.
//!
//! The engine is pure: it receives already-normalized characters and a context
//! snapshot, and returns an [`Action`] for the injector. It performs no I/O, so
//! every rule is testable on any host.
//!
//! Trigger policy (FR-01, ADR-001): space and closing punctuation. Enter is
//! an opt-in trigger (`enter_trigger`, off by default). Tab is a trigger only in the `aggressive` profile when
//! explicitly enabled. The apostrophe is treated as a word character, not a
//! trigger, so `don't` stays one word.

use layoutpilot_layouts::{LayoutId, LayoutTable};

use crate::buffer::CharBuffer;
use crate::conversion::{convert_phrase, reinterpret_word, split_trailing_suffix};
use crate::detector::{DetectionContext, Detector, Learning};
use crate::profile::{EngineConfig, Profile};
use crate::protection::{is_protected_context, is_protected_token};
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
    /// Number of characters in the original word or phrase.
    pub word_chars: usize,
    /// Characters after the replacement that must stay untouched.
    ///
    /// Manual gestures may fire after a space. The trailing space is kept in
    /// place instead of being deleted and retyped.
    pub trailing: String,
}

impl Correction {
    fn new(original: String, converted: String, separator: Option<char>) -> Self {
        let word_chars = original.chars().count();
        Self {
            original,
            converted,
            separator,
            word_chars,
            trailing: String::new(),
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
    /// Made by a gesture/hotkey rather than automatically.
    manual: bool,
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
    /// The early trigger already fired for the current word.
    early_fired: bool,
    /// Learning events not yet taken by the caller for persistence.
    learned: Vec<Learning>,
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
            early_fired: false,
            learned: Vec::new(),
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

    /// Whether the last correction is currently applied (undo would revert it).
    #[must_use]
    pub fn undo_applied(&self) -> bool {
        self.undo.as_ref().is_some_and(|record| record.applied)
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
                self.early_fired = false;
            }
            self.current_word.push(ch);
            self.buffer.push(ch);
            return self.evaluate_early(context);
        }

        // Punctuation is kept inside the word when it can be a letter in the
        // other layout, when punctuation triggers are disabled, or when Shift
        // is held (injecting during Shift would uppercase the replacement).
        // Trailing sentence punctuation is split off later and preserved.
        if is_punctuation(ch)
            && (punctuation_is_letter(ch, context.active_layout)
                || (!self.current_word.is_empty()
                    && (!self.config.punct_trigger || context.modifiers.shift)))
        {
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

    /// Manually convert the last word (FR-03).
    ///
    /// This bypasses the minimum length and confidence checks, but still needs
    /// a trustworthy layout and a reversible word. It is available in the
    /// `manual` profile and while an IME is active. If no word is being typed,
    /// the last completed word in the current line is used, so the gesture
    /// works after a space as well.
    pub fn fix_last_word(&mut self, context: &InputContext<'_>) -> Option<Correction> {
        if self.paused
            || context.profile == Profile::Off
            || !context.layout_trusted
            || self.word_tainted
        {
            return None;
        }

        let target = context.active_layout.other();
        let (original, trailing) = if self.current_word.is_empty() {
            self.last_typed_token()?
        } else {
            (self.current_word.clone(), String::new())
        };
        let (core, suffix) = split_trailing_suffix(&original);
        if core.is_empty() {
            return None;
        }
        let converted_core =
            reinterpret_word(core, context.active_layout.table(), target.table()).ok()?;
        let mut converted = converted_core;
        converted.push_str(suffix);
        if converted == original || !target.table().can_type(&converted) {
            return None;
        }

        self.apply_manual(original, converted, trailing)
    }

    /// Manually convert the current phrase (words separated by spaces).
    ///
    /// The phrase is the text since the last sentence boundary or newline, so
    /// a triple Shift can flip a whole mistyped sentence while leaving earlier
    /// text alone. Trailing spaces are preserved and the `manual`/`off` profile
    /// rules are the same as for [`Self::fix_last_word`].
    pub fn fix_last_phrase(&mut self, context: &InputContext<'_>) -> Option<Correction> {
        if self.paused || context.profile == Profile::Off || !context.layout_trusted {
            return None;
        }

        let target = context.active_layout.other();
        let text = self.buffer.text();
        let (start, end) = phrase_bounds(&text)?;
        let original = text[start..end].to_owned();
        let trailing = text[end..].to_owned();
        let converted =
            convert_phrase(&original, context.active_layout.table(), target.table()).ok()?;
        if converted == original || !can_type_manual(&converted, target.table()) {
            return None;
        }

        self.apply_manual(original, converted, trailing)
    }

    fn last_typed_token(&self) -> Option<(String, String)> {
        let text = self.buffer.text();
        let line_start = text.rfind(['\n', '\t']).map_or(0, |index| index + 1);
        let line = &text[line_start..];
        let end = line.trim_end_matches(' ').len();
        if end == 0 {
            return None;
        }
        let region = &line[..end];
        let start = region.rfind(' ').map_or(0, |index| index + 1);
        if start >= region.len() {
            return None;
        }
        Some((region[start..].to_owned(), line[end..].to_owned()))
    }

    fn apply_manual(
        &mut self,
        original: String,
        converted: String,
        trailing: String,
    ) -> Option<Correction> {
        let mut suffix = original.clone();
        suffix.push_str(&trailing);
        let mut replacement = converted.clone();
        replacement.push_str(&trailing);
        if !self.buffer.replace_suffix(&suffix, &replacement) {
            return None;
        }

        let mut correction = Correction::new(original, converted, None);
        correction.trailing = trailing;
        self.current_word.clear();
        self.word_tainted = false;
        self.next_word_attached = false;
        self.undo = self.config.undo_enabled.then(|| UndoRecord {
            correction: correction.clone(),
            applied: true,
            manual: true,
        });
        for word in word_cores(&correction.converted) {
            self.record(Learning::Accept(word));
        }
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
        let manual = self.undo.as_ref()?.manual;
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
        from.push_str(&correction.trailing);
        to.push_str(&correction.trailing);
        self.buffer.replace_suffix(&from, &to);

        if let Some(record) = self.undo.as_mut() {
            record.applied = !applied;
        }
        if applied {
            if manual {
                for word in word_cores(&correction.converted) {
                    self.record(Learning::Retract(word));
                }
            } else if let Some(word) = word_cores(&correction.original).pop() {
                // The word that triggered the correction is the one to keep.
                self.record(Learning::Keep(word));
            }
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
            || (ch == '\n' && self.config.enter_trigger)
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

        let target = context.active_layout.other();
        let (core, suffix) = split_trailing_suffix(&self.current_word);
        if core.is_empty() {
            return None;
        }
        let converted_core =
            reinterpret_word(core, context.active_layout.table(), target.table()).ok()?;
        let letters = converted_core
            .chars()
            .filter(|ch| ch.is_alphabetic())
            .count();
        if letters < self.config.min_word_len_for(context.profile) {
            return None;
        }
        let mut converted = converted_core.clone();
        converted.push_str(suffix);
        if converted == self.current_word || !target.table().can_type(&converted) {
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
                typed: core,
                candidate: &converted_core,
                app_id: context.app_id,
                previous_word,
            })
            .clamp(0.0, 1.0);
        if confidence < self.config.threshold_for(context.profile) {
            return None;
        }

        let head = text
            .strip_suffix(self.current_word.as_str())
            .unwrap_or(&text[..start]);
        let (mut original, mut replacement) = self.lookback(head, context);
        original.push_str(&self.current_word);
        replacement.push_str(&converted);

        self.buffer.replace_suffix(&original, &replacement);
        let correction = Correction::new(original, replacement, Some(separator));
        self.undo = self.config.undo_enabled.then(|| UndoRecord {
            correction: correction.clone(),
            applied: true,
            manual: false,
        });
        if self.early_fired {
            // The early guess was wrong and the finished word goes back:
            // remember it so its prefix does not fire again.
            if let Some(word) = word_cores(&correction.converted).pop() {
                self.record(Learning::Keep(word));
            }
        }
        Some(Action::Replace(correction))
    }
}

impl<D: Detector> Engine<D> {
    /// Collect the words right before a confidently corrected one that were
    /// typed in the same wrong layout but were too short (or too uncertain)
    /// to be corrected on their own.
    ///
    /// `head` is the buffer before the corrected word. Returns the original
    /// and converted text to prepend, spaces included. The walk stops at the
    /// first word that does not qualify, at a line break or at sentence
    /// punctuation. Words that were corrected earlier are already in the
    /// target script and cannot be reinterpreted, so they stop it too.
    /// Learning events since the last call, for persistence. They are
    /// already applied to the detector.
    pub fn take_learning(&mut self) -> Vec<Learning> {
        std::mem::take(&mut self.learned)
    }

    fn record(&mut self, event: Learning) {
        if !self.config.learning {
            return;
        }
        self.detector.learn(&event);
        self.learned.push(event);
    }

    /// Correct the unfinished word if its prefix cannot start a word of the
    /// current language but commonly starts one in the other. The rest of the
    /// word is then typed directly in the right layout. If the guess was
    /// wrong, the regular check at the separator converts the finished word
    /// back (`iner` → `штук`, … `штукешф ` → `inertia `).
    fn evaluate_early(&mut self, context: &InputContext<'_>) -> Option<Action> {
        if !self.config.early_trigger
            || self.early_fired
            || self.word_tainted
            || !matches!(context.profile, Profile::Auto | Profile::Aggressive)
            || !context.layout_trusted
            || context.ime_active
        {
            return None;
        }
        let target = context.active_layout.other();
        let candidate = reinterpret_word(
            &self.current_word,
            context.active_layout.table(),
            target.table(),
        )
        .ok()?;
        let letters = candidate.chars().filter(|ch| ch.is_alphabetic()).count();
        if letters < self.config.early_min_len.max(1)
            || candidate == self.current_word
            || !candidate.chars().all(is_word_char)
            || !target.table().can_type(&candidate)
        {
            return None;
        }
        let text = self.buffer.text();
        if is_protected_context(&text) {
            return None;
        }
        let confidence = self
            .detector
            .early_confidence(&self.current_word, &candidate)
            .clamp(0.0, 1.0);
        if confidence < self.config.threshold_for(context.profile) {
            return None;
        }

        let head = text
            .strip_suffix(self.current_word.as_str())
            .unwrap_or_default();
        let (mut original, mut replacement) = self.lookback(head, context);
        original.push_str(&self.current_word);
        replacement.push_str(&candidate);
        if !self.buffer.replace_suffix(&original, &replacement) {
            return None;
        }
        self.early_fired = true;
        self.current_word = candidate;
        let correction = Correction::new(original, replacement, None);
        self.undo = self.config.undo_enabled.then(|| UndoRecord {
            correction: correction.clone(),
            applied: true,
            manual: false,
        });
        Some(Action::Replace(correction))
    }

    fn lookback(&self, head: &str, context: &InputContext<'_>) -> (String, String) {
        let mut original = String::new();
        let mut converted = String::new();
        let mut rest = head;
        for _ in 0..self.config.lookback_words {
            let trimmed = rest.trim_end_matches(' ');
            let spaces = &rest[trimmed.len()..];
            if spaces.is_empty() || trimmed.is_empty() {
                break;
            }
            let token_start = trimmed
                .rfind(char::is_whitespace)
                .map_or(0, |index| index + 1);
            let token = &trimmed[token_start..];
            let Some(candidate) = self.lookback_candidate(token, context) else {
                break;
            };
            original.insert_str(0, spaces);
            original.insert_str(0, token);
            converted.insert_str(0, spaces);
            converted.insert_str(0, &candidate);
            rest = &trimmed[..token_start];
        }
        (original, converted)
    }

    fn lookback_candidate(&self, token: &str, context: &InputContext<'_>) -> Option<String> {
        if token.is_empty() || is_protected_token(token) {
            return None;
        }
        if token.chars().any(|ch| ch.is_ascii_digit()) {
            return None;
        }
        // Sentence punctuation ends the phrase.
        let (_, suffix) = split_trailing_suffix(token);
        if !suffix.is_empty() {
            return None;
        }
        let target = context.active_layout.other();
        let candidate =
            reinterpret_word(token, context.active_layout.table(), target.table()).ok()?;
        if candidate == token
            || !candidate.chars().all(is_word_char)
            || !target.table().can_type(&candidate)
        {
            return None;
        }
        let confidence = self.detector.confidence(&DetectionContext {
            typed: token,
            candidate: &candidate,
            app_id: context.app_id,
            previous_word: None,
        });
        (confidence >= self.config.lookback_threshold).then_some(candidate)
    }
}

/// Lowercase word cores of `text` (trailing punctuation dropped).
fn word_cores(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|token| split_trailing_suffix(token).0.to_lowercase())
        .filter(|core| core.chars().any(char::is_alphabetic))
        .collect()
}

fn is_punctuation(ch: char) -> bool {
    !ch.is_alphanumeric() && !ch.is_whitespace() && !is_word_char(ch)
}

fn punctuation_is_letter(ch: char, layout: LayoutId) -> bool {
    layout
        .table()
        .keystroke_for(ch)
        .and_then(|keystroke| layout.other().table().char_at(keystroke))
        .is_some_and(char::is_alphabetic)
}

fn phrase_bounds(text: &str) -> Option<(usize, usize)> {
    let mut start = 0;
    let mut iter = text.char_indices().peekable();
    while let Some((index, ch)) = iter.next() {
        let boundary = if ch == '\n' || ch == '\t' {
            true
        } else if matches!(ch, '.' | '!' | '?' | ';' | ':') {
            let followed_by_space = iter.peek().is_some_and(|(_, next)| next.is_whitespace());
            let has_more_text = text[index + ch.len_utf8()..]
                .chars()
                .any(|next| next != ' ');
            followed_by_space && has_more_text
        } else {
            false
        };
        if boundary {
            start = index + ch.len_utf8();
        }
    }

    let mut end = text.len();
    while end > start && text.as_bytes()[end - 1] == b' ' {
        end -= 1;
    }
    while start < end && text.as_bytes()[start] == b' ' {
        start += 1;
    }
    (start < end).then_some((start, end))
}

fn can_type_manual(text: &str, table: &LayoutTable) -> bool {
    text.chars()
        .all(|ch| ch == ' ' || table.keystroke_for(ch).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::LexiconDetector;

    fn detector() -> LexiconDetector {
        LexiconDetector::from_words([
            "привет",
            "прив",
            "пр",
            "мир",
            "vbh",
            "hello",
            "жизнь",
            "салют",
            "смех",
        ])
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

    fn phrase_detector() -> LexiconDetector {
        LexiconDetector::from_words(["а", "он", "не", "пришел", "ok"])
    }

    #[test]
    fn lookback_converts_short_words_with_the_corrected_one() {
        let mut engine = Engine::new(EngineConfig::default(), phrase_detector());
        let correction = replace(type_text(&mut engine, "f jy yt ghbitk ", &auto()));
        assert_eq!(correction.original, "f jy yt ghbitk");
        assert_eq!(correction.converted, "а он не пришел");
        assert_eq!(correction.delete_chars(true), 15);
        assert_eq!(engine.buffer_text(), "а он не пришел ");

        // Undo restores the whole phrase.
        assert!(matches!(engine.undo(), Some(Action::Undo(_))));
        assert_eq!(engine.buffer_text(), "f jy yt ghbitk ");
    }

    #[test]
    fn lookback_is_limited_and_can_be_disabled() {
        let config = EngineConfig {
            lookback_words: 1,
            ..EngineConfig::default()
        };
        let mut engine = Engine::new(config, phrase_detector());
        let correction = replace(type_text(&mut engine, "f jy yt ghbitk ", &auto()));
        assert_eq!(correction.original, "yt ghbitk");

        let config = EngineConfig {
            lookback_words: 0,
            ..EngineConfig::default()
        };
        let mut engine = Engine::new(config, phrase_detector());
        let correction = replace(type_text(&mut engine, "f jy yt ghbitk ", &auto()));
        assert_eq!(correction.original, "ghbitk");
    }

    #[test]
    fn lookback_stops_at_boundaries() {
        for (text, expected) in [
            // A source-language word.
            ("f ok yt ghbitk ", "yt ghbitk"),
            // Sentence punctuation and line breaks.
            ("f jy. yt ghbitk ", "yt ghbitk"),
            ("f jy\nyt ghbitk ", "yt ghbitk"),
            // Digits next to a word.
            ("f 2jy yt ghbitk ", "yt ghbitk"),
        ] {
            let mut engine = Engine::new(EngineConfig::default(), phrase_detector());
            let correction = replace(type_text(&mut engine, text, &auto()));
            assert_eq!(correction.original, expected, "{text:?}");
        }
    }

    #[test]
    fn lookback_does_not_reconvert_corrected_words() {
        let mut engine = Engine::new(EngineConfig::default(), phrase_detector());
        type_text(&mut engine, "ghbitk ", &auto());
        assert_eq!(engine.buffer_text(), "пришел ");
        // Still in `us` (no daemon here): the next correction must not touch
        // the already converted word.
        let correction = replace(type_text(&mut engine, "yt ghbitk ", &auto()));
        assert_eq!(correction.original, "yt ghbitk");
        assert_eq!(engine.buffer_text(), "пришел не пришел ");
    }

    fn early_config() -> EngineConfig {
        EngineConfig {
            early_trigger: true,
            ..EngineConfig::default()
        }
    }

    #[test]
    fn early_trigger_fires_mid_word() {
        let mut engine = Engine::new(early_config(), detector());
        assert_eq!(type_text(&mut engine, "ghb", &auto()), None);
        let correction = replace(engine.on_char('d', &auto()));
        assert_eq!(correction.original, "ghbd");
        assert_eq!(correction.converted, "прив");
        assert_eq!(correction.separator, None);
        assert_eq!(correction.delete_chars(true), 4);

        // The daemon switched to `ru`; the rest arrives in Cyrillic and the
        // finished word is not corrected again.
        let ru = InputContext::new(Profile::Auto, LayoutId::Ru);
        assert_eq!(type_text(&mut engine, "ет ", &ru), None);
        assert_eq!(engine.buffer_text(), "привет ");
    }

    #[test]
    fn early_trigger_is_opt_in_and_profile_bound() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert_eq!(type_text(&mut engine, "ghbd", &auto()), None);

        for profile in [Profile::Manual, Profile::Conservative] {
            let mut engine = Engine::new(early_config(), detector());
            let context = InputContext::new(profile, LayoutId::Us);
            assert_eq!(
                type_text(&mut engine, "ghbd", &context),
                None,
                "{profile:?}"
            );
        }
    }

    #[test]
    fn early_trigger_skips_known_prefixes() {
        // `hel` starts `hello`: still possible in English.
        let mut engine = Engine::new(early_config(), detector());
        assert_eq!(type_text(&mut engine, "hell", &auto()), None);
    }

    fn keep(word: &str) -> Learning {
        Learning::Keep(word.to_owned())
    }

    #[test]
    fn undo_of_an_automatic_correction_keeps_the_typed_word() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn ", &auto());
        assert!(engine.take_learning().is_empty());
        assert!(matches!(engine.undo(), Some(Action::Undo(_))));
        assert_eq!(engine.take_learning(), vec![keep("ghbdtn")]);
        // Redo does not teach anything new; the journal is drained.
        assert!(matches!(engine.undo(), Some(Action::Redo(_))));
        assert!(engine.take_learning().is_empty());
    }

    #[test]
    fn undo_of_a_phrase_keeps_only_the_anchor() {
        let mut engine = Engine::new(EngineConfig::default(), phrase_detector());
        type_text(&mut engine, "f jy yt ghbitk ", &auto());
        engine.undo();
        assert_eq!(engine.take_learning(), vec![keep("ghbitk")]);
    }

    #[test]
    fn manual_fix_accepts_and_its_undo_retracts() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let manual = InputContext::new(Profile::Manual, LayoutId::Us);
        type_text(&mut engine, "vbh", &manual);
        engine.fix_last_word(&manual).expect("fix");
        assert_eq!(
            engine.take_learning(),
            vec![Learning::Accept("мир".to_owned())]
        );
        engine.undo();
        assert_eq!(
            engine.take_learning(),
            vec![Learning::Retract("мир".to_owned())]
        );
    }

    /// Lexicon detector whose early guess always fires — to model a wrong
    /// guess on a word missing from the dictionaries.
    struct EagerDetector(LexiconDetector);

    impl Detector for EagerDetector {
        fn confidence(&self, context: &DetectionContext<'_>) -> f32 {
            self.0.confidence(context)
        }

        fn early_confidence(&self, _typed: &str, _candidate: &str) -> f32 {
            1.0
        }
    }

    #[test]
    fn a_reverted_early_guess_is_kept() {
        let mut words = detector();
        words.insert("ghbdvbh");
        let mut engine = Engine::new(early_config(), EagerDetector(words));
        replace(type_text(&mut engine, "ghbd", &auto()));
        assert!(engine.take_learning().is_empty());
        // The user finishes a word that only exists in English; the space
        // check converts it back and remembers it.
        let ru = InputContext::new(Profile::Auto, LayoutId::Ru);
        let correction = replace(type_text(&mut engine, "мир ", &ru));
        assert_eq!(correction.converted, "ghbdvbh");
        assert_eq!(engine.take_learning(), vec![keep("ghbdvbh")]);
    }

    #[test]
    fn learning_can_be_disabled() {
        let config = EngineConfig {
            learning: false,
            ..EngineConfig::default()
        };
        let mut engine = Engine::new(config, detector());
        type_text(&mut engine, "ghbdtn ", &auto());
        engine.undo();
        assert!(engine.take_learning().is_empty());
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
    fn trailing_punctuation_is_preserved_until_space() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn", &auto());
        assert_eq!(engine.on_char('.', &auto()), None);
        let correction = replace(engine.on_char(' ', &auto()));
        assert_eq!(correction.original, "ghbdtn.");
        assert_eq!(correction.converted, "привет.");
        assert_eq!(correction.separator, Some(' '));
        assert_eq!(correction.insert_text(true), "привет. ");
    }

    #[test]
    fn corrects_punctuation_letter_at_start_of_word() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let correction = replace(type_text(&mut engine, ";bpym ", &auto()));
        assert_eq!(correction.original, ";bpym");
        assert_eq!(correction.converted, "жизнь");
        assert_eq!(correction.word_chars, 5);
        assert_eq!(correction.separator, Some(' '));
    }

    #[test]
    fn corrects_punctuation_letter_inside_a_word() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let correction = replace(type_text(&mut engine, "cfk.n ", &auto()));
        assert_eq!(correction.original, "cfk.n");
        assert_eq!(correction.converted, "салют");
        assert_eq!(correction.delete_chars(true), 6);
        assert_eq!(correction.insert_text(true), "салют ");
    }

    #[test]
    fn corrects_a_bracket_letter_at_the_end() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let correction = replace(type_text(&mut engine, "cvt[ ", &auto()));
        assert_eq!(correction.original, "cvt[");
        assert_eq!(correction.converted, "смех");
    }

    #[test]
    fn unambiguous_trigger_still_fires_immediately() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        let correction = replace(type_text(&mut engine, "ghbdtn!", &auto()));
        assert_eq!(correction.separator, Some('!'));
        assert_eq!(correction.insert_text(true), "привет!");
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
    fn manual_word_fix_works_after_a_space() {
        let context = InputContext::new(Profile::Manual, LayoutId::Us);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn ", &context);

        let correction = engine.fix_last_word(&context).expect("last word");
        assert_eq!(correction.original, "ghbdtn");
        assert_eq!(correction.converted, "привет");
        assert_eq!(correction.trailing, " ");
        assert_eq!(correction.word_chars, 6);
        assert_eq!(engine.buffer_text(), "привет ");
        assert_eq!(engine.current_word(), "");

        let undo = engine.undo().expect("undo");
        assert!(matches!(undo, Action::Undo(_)));
        assert_eq!(engine.buffer_text(), "ghbdtn ");
    }

    #[test]
    fn manual_phrase_fix_converts_all_words() {
        let context = InputContext::new(Profile::Manual, LayoutId::Us);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn vbh ", &context);

        let correction = engine.fix_last_phrase(&context).expect("phrase");
        assert_eq!(correction.original, "ghbdtn vbh");
        assert_eq!(correction.converted, "привет мир");
        assert_eq!(correction.trailing, " ");
        assert_eq!(engine.buffer_text(), "привет мир ");

        let undo = engine.undo().expect("undo");
        assert!(matches!(undo, Action::Undo(_)));
        assert_eq!(engine.buffer_text(), "ghbdtn vbh ");
    }

    #[test]
    fn phrase_fix_starts_after_the_last_sentence_boundary() {
        let context = InputContext::new(Profile::Manual, LayoutId::Us);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn. vbh ", &context);

        let correction = engine.fix_last_phrase(&context).expect("phrase");
        assert_eq!(correction.original, "vbh");
        assert_eq!(correction.converted, "мир");
        assert_eq!(engine.buffer_text(), "ghbdtn. мир ");
    }

    #[test]
    fn phrase_fix_preserves_punctuation_between_words() {
        let context = InputContext::new(Profile::Manual, LayoutId::Us);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "ghbdtn, vbh", &context);

        let correction = engine.fix_last_phrase(&context).expect("phrase");
        assert_eq!(correction.converted, "привет, мир");
        assert_eq!(engine.buffer_text(), "привет, мир");
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
    fn enter_is_a_trigger_when_enabled() {
        let config = EngineConfig {
            enter_trigger: true,
            ..EngineConfig::default()
        };
        let mut engine = Engine::new(config, detector());
        let correction = replace(type_text(&mut engine, "ghbdtn\n", &auto()));
        assert_eq!(correction.separator, Some('\n'));
        assert_eq!(correction.converted, "привет");
    }

    #[test]
    fn punctuation_stays_in_word_when_triggers_disabled() {
        let config = EngineConfig {
            punct_trigger: false,
            ..EngineConfig::default()
        };
        let context = InputContext::new(Profile::Auto, LayoutId::Ru);
        let mut engine = Engine::new(config, detector());
        assert_eq!(type_text(&mut engine, "руддщ!", &context), None);
        let correction = replace(engine.on_char(' ', &context));
        assert_eq!(correction.original, "руддщ!");
        assert_eq!(correction.converted, "hello!");
    }

    #[test]
    fn shifted_punctuation_is_deferred() {
        let context = InputContext::new(Profile::Auto, LayoutId::Ru);
        let mut engine = Engine::new(EngineConfig::default(), detector());
        type_text(&mut engine, "руддщ", &context);
        let shift = context.with_modifiers(Modifiers {
            shift: true,
            ..Modifiers::default()
        });
        assert_eq!(engine.on_char('!', &shift), None);
        let correction = replace(engine.on_char(' ', &context));
        assert_eq!(correction.converted, "hello!");
    }

    #[test]
    fn backspace_undo_state_is_exposed() {
        let mut engine = Engine::new(EngineConfig::default(), detector());
        assert!(!engine.undo_applied());
        type_text(&mut engine, "ghbdtn ", &auto());
        assert!(engine.undo_applied());
        let _ = engine.undo();
        assert!(!engine.undo_applied());
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
