//! End-to-end checks: the scored detector behind the real engine.

use layoutpilot_core::{
    Action, CorrectionManager, Engine, EngineConfig, EngineState, InputContext, LayoutId, PlanKind,
    Profile,
};
use layoutpilot_dict::DictionaryDetector;

fn engine() -> Engine<DictionaryDetector> {
    Engine::new(EngineConfig::default(), DictionaryDetector::new())
}

fn context(profile: Profile, layout: LayoutId) -> InputContext<'static> {
    InputContext::new(profile, layout)
}

fn type_text<D: layoutpilot_core::Detector>(
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

fn replacement(action: Option<Action>) -> layoutpilot_core::Correction {
    match action {
        Some(Action::Replace(correction)) => correction,
        other => panic!("expected Replace, got {other:?}"),
    }
}

#[test]
fn manager_builds_a_real_correction_plan() {
    let mut manager = CorrectionManager::new(EngineConfig::default(), DictionaryDetector::new());
    let context = context(Profile::Auto, LayoutId::Us);
    let mut plan = None;
    for ch in "ghbdtn ".chars() {
        if let Some(next) = manager.on_char(ch, &context) {
            plan = Some(next);
        }
    }
    let plan = plan.expect("plan");
    assert_eq!(plan.kind, PlanKind::Replace);
    assert_eq!(plan.layout, LayoutId::Ru);
    assert_eq!(plan.backspaces, 6);
    assert_eq!(plan.text, "привет");
    assert!(plan.is_typeable());
}

#[test]
fn corrects_wrong_layout_with_dictionary_scoring() {
    let context = context(Profile::Auto, LayoutId::Us);
    let mut engine = engine();
    let correction = replacement(type_text(&mut engine, "ghbdtn ", &context));
    assert_eq!(correction.original, "ghbdtn");
    assert_eq!(correction.converted, "привет");
    assert_eq!(correction.insert_text(true), "привет ");
    assert_eq!(engine.state(), EngineState::Corrected);
}

#[test]
fn corrects_punctuation_that_is_a_letter() {
    let context = context(Profile::Auto, LayoutId::Us);
    let mut engine = engine();
    let correction = replacement(type_text(&mut engine, "cfk.n ", &context));
    assert_eq!(correction.original, "cfk.n");
    assert_eq!(correction.converted, "салют");
    assert_eq!(correction.insert_text(true), "салют ");
}

#[test]
fn preserves_case_and_punctuation() {
    let context = context(Profile::Auto, LayoutId::Us);
    let mut engine = engine();
    let correction = replacement(type_text(&mut engine, "Ghbdtn, ", &context));
    assert_eq!(correction.original, "Ghbdtn,");
    assert_eq!(correction.converted, "Привет,");
    assert_eq!(correction.insert_text(true), "Привет, ");
}

#[test]
fn does_not_touch_correct_english() {
    let context = context(Profile::Auto, LayoutId::Us);
    let mut engine = engine();
    assert_eq!(type_text(&mut engine, "hello ", &context), None);
    assert!(!engine.undo_available());
}

#[test]
fn does_not_touch_correct_russian() {
    let context = context(Profile::Auto, LayoutId::Ru);
    let mut engine = engine();
    assert_eq!(type_text(&mut engine, "привет ", &context), None);
    assert!(!engine.undo_available());
}

#[test]
fn corrects_russian_typed_in_us_layout() {
    let context = context(Profile::Auto, LayoutId::Ru);
    let mut engine = engine();
    let correction = replacement(type_text(&mut engine, "руддщ ", &context));
    assert_eq!(correction.converted, "hello");
}

#[test]
fn corrects_short_word_with_dictionary_support() {
    let context = context(Profile::Auto, LayoutId::Us);
    let mut engine = engine();
    let correction = replacement(type_text(&mut engine, "ytn ", &context));
    assert_eq!(correction.converted, "нет");
}

#[test]
fn undo_restores_the_mistyped_word() {
    let context = context(Profile::Auto, LayoutId::Us);
    let mut engine = engine();
    type_text(&mut engine, "ghbdtn ", &context);
    assert!(matches!(engine.undo(), Some(Action::Undo(_))));
    assert_eq!(engine.buffer_text(), "ghbdtn ");
}

#[test]
fn urls_are_protected_by_the_engine() {
    let context = context(Profile::Auto, LayoutId::Us);
    let mut engine = engine();
    assert_eq!(
        type_text(&mut engine, "https://example.com/ghbdtn ", &context),
        None
    );
}

#[test]
fn manual_profile_uses_manual_fix() {
    let context = context(Profile::Manual, LayoutId::Us);
    let mut engine = engine();
    assert_eq!(type_text(&mut engine, "ghbdtn ", &context), None);

    type_text(&mut engine, "ghbdtn", &context);
    let correction = engine.fix_last_word(&context).expect("manual fix");
    assert_eq!(correction.converted, "привет");
}
