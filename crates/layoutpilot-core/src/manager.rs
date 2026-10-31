//! Composition layer between the engine and a platform injector.
//!
//! [`CorrectionManager`] owns the engine, decides which direction a correction
//! goes (source layout → target layout) and produces a [`CorrectionPlan`] that
//! a platform backend can execute: switch layout, send Backspace N times and
//! type text. It performs no I/O, so plans are easy to test.

use layoutpilot_layouts::LayoutId;

use crate::detector::Detector;
use crate::engine::{Action, Correction, Engine, InputContext, ResetReason};
use crate::profile::EngineConfig;

/// What produced a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    /// Automatic or manual forward correction.
    Replace,
    /// Undo the last correction.
    Undo,
    /// Re-apply the last correction.
    Redo,
}

/// A platform-independent instruction for the injector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionPlan {
    /// What produced this plan.
    pub kind: PlanKind,
    /// Layout to switch to before typing.
    pub layout: LayoutId,
    /// Number of Backspace presses before typing.
    pub backspaces: usize,
    /// Text to type after the backspaces.
    pub text: String,
    /// Separator to type after a short pause, if the correction consumed one.
    pub separator: Option<char>,
}

impl CorrectionPlan {
    /// Whether every character of the plan can be typed in its layout.
    #[must_use]
    pub fn is_typeable(&self) -> bool {
        self.text
            .chars()
            .chain(self.separator)
            .all(|ch| self.layout.table().keystroke_for(ch).is_some())
    }
}

#[derive(Debug, Clone)]
struct LastDirection {
    source: LayoutId,
    target: LayoutId,
    correction: Correction,
}

/// Owns an [`Engine`] and turns its actions into executable plans.
#[derive(Debug, Clone)]
pub struct CorrectionManager<D: Detector> {
    engine: Engine<D>,
    last: Option<LastDirection>,
}

impl<D: Detector> CorrectionManager<D> {
    /// Create a manager.
    #[must_use]
    pub fn new(config: EngineConfig, detector: D) -> Self {
        Self {
            engine: Engine::new(config, detector),
            last: None,
        }
    }

    /// Engine reference.
    #[must_use]
    pub const fn engine(&self) -> &Engine<D> {
        &self.engine
    }

    /// Mutable engine reference.
    pub const fn engine_mut(&mut self) -> &mut Engine<D> {
        &mut self.engine
    }

    /// Process a character and return a plan if the engine decided to act.
    pub fn on_char(&mut self, ch: char, context: &InputContext<'_>) -> Option<CorrectionPlan> {
        let action = self.engine.on_char(ch, context)?;
        self.plan_action(action, context.active_layout)
    }

    /// Process Backspace; this never produces a plan.
    pub fn on_backspace(&mut self) {
        self.engine.on_backspace();
    }

    /// Manually convert the current word (FR-03).
    pub fn on_fix_last_word(&mut self, context: &InputContext<'_>) -> Option<CorrectionPlan> {
        let correction = self.engine.fix_last_word(context)?;
        let source = context.active_layout;
        let target = source.other();
        let plan = CorrectionPlan {
            kind: PlanKind::Replace,
            layout: target,
            backspaces: correction.word_chars + usize::from(correction.separator.is_some()),
            text: correction.converted.clone(),
            separator: correction.separator,
        };
        self.last = Some(LastDirection {
            source,
            target,
            correction,
        });
        Some(plan)
    }

    /// Undo or redo the last correction.
    pub fn undo(&mut self) -> Option<CorrectionPlan> {
        let action = self.engine.undo()?;
        self.plan_action(action, self.last.as_ref()?.source)
    }

    /// Drop context, undo state and last direction.
    pub fn reset(&mut self, reason: ResetReason) {
        self.engine.reset(reason);
        self.last = None;
    }

    /// Pause the engine and clear context.
    pub fn pause(&mut self) {
        self.engine.pause();
        self.last = None;
    }

    /// Resume the engine.
    pub fn resume(&mut self) {
        self.engine.resume();
    }

    /// Last applied correction, if any.
    #[must_use]
    pub fn last_correction(&self) -> Option<&Correction> {
        self.last.as_ref().map(|last| &last.correction)
    }

    fn plan_action(&mut self, action: Action, active_layout: LayoutId) -> Option<CorrectionPlan> {
        match action {
            Action::Replace(correction) => {
                let source = active_layout;
                let target = source.other();
                let plan = CorrectionPlan {
                    kind: PlanKind::Replace,
                    layout: target,
                    backspaces: correction.word_chars + usize::from(correction.separator.is_some()),
                    text: correction.converted.clone(),
                    separator: correction.separator,
                };
                self.last = Some(LastDirection {
                    source,
                    target,
                    correction,
                });
                Some(plan)
            }
            Action::Undo(correction) => {
                let last = self.last.as_ref()?;
                Some(CorrectionPlan {
                    kind: PlanKind::Undo,
                    layout: last.source,
                    backspaces: correction.word_chars + usize::from(correction.separator.is_some()),
                    text: correction.original,
                    separator: correction.separator,
                })
            }
            Action::Redo(correction) => {
                let last = self.last.as_ref()?;
                Some(CorrectionPlan {
                    kind: PlanKind::Redo,
                    layout: last.target,
                    backspaces: correction.word_chars + usize::from(correction.separator.is_some()),
                    text: correction.converted,
                    separator: correction.separator,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::StaticDetector;
    use crate::engine::{InputContext, Modifiers};
    use crate::profile::{EngineConfig, Profile};

    fn manager() -> CorrectionManager<StaticDetector> {
        CorrectionManager::new(EngineConfig::default(), StaticDetector::new(1.0))
    }

    fn context(profile: Profile, layout: LayoutId) -> InputContext<'static> {
        InputContext::new(profile, layout)
    }

    fn type_text<D: Detector>(
        manager: &mut CorrectionManager<D>,
        text: &str,
        context: &InputContext<'_>,
    ) -> Option<CorrectionPlan> {
        let mut plan = None;
        for ch in text.chars() {
            if let Some(next) = manager.on_char(ch, context) {
                plan = Some(next);
            }
        }
        plan
    }

    #[test]
    fn creates_a_forward_plan() {
        let mut manager = manager();
        let plan = type_text(
            &mut manager,
            "ghbdtn ",
            &context(Profile::Auto, LayoutId::Us),
        )
        .expect("plan");
        assert_eq!(plan.kind, PlanKind::Replace);
        assert_eq!(plan.layout, LayoutId::Ru);
        assert_eq!(plan.backspaces, 7);
        assert_eq!(plan.text, "привет");
        assert_eq!(plan.separator, Some(' '));
        assert!(plan.is_typeable());
    }

    #[test]
    fn undo_and_redo_swap_direction() {
        let mut manager = manager();
        let context = context(Profile::Auto, LayoutId::Us);
        type_text(&mut manager, "ghbdtn ", &context);

        let undo = manager.undo().expect("undo");
        assert_eq!(undo.kind, PlanKind::Undo);
        assert_eq!(undo.layout, LayoutId::Us);
        assert_eq!(undo.backspaces, 7);
        assert_eq!(undo.text, "ghbdtn");
        assert_eq!(undo.separator, Some(' '));

        let redo = manager.undo().expect("redo");
        assert_eq!(redo.kind, PlanKind::Redo);
        assert_eq!(redo.layout, LayoutId::Ru);
        assert_eq!(redo.backspaces, 7);
        assert_eq!(redo.text, "привет");
        assert_eq!(redo.separator, Some(' '));
    }

    #[test]
    fn manual_fix_has_no_separator() {
        let mut manager = manager();
        let context = context(Profile::Manual, LayoutId::Us);
        type_text(&mut manager, "ghbdtn", &context);
        let plan = manager.on_fix_last_word(&context).expect("plan");
        assert_eq!(plan.kind, PlanKind::Replace);
        assert_eq!(plan.layout, LayoutId::Ru);
        assert_eq!(plan.backspaces, 6);
        assert_eq!(plan.text, "привет");
        assert_eq!(plan.separator, None);
    }

    #[test]
    fn manual_profile_does_not_auto_correct() {
        let mut manager = manager();
        let context = context(Profile::Manual, LayoutId::Us);
        assert_eq!(type_text(&mut manager, "ghbdtn ", &context), None);
    }

    #[test]
    fn reset_drops_undo_and_direction() {
        let mut manager = manager();
        let context = context(Profile::Auto, LayoutId::Us);
        type_text(&mut manager, "ghbdtn ", &context);
        assert!(manager.last_correction().is_some());
        manager.reset(ResetReason::FocusChange);
        assert!(manager.last_correction().is_none());
        assert_eq!(manager.undo(), None);
    }

    #[test]
    fn backspace_does_not_create_a_plan() {
        let mut manager = manager();
        let context = context(Profile::Auto, LayoutId::Us);
        type_text(&mut manager, "ghbdtn", &context);
        manager.on_backspace();
        assert_eq!(manager.engine().current_word(), "ghbdt");
    }

    #[test]
    fn modifier_shortcut_resets_context() {
        let mut manager = manager();
        let context = context(Profile::Auto, LayoutId::Us);
        type_text(&mut manager, "ghbdtn", &context);
        let shortcut = context.with_modifiers(Modifiers {
            ctrl: true,
            ..Modifiers::default()
        });
        assert_eq!(manager.on_char('c', &shortcut), None);
        assert_eq!(manager.engine().buffer_text(), "");
    }
}
