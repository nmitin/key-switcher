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
    assert_eq!(plan.backspaces, 7);
    assert_eq!(plan.text, "привет");
    assert_eq!(plan.separator, Some(' '));
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

fn corrections(text: &str, layout: LayoutId) -> Vec<(String, String)> {
    let mut engine = engine();
    let context = context(Profile::Auto, layout);
    text.chars()
        .filter_map(|ch| match engine.on_char(ch, &context) {
            Some(Action::Replace(correction)) => Some((correction.original, correction.converted)),
            _ => None,
        })
        .collect()
}

fn pair(original: &str, converted: &str) -> (String, String) {
    (original.to_owned(), converted.to_owned())
}

#[test]
fn short_words_before_a_corrected_word_are_converted_too() {
    assert_eq!(
        corrections("f jy yt ghbitk ", LayoutId::Us),
        vec![pair("f jy yt ghbitk", "а он не пришел")]
    );
    assert_eq!(
        corrections("b z nj;t ghbitk ", LayoutId::Us),
        vec![pair("b z nj;t", "и я тоже"), pair("ghbitk", "пришел")]
    );
    assert_eq!(
        corrections("шт еру ьщктштп ", LayoutId::Ru),
        vec![pair("шт еру", "in the"), pair("ьщктштп", "morning")]
    );
}

#[test]
fn real_words_stop_the_lookback() {
    for prefix in ["I", "a", "in", "ok", "to", "it", "the", "hello", "PR"] {
        let found = corrections(&format!("{prefix} ghbdtn "), LayoutId::Us);
        assert_eq!(found, vec![pair("ghbdtn", "привет")], "prefix {prefix}");
    }
    for prefix in ["и", "в", "я"] {
        let found = corrections(&format!("{prefix} ыщьуерштп "), LayoutId::Ru);
        assert_eq!(
            found,
            vec![pair("ыщьуерштп", "something")],
            "prefix {prefix}"
        );
    }
}

/// Type physical keys (given as `us` characters) like the daemon does: an
/// early correction switches the layout, and later keys produce characters of
/// the new layout. Returns the resulting text.
fn type_with_early_trigger(keys: &str) -> String {
    let config = EngineConfig {
        early_trigger: true,
        ..EngineConfig::default()
    };
    let mut engine = Engine::new(config, DictionaryDetector::new());
    let mut layout = LayoutId::Us;
    for key in keys.chars() {
        let ch = if layout == LayoutId::Us || key == ' ' {
            key
        } else {
            let stroke = LayoutId::Us.table().keystroke_for(key).expect("us key");
            LayoutId::Ru.table().char_at(stroke).expect("ru char")
        };
        let context = InputContext::new(Profile::Auto, layout);
        if let Some(Action::Replace(_)) = engine.on_char(ch, &context) {
            layout = layout.other();
        }
    }
    engine.buffer_text()
}

#[test]
fn early_trigger_switches_layout_mid_word() {
    assert_eq!(type_with_early_trigger("ghbdtn "), "привет ");
    assert_eq!(
        type_with_early_trigger("f jy yt ghbitk "),
        "а он не пришел "
    );
}

#[test]
fn early_trigger_leaves_real_and_technical_words_alone() {
    for word in [
        "hello", "kubectl", "systemd", "yakuake", "github", "nginx", "postgres", "inertia",
        "wayland",
    ] {
        let text = format!("{word} ");
        assert_eq!(type_with_early_trigger(&text), text, "{word}");
    }
}

#[test]
fn early_trigger_detector_needs_an_impossible_prefix() {
    let detector = DictionaryDetector::new();
    assert!(detector.early_confidence_for("ghbd", "прив") > 0.9);
    assert!(detector.early_confidence_for("hell", "рудд") < 0.1);
    assert!(detector.early_confidence_for("руддщ", "hello") > 0.9);
    assert!(detector.early_confidence_for("прив", "ghbd") < 0.1);
}

#[test]
fn undone_word_is_not_corrected_again() {
    let mut engine = engine();
    let us = context(Profile::Auto, LayoutId::Us);
    replacement(type_text(&mut engine, "ghbdtn ", &us));
    engine.undo();
    assert_eq!(
        engine.take_learning(),
        vec![layoutpilot_core::Learning::Keep("ghbdtn".to_owned())]
    );
    assert_eq!(type_text(&mut engine, "ghbdtn ", &us), None);
    // Other words are still corrected.
    replacement(type_text(&mut engine, "vbh ", &us));
}

#[test]
fn undone_early_prefix_does_not_fire_again() {
    let config = EngineConfig {
        early_trigger: true,
        ..EngineConfig::default()
    };
    let mut engine = Engine::new(config, DictionaryDetector::new());
    let us = context(Profile::Auto, LayoutId::Us);
    let correction = replacement(type_text(&mut engine, "ghbd", &us));
    assert_eq!(correction.converted, "прив");
    engine.undo();
    assert_eq!(type_text(&mut engine, " ghbd", &us), None);
}

#[test]
fn learning_survives_a_replay() {
    use layoutpilot_core::Learning;
    let mut detector = DictionaryDetector::new();
    for event in [
        Learning::Keep("ghbdtn".to_owned()),
        Learning::Accept("ghbdtn".to_owned()),
        Learning::Keep("rjulf".to_owned()),
    ] {
        detector.apply_learning(&event);
    }
    // Accept lifts an earlier Keep.
    assert!(!detector.is_kept("ghbdtn"));
    assert!(detector.is_kept("rjulf"));
    assert!(detector.confidence_for("rjulf", "когда") < 0.01);
    assert!(detector.early_confidence_for("rjul", "когд") < 0.01);
    assert_eq!(detector.learned_counts(), (1, 1));
}

fn auto_result(text: &str, layout: LayoutId) -> String {
    let mut engine = engine();
    let context = context(Profile::Auto, layout);
    for ch in text.chars() {
        let _ = engine.on_char(ch, &context);
    }
    engine.buffer_text()
}

#[test]
fn trailing_punctuation_letter_is_read_as_a_letter_when_it_makes_a_word() {
    assert_eq!(auto_result("pyf. ", LayoutId::Us), "знаю ");
    assert_eq!(auto_result("z yt pyf. ", LayoutId::Us), "я не знаю ");
}

#[test]
fn trailing_punctuation_stays_punctuation_otherwise() {
    for (typed, expected) in [
        ("ghbdtn, ", "привет, "),
        ("ghbdtn. ", "привет. "),
        ("rfr ltkf, ", "как дела, "),
        ("rjulf. ", "когда. "),
        ("ve;xbyf; ", "мужчина; "),
        ("hello. ", "hello. "),
        ("etc. ", "etc. "),
    ] {
        assert_eq!(auto_result(typed, LayoutId::Us), expected, "{typed:?}");
    }
}

#[test]
fn manual_fix_reads_a_trailing_letter_too() {
    let manual = context(Profile::Manual, LayoutId::Us);
    for (typed, expected) in [("pyf.", "знаю"), ("ghbdtn,", "привет,")] {
        let mut fresh = engine();
        type_text(&mut fresh, typed, &manual);
        let correction = fresh.fix_last_word(&manual).expect("fix");
        assert_eq!(correction.converted, expected, "{typed:?}");
    }
}
