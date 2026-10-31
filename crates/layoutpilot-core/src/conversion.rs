//! Conversion between layouts by reinterpreting physical key presses.
//!
//! The core operation is not transliteration: the same physical keys are read
//! in another layout. For example `ghbdtn` typed in US becomes `привет` in RU
//! because `g`, `h`, `b`, ... occupy the same keys as `п`, `р`, `и`, ...
//!
//! Two levels of strictness are provided:
//!
//! * [`reinterpret`] / [`convert_text`] map letters and copy everything else;
//! * [`convert_word`] additionally requires that every mapped character is a
//!   letter, which rejects words like `хит` (the `х` key produces `[` in US).

use layoutpilot_layouts::{Keystroke, LayoutId, LayoutTable};

use crate::segmenter::{is_word_char, last_word};

/// Errors produced by layout conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionError {
    /// [`convert_word`] received an empty string.
    EmptyWord,
    /// [`convert_word`] received a character that cannot be part of a word.
    NotAWord {
        /// The offending character.
        ch: char,
    },
    /// The source layout has no physical key for this character.
    MissingSourceKeystroke {
        /// The character that could not be typed.
        ch: char,
        /// Source layout.
        layout: LayoutId,
    },
    /// The target layout has no character on the required physical key.
    MissingTargetChar {
        /// The source character being converted.
        ch: char,
        /// The physical key.
        keystroke: Keystroke,
        /// Target layout.
        layout: LayoutId,
    },
    /// The converted word is not typeable in the target layout as a word.
    ///
    /// This happens for Russian letters that live on punctuation keys: `х`
    /// becomes `[`, `ж` becomes `;`, and so on. Such a result is physically
    /// correct but cannot be a word candidate, so the automatic path rejects it.
    NotReversible {
        /// The physically converted string (kept for diagnostics).
        converted: String,
        /// Target layout.
        target: LayoutId,
        /// Characters that made the result non-word-like.
        blocked: Vec<char>,
    },
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyWord => f.write_str("word is empty"),
            Self::NotAWord { ch } => write!(f, "{ch:?} is not a word character"),
            Self::MissingSourceKeystroke { ch, layout } => {
                write!(f, "layout {layout} has no key for {ch:?}")
            }
            Self::MissingTargetChar {
                ch,
                keystroke,
                layout,
            } => write!(
                f,
                "layout {layout} has no character at {keystroke} (source {ch:?})"
            ),
            Self::NotReversible {
                converted,
                target,
                blocked,
            } => write!(
                f,
                "converted word {converted:?} is not reversible in {target}: {blocked:?}"
            ),
        }
    }
}

impl std::error::Error for ConversionError {}

/// Reinterpret every letter of `text` in the target layout.
///
/// Non-alphabetic characters are copied unchanged (FR-06): punctuation,
/// digits, spaces and emoji survive conversion. The result may contain
/// non-letters when a source letter lives on a punctuation key in the target
/// layout; use [`convert_word`] to reject such results.
///
/// # Errors
///
/// Returns an error if a letter has no key in the source layout or the target
/// layout has no character on the corresponding key.
pub fn reinterpret(
    text: &str,
    source: &LayoutTable,
    target: &LayoutTable,
) -> Result<String, ConversionError> {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        if !ch.is_alphabetic() {
            output.push(ch);
            continue;
        }
        let keystroke =
            source
                .keystroke_for(ch)
                .ok_or(ConversionError::MissingSourceKeystroke {
                    ch,
                    layout: source.id(),
                })?;
        let mapped = target
            .char_at(keystroke)
            .ok_or(ConversionError::MissingTargetChar {
                ch,
                keystroke,
                layout: target.id(),
            })?;
        output.push(mapped);
    }
    Ok(output)
}

/// Convert a text fragment, preserving punctuation and digits.
///
/// This is [`reinterpret`] under a name that reflects the user-facing action.
///
/// # Errors
///
/// See [`reinterpret`].
pub fn convert_text(
    text: &str,
    source: &LayoutTable,
    target: &LayoutTable,
) -> Result<String, ConversionError> {
    reinterpret(text, source, target)
}

/// Convert a single word, requiring the result to stay a word.
///
/// Unlike [`reinterpret`], this function rejects results where a letter maps to
/// punctuation in the target layout (for example `хит` → `[bn` in US). Those
/// results are physically correct but are not valid word candidates for the
/// automatic correction path (FR-38).
///
/// The word may contain hyphens and apostrophes; they are preserved.
///
/// # Errors
///
/// Returns [`ConversionError::EmptyWord`] for an empty string,
/// [`ConversionError::NotAWord`] for characters outside the word alphabet, and
/// [`ConversionError::NotReversible`] if a letter maps to a non-letter.
pub fn convert_word(
    word: &str,
    source: &LayoutTable,
    target: &LayoutTable,
) -> Result<String, ConversionError> {
    if word.is_empty() {
        return Err(ConversionError::EmptyWord);
    }

    let mut output = String::with_capacity(word.len());
    let mut blocked = Vec::new();
    for ch in word.chars() {
        if !is_word_char(ch) {
            return Err(ConversionError::NotAWord { ch });
        }
        if !ch.is_alphabetic() {
            output.push(ch);
            continue;
        }
        let keystroke =
            source
                .keystroke_for(ch)
                .ok_or(ConversionError::MissingSourceKeystroke {
                    ch,
                    layout: source.id(),
                })?;
        let mapped = target
            .char_at(keystroke)
            .ok_or(ConversionError::MissingTargetChar {
                ch,
                keystroke,
                layout: target.id(),
            })?;
        if !mapped.is_alphabetic() {
            blocked.push(mapped);
        }
        output.push(mapped);
    }
    if !blocked.is_empty() {
        return Err(ConversionError::NotReversible {
            converted: output,
            target: target.id(),
            blocked,
        });
    }
    // Preserved characters (apostrophes, hyphens, digits) must also be
    // typeable in the target layout; otherwise replaying physical keys would
    // produce a different string (FR-38).
    if !target.can_type(&output) {
        return Err(ConversionError::NotReversible {
            converted: output.clone(),
            target: target.id(),
            blocked: target.untypeable(&output),
        });
    }
    Ok(output)
}

/// The result of converting the last word in a text fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastWordConversion {
    /// Byte offset of the word start.
    pub start: usize,
    /// Byte offset of the word end (exclusive).
    pub end: usize,
    /// Original word as typed.
    pub original: String,
    /// Converted word.
    pub converted: String,
}

impl LastWordConversion {
    /// Whether conversion changed the word.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.original != self.converted
    }
}

/// Convert the last word in `text`, ignoring trailing separators.
///
/// Returns `Ok(None)` when the text has no word. This is the primitive the
/// correction engine will call on a separator.
///
/// # Errors
///
/// See [`convert_word`].
pub fn convert_last_word(
    text: &str,
    source: &LayoutTable,
    target: &LayoutTable,
) -> Result<Option<LastWordConversion>, ConversionError> {
    let Some(word) = last_word(text) else {
        return Ok(None);
    };
    let converted = convert_word(word.text, source, target)?;
    Ok(Some(LastWordConversion {
        start: word.start,
        end: word.end,
        original: word.text.to_owned(),
        converted,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use layoutpilot_layouts::{ru, us};

    #[test]
    fn converts_latin_typed_in_us_to_russian() {
        assert_eq!(convert_word("ghbdtn", us(), ru()).unwrap(), "привет");
        assert_eq!(convert_word("rjvfylf", us(), ru()).unwrap(), "команда");
    }

    #[test]
    fn converts_russian_typed_in_ru_to_latin() {
        assert_eq!(convert_word("привет", ru(), us()).unwrap(), "ghbdtn");
        assert_eq!(convert_word("команда", ru(), us()).unwrap(), "rjvfylf");
    }

    #[test]
    fn preserves_case() {
        assert_eq!(convert_word("Ghbdtn", us(), ru()).unwrap(), "Привет");
        assert_eq!(convert_word("GHBDTN", us(), ru()).unwrap(), "ПРИВЕТ");
        assert_eq!(convert_word("Привет", ru(), us()).unwrap(), "Ghbdtn");
        assert_eq!(convert_word("ПРИВЕТ", ru(), us()).unwrap(), "GHBDTN");
    }

    #[test]
    fn preserves_punctuation_and_digits_in_text() {
        assert_eq!(
            convert_text("ghbdtn, world! 123", us(), ru()).unwrap(),
            "привет, цщкдв! 123"
        );
    }

    #[test]
    fn punctuation_inside_a_word_is_preserved() {
        assert_eq!(convert_word("stop-go", us(), ru()).unwrap(), "ыещз-пщ");
    }

    #[test]
    fn reinterpret_is_pure_physical_mapping() {
        assert_eq!(reinterpret("хит", ru(), us()).unwrap(), "[bn");
        assert_eq!(reinterpret("ёж", ru(), us()).unwrap(), "`;");
    }

    #[test]
    fn word_conversion_rejects_letters_on_punctuation_keys() {
        let error = convert_word("хит", ru(), us()).unwrap_err();
        match error {
            ConversionError::NotReversible {
                converted,
                target,
                blocked,
            } => {
                assert_eq!(converted, "[bn");
                assert_eq!(target, LayoutId::Us);
                assert_eq!(blocked, vec!['[']);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn rejects_preserved_punctuation_that_is_not_typeable() {
        let error = convert_word("don't", us(), ru()).unwrap_err();
        assert!(matches!(
            error,
            ConversionError::NotReversible {
                target: LayoutId::Ru,
                ..
            }
        ));
    }

    #[test]
    fn rejects_non_word_characters() {
        assert_eq!(
            convert_word("ghbdtn,", us(), ru()),
            Err(ConversionError::NotAWord { ch: ',' })
        );
        assert_eq!(
            convert_word("", us(), ru()),
            Err(ConversionError::EmptyWord)
        );
    }

    #[test]
    fn rejects_letters_missing_from_source_layout() {
        let error = convert_word("ß", us(), ru()).unwrap_err();
        assert!(matches!(
            error,
            ConversionError::MissingSourceKeystroke {
                ch: 'ß',
                layout: LayoutId::Us
            }
        ));
    }

    #[test]
    fn round_trip_on_shared_letters() {
        for word in ["ghbdtn", "rjvfylf", "Hello", "WORLD"] {
            let ru_word = convert_word(word, us(), ru()).unwrap();
            let back = convert_word(&ru_word, ru(), us()).unwrap();
            assert_eq!(back, word);
        }
        for word in ["привет", "команда", "Мир"] {
            let us_word = convert_word(word, ru(), us()).unwrap();
            let back = convert_word(&us_word, us(), ru()).unwrap();
            assert_eq!(back, word);
        }
    }

    #[test]
    fn converts_last_word_ignoring_separators() {
        let conversion = convert_last_word("привет, мир! ", ru(), us())
            .unwrap()
            .unwrap();
        assert_eq!(conversion.original, "мир");
        assert_eq!(conversion.converted, "vbh");
        assert_eq!((conversion.start, conversion.end), (14, 20));
        assert!(conversion.changed());
    }

    #[test]
    fn last_word_without_letters_is_none() {
        assert_eq!(convert_last_word("123 !!!", us(), ru()), Ok(None));
    }
}
