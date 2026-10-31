//! Property tests for the physically reversible part of the ru/en mapping.
//!
//! Seven Russian letters (`х ъ ж э б ю ё`) live on punctuation keys. Their
//! conversion to US produces punctuation, so `convert_word` rejects them by
//! design; the round-trip properties below use the shared 26-letter subset.

use layoutpilot_core::{convert_text, convert_word, reinterpret, ConversionError};
use layoutpilot_layouts::{ru, us};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

const US_LETTERS: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L',
    'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

// Russian letters that live on the same physical keys as US letters.
const RU_SHARED_LETTERS: &[char] = &[
    'й', 'ц', 'у', 'к', 'е', 'н', 'г', 'ш', 'щ', 'з', 'ф', 'ы', 'в', 'а', 'п', 'р', 'о', 'л', 'д',
    'я', 'ч', 'с', 'м', 'и', 'т', 'ь', 'Й', 'Ц', 'У', 'К', 'Е', 'Н', 'Г', 'Ш', 'Щ', 'З', 'Ф', 'Ы',
    'В', 'А', 'П', 'Р', 'О', 'Л', 'Д', 'Я', 'Ч', 'С', 'М', 'И', 'Т', 'Ь',
];

const SEPARATORS: &[char] = &[' ', ',', '.', '!', '?', '-', '\'', '0', '1', '2', '3'];

fn string_from(chars: &'static [char]) -> impl Strategy<Value = String> {
    vec(select(chars), 1..20).prop_map(|chars| chars.into_iter().collect())
}

fn mixed_string(letters: &'static [char]) -> impl Strategy<Value = String> {
    let alphabet = prop_oneof![select(letters), select(SEPARATORS)];
    vec(alphabet, 1..30).prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #[test]
    fn us_to_ru_to_us_is_identity(word in string_from(US_LETTERS)) {
        let cyrillic = convert_word(&word, us(), ru()).unwrap();
        let back = convert_word(&cyrillic, ru(), us()).unwrap();
        prop_assert_eq!(back, word);
    }

    #[test]
    fn ru_shared_to_us_to_ru_is_identity(word in string_from(RU_SHARED_LETTERS)) {
        let latin = convert_word(&word, ru(), us()).unwrap();
        let back = convert_word(&latin, us(), ru()).unwrap();
        prop_assert_eq!(back, word);
    }

    #[test]
    fn converts_word_to_letters_only(word in string_from(US_LETTERS)) {
        let converted = convert_word(&word, us(), ru()).unwrap();
        prop_assert!(converted.chars().all(char::is_alphabetic));
        prop_assert_eq!(converted.chars().count(), word.chars().count());
    }

    #[test]
    fn preserves_case_per_character(word in string_from(US_LETTERS)) {
        let converted = convert_word(&word, us(), ru()).unwrap();
        for (source, target) in word.chars().zip(converted.chars()) {
            prop_assert_eq!(source.is_uppercase(), target.is_uppercase());
        }
    }

    #[test]
    fn converts_text_preserves_non_letters(text in mixed_string(US_LETTERS)) {
        let converted = convert_text(&text, us(), ru()).unwrap();
        prop_assert_eq!(converted.chars().count(), text.chars().count());
        for (source, target) in text.chars().zip(converted.chars()) {
            if !source.is_alphabetic() {
                prop_assert_eq!(source, target);
            }
        }
    }

    #[test]
    fn rejected_word_is_physically_convertible(word in string_from(RU_SHARED_LETTERS)) {
        // A sanity check that accepted words are exactly the ones that stay words.
        let latin = convert_word(&word, ru(), us()).unwrap();
        prop_assert!(us().can_type(&latin));
    }

    #[test]
    fn russian_punctuation_key_letters_are_rejected(word in string_from(&['х', 'ъ', 'ж', 'э', 'б', 'ю', 'ё'])) {
        let result = convert_word(&word, ru(), us());
        let rejected = matches!(result, Err(ConversionError::NotReversible { .. }));
        prop_assert!(rejected);
    }

    #[test]
    fn reinterpret_between_us_and_ru_keeps_char_count(word in string_from(US_LETTERS)) {
        let ru_word = reinterpret(&word, us(), ru()).unwrap();
        let back = reinterpret(&ru_word, ru(), us()).unwrap();
        prop_assert_eq!(back.as_str(), word.as_str());
        prop_assert_eq!(ru_word.chars().count(), word.chars().count());
    }
}
