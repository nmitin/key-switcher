//! Normalize X11 key events into characters using the XKB layout tables.

use layoutpilot_core::{Keystroke, LayoutTable, Modifiers};
use x11rb::protocol::xproto::KeyButMask;

use crate::capture::KeyEvent;

/// Convert a key press into a character for `layout`.
///
/// Caps Lock is applied to letters only; punctuation follows Shift exactly.
#[must_use]
pub fn char_for(event: &KeyEvent, layout: &LayoutTable) -> Option<char> {
    if !event.pressed {
        return None;
    }
    let shift = event.state & u16::from(KeyButMask::SHIFT) != 0;
    let caps_lock = event.state & u16::from(KeyButMask::LOCK) != 0;
    let keycode = u16::from(event.keycode);

    let base = layout.char_at(Keystroke::new(keycode, 1));
    let level = if shift || (caps_lock && base.is_some_and(char::is_alphabetic)) {
        2
    } else {
        1
    };
    layout.char_at(Keystroke::new(keycode, level))
}

/// Extract the modifier snapshot for the engine policy.
#[must_use]
pub fn modifiers(event: &KeyEvent) -> Modifiers {
    let bit = |mask: KeyButMask| event.state & u16::from(mask) != 0;
    Modifiers {
        ctrl: bit(KeyButMask::CONTROL),
        alt: bit(KeyButMask::MOD1),
        shift: bit(KeyButMask::SHIFT),
        super_key: bit(KeyButMask::MOD4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layoutpilot_core::LayoutId;

    fn event(keycode: u8, state: u16) -> KeyEvent {
        KeyEvent {
            keycode,
            state,
            time: 0,
            pressed: true,
        }
    }

    #[test]
    fn normalizes_letters_and_shift() {
        assert_eq!(char_for(&event(24, 0), LayoutId::Us.table()), Some('q'));
        assert_eq!(
            char_for(
                &event(24, u16::from(KeyButMask::SHIFT)),
                LayoutId::Us.table()
            ),
            Some('Q')
        );
        assert_eq!(char_for(&event(24, 0), LayoutId::Ru.table()), Some('й'));
        assert_eq!(
            char_for(
                &event(24, u16::from(KeyButMask::SHIFT)),
                LayoutId::Ru.table()
            ),
            Some('Й')
        );
    }

    #[test]
    fn caps_lock_affects_letters_only() {
        assert_eq!(
            char_for(
                &event(24, u16::from(KeyButMask::LOCK)),
                LayoutId::Us.table()
            ),
            Some('Q')
        );
        assert_eq!(
            char_for(
                &event(20, u16::from(KeyButMask::LOCK)),
                LayoutId::Us.table()
            ),
            Some('-')
        );
    }

    #[test]
    fn extracts_modifiers() {
        let state = u16::from(KeyButMask::SHIFT)
            | u16::from(KeyButMask::CONTROL)
            | u16::from(KeyButMask::MOD4);
        let modifiers = modifiers(&event(24, state));
        assert!(modifiers.shift);
        assert!(modifiers.ctrl);
        assert!(modifiers.super_key);
        assert!(!modifiers.alt);
    }

    #[test]
    fn release_has_no_character() {
        let mut release = event(24, 0);
        release.pressed = false;
        assert_eq!(char_for(&release, LayoutId::Us.table()), None);
    }
}
