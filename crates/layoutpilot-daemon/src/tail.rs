//! Keys the user typed while a correction was being prepared or injected.
//!
//! Capture is read-only: a key pressed after the trigger reaches the focused
//! application before (or while) the daemon types the replacement. If the
//! plan ignored it, the Backspace count would be short by one per key and the
//! text would end up interleaved (`ghbdtn r` → `gпривет `). Instead the
//! daemon collects these keys ("the tail"), deletes them together with the
//! word and retypes them in the target layout — so `ghbdtn rfr` typed in one
//! burst becomes `привет как`.
//!
//! Keys that cannot be replayed as text (Backspace, Enter, arrows, shortcuts)
//! make the correction unsafe; it is then skipped.

use layoutpilot_core::LayoutTable;
use layoutpilot_x11::{char_for, KeyEvent};

use crate::{
    is_modifier, ALT_MASK, CONTROL_MASK, EVDEV_CAPSLOCK, EVDEV_LEFTSHIFT, EVDEV_RIGHTSHIFT,
    SPACE_KEYCODE, SUPER_MASK,
};

/// A physical key event that is already normalized (own events filtered out,
/// modifier state resolved) but not yet fed to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingKey {
    pub event: KeyEvent,
    /// An evdev modifier: it bypasses `handle_key` and only drives the Shift
    /// gesture, exactly as in the live path.
    pub evdev_modifier: bool,
}

/// Why a tail cannot be replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Blocked {
    /// Ctrl/Alt/Super was pressed: the user started a shortcut.
    Shortcut,
    /// A key without a printable character in the target layout.
    NonText(u8),
}

/// Translate the presses in `keys` into the text they produce in `target`.
///
/// Releases and Shift/CapsLock are transparent. Any other key makes the tail
/// unsafe to replay.
pub(crate) fn tail_text(keys: &[PendingKey], target: &LayoutTable) -> Result<String, Blocked> {
    let mut text = String::new();
    for key in keys {
        let event = key.event;
        if !event.pressed {
            continue;
        }
        let evdev = u16::from(event.keycode).saturating_sub(8);
        if is_modifier(evdev) || evdev == EVDEV_CAPSLOCK {
            if matches!(evdev, EVDEV_LEFTSHIFT | EVDEV_RIGHTSHIFT | EVDEV_CAPSLOCK) {
                continue;
            }
            return Err(Blocked::Shortcut);
        }
        if event.state & (CONTROL_MASK | ALT_MASK | SUPER_MASK) != 0 {
            return Err(Blocked::Shortcut);
        }
        if event.keycode == SPACE_KEYCODE {
            text.push(' ');
            continue;
        }
        match char_for(&event, target) {
            Some(ch) if !ch.is_control() => text.push(ch),
            _ => return Err(Blocked::NonText(event.keycode)),
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BACKSPACE_KEYCODE, LOCK_MASK, RETURN_KEYCODE, SHIFT_MASK};
    use layoutpilot_core::LayoutId;

    fn key(keycode: u8, state: u16, pressed: bool) -> PendingKey {
        PendingKey {
            event: KeyEvent {
                keycode,
                state,
                time: 0,
                pressed,
            },
            evdev_modifier: false,
        }
    }

    /// Press + release of the key that types `ch` in `us`.
    fn typed(ch: char) -> [PendingKey; 2] {
        let stroke = LayoutId::Us.table().keystroke_for(ch).expect("us key");
        let keycode = u8::try_from(stroke.keycode).expect("keycode");
        let state = if stroke.needs_shift() { SHIFT_MASK } else { 0 };
        [key(keycode, state, true), key(keycode, state, false)]
    }

    fn typed_str(text: &str) -> Vec<PendingKey> {
        text.chars().flat_map(typed).collect()
    }

    #[test]
    fn empty_tail_is_empty_text() {
        assert_eq!(tail_text(&[], LayoutId::Ru.table()), Ok(String::new()));
    }

    #[test]
    fn letters_are_retyped_in_the_target_layout() {
        let keys = typed_str("rfr");
        assert_eq!(tail_text(&keys, LayoutId::Ru.table()), Ok("как".to_owned()));
        assert_eq!(tail_text(&keys, LayoutId::Us.table()), Ok("rfr".to_owned()));
    }

    #[test]
    fn spaces_and_punctuation_letters_survive() {
        // Physical keys are replayed: `,` is `б` and `;` is `ж` in ru.
        let keys = typed_str("rfr ltkf, ;bpym");
        assert_eq!(
            tail_text(&keys, LayoutId::Ru.table()),
            Ok("как делаб жизнь".to_owned())
        );
    }

    #[test]
    fn shift_and_caps_lock_are_transparent() {
        let shift = 50; // evdev 42 + 8
        let caps = 66; // evdev 58 + 8
        let mut keys = vec![key(shift, 0, true)];
        keys.extend(typed('R'));
        keys.push(key(shift, SHIFT_MASK, false));
        keys.push(key(caps, 0, true));
        keys.push(key(caps, LOCK_MASK, false));
        let r = LayoutId::Us.table().keystroke_for('r').unwrap().keycode as u8;
        keys.push(key(r, LOCK_MASK, true));
        assert_eq!(tail_text(&keys, LayoutId::Ru.table()), Ok("КК".to_owned()));
    }

    #[test]
    fn editing_keys_block_the_tail() {
        let mut keys = typed_str("rf");
        keys.push(key(BACKSPACE_KEYCODE, 0, true));
        assert_eq!(
            tail_text(&keys, LayoutId::Ru.table()),
            Err(Blocked::NonText(BACKSPACE_KEYCODE))
        );
        let enter = [key(RETURN_KEYCODE, 0, true)];
        assert_eq!(
            tail_text(&enter, LayoutId::Ru.table()),
            Err(Blocked::NonText(RETURN_KEYCODE))
        );
    }

    #[test]
    fn shortcuts_block_the_tail() {
        let ctrl = 37; // evdev 29 + 8
        assert_eq!(
            tail_text(&[key(ctrl, 0, true)], LayoutId::Ru.table()),
            Err(Blocked::Shortcut)
        );
        let s = LayoutId::Us.table().keystroke_for('s').unwrap().keycode as u8;
        assert_eq!(
            tail_text(&[key(s, CONTROL_MASK, true)], LayoutId::Ru.table()),
            Err(Blocked::Shortcut)
        );
    }
}
