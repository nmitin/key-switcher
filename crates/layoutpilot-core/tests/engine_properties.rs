//! Property tests for the correction engine.
//!
//! These use a detector that always returns full confidence, so the properties
//! check the state machine and policy gates rather than scoring quality.

use layoutpilot_core::{
    convert_word, Action, Engine, EngineConfig, EngineState, InputContext, LayoutId, Profile,
    StaticDetector,
};
use layoutpilot_layouts::{ru, us};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

const US_LETTERS: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L',
    'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

const TRIGGERS: &[char] = &[' ', ',', '.', '!', '?', ';', ':', ')', ']', '}', '"'];

fn word() -> impl Strategy<Value = String> {
    vec(select(US_LETTERS), 3..20).prop_map(|chars| chars.into_iter().collect())
}

fn short_word() -> impl Strategy<Value = String> {
    vec(select(US_LETTERS), 1..3).prop_map(|chars| chars.into_iter().collect())
}

fn new_engine() -> Engine<StaticDetector> {
    Engine::new(EngineConfig::default(), StaticDetector::new(1.0))
}

fn context() -> InputContext<'static> {
    InputContext::new(Profile::Auto, LayoutId::Us)
}

fn type_text<D: layoutpilot_core::Detector>(engine: &mut Engine<D>, text: &str) -> Option<Action> {
    let context = context();
    let mut action = None;
    for ch in text.chars() {
        if let Some(next) = engine.on_char(ch, &context) {
            action = Some(next);
        }
    }
    action
}

proptest! {
    #[test]
    fn every_word_is_corrected_on_space(word in word()) {
        let expected = convert_word(&word, us(), ru()).unwrap();
        let expected_buffer = format!("{expected} ");
        let input = format!("{word} ");
        let mut engine = new_engine();
        let action = type_text(&mut engine, &input);

        let Some(Action::Replace(correction)) = action else {
            prop_assert!(false, "expected Replace");
            unreachable!();
        };
        prop_assert_eq!(&correction.original, &word);
        prop_assert_eq!(&correction.converted, &expected);
        prop_assert_eq!(engine.buffer_text(), expected_buffer);
        prop_assert_eq!(engine.state(), EngineState::Corrected);
    }

    #[test]
    fn every_correction_can_be_undone_and_redone(word in word()) {
        let input = format!("{word} ");
        let original_buffer = input.clone();
        let mut engine = new_engine();
        type_text(&mut engine, &input);
        let Some(Action::Undo(correction)) = engine.undo() else {
            prop_assert!(false, "expected Undo");
            unreachable!();
        };
        prop_assert_eq!(&correction.original, &word);
        prop_assert_eq!(engine.buffer_text(), original_buffer);

        let Some(Action::Redo(_)) = engine.undo() else {
            prop_assert!(false, "expected Redo");
            unreachable!();
        };
        let redo_buffer = format!("{} ", correction.converted);
        prop_assert_eq!(engine.buffer_text(), redo_buffer);
    }

    #[test]
    fn short_words_are_not_corrected(word in short_word()) {
        let input = format!("{word} ");
        let mut engine = new_engine();
        prop_assert!(type_text(&mut engine, &input).is_none());
        prop_assert!(!engine.undo_available());
    }

    #[test]
    fn every_trigger_fires(word in word()) {
        for trigger in TRIGGERS {
            let input = format!("{word}{trigger}");
            let mut engine = new_engine();
            let action = type_text(&mut engine, &input);
            prop_assert!(matches!(action, Some(Action::Replace(_))));
        }
    }

    #[test]
    fn enter_and_tab_do_not_fire(word in word()) {
        let newline = format!("{word}\n");
        let mut engine = new_engine();
        prop_assert!(type_text(&mut engine, &newline).is_none());

        let tab = format!("{word}\t");
        let mut engine = new_engine();
        prop_assert!(type_text(&mut engine, &tab).is_none());
    }

    #[test]
    fn digits_taint_adjacent_words(word in word()) {
        let trailing_digit = format!("{word}1 ");
        let mut engine = new_engine();
        prop_assert!(type_text(&mut engine, &trailing_digit).is_none());

        let leading_digit = format!("1{word} ");
        let mut engine = new_engine();
        prop_assert!(type_text(&mut engine, &leading_digit).is_none());
    }

    #[test]
    fn input_invalidates_undo(word in word()) {
        let input = format!("{word} ");
        let mut engine = new_engine();
        type_text(&mut engine, &input);
        prop_assert!(engine.undo_available());
        engine.on_char('a', &context());
        prop_assert!(!engine.undo_available());
    }

    #[test]
    fn manual_fix_always_produces_a_correction(word in word()) {
        let expected = convert_word(&word, us(), ru()).unwrap();
        let mut engine = new_engine();
        type_text(&mut engine, &word);
        let correction = engine.fix_last_word(&context()).unwrap();
        prop_assert_eq!(correction.converted, expected);
        prop_assert!(engine.undo_available());
    }
}
