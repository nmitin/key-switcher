//! uinput injection backend.
//!
//! On Wayland the daemon cannot use XTest for native clients. This backend
//! creates a virtual keyboard through `/dev/uinput` and replays the physical
//! keycodes of the target layout, exactly like a real keyboard would.
//!
//! The device name is part of the contract: [`layoutpilot_core::VIRTUAL_KEYBOARD_NAME`]
//! is filtered out by evdev capture, otherwise every injected key would be
//! captured again and could trigger a correction loop.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::io;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use layoutpilot_core::{Keystroke, LayoutTable};

/// Name of the virtual keyboard. Also used by capture to ignore own events.
pub const DEVICE_NAME: &str = layoutpilot_core::VIRTUAL_KEYBOARD_NAME;

/// XKB keycode for Backspace (evdev 14 + 8).
pub const BACKSPACE_KEYCODE: u16 = 22;
/// XKB keycode for Return (evdev 28 + 8).
pub const RETURN_KEYCODE: u16 = 36;
/// XKB keycode for Tab (evdev 15 + 8).
pub const TAB_KEYCODE: u16 = 23;
/// XKB keycode for Space (evdev 57 + 8).
pub const SPACE_KEYCODE: u16 = 65;
/// XKB keycode for left Shift (evdev 42 + 8).
pub const SHIFT_KEYCODE: u16 = 50;

const EVDEV_KEY_LEFTSHIFT: u16 = 42;
const EVDEV_KEY_LEFTCTRL: u16 = 29;
const EVDEV_KEY_BACKSPACE: u16 = 14;
const EVDEV_KEY_ENTER: u16 = 28;
const EVDEV_KEY_TAB: u16 = 15;
const EVDEV_KEY_SPACE: u16 = 57;
const EVDEV_TO_XKB_OFFSET: u16 = 8;

/// uinput injection error.
#[derive(Debug)]
pub enum InjectError {
    /// `/dev/uinput` could not be opened or written.
    Io(io::Error),
    /// A character has no physical key in the target layout.
    UnsupportedChar(char),
    /// A layout keycode cannot be represented as an evdev keycode.
    KeycodeOutOfRange(u16),
}

impl std::fmt::Display for InjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "uinput I/O error: {error}"),
            Self::UnsupportedChar(ch) => write!(f, "character {ch:?} has no key in the layout"),
            Self::KeycodeOutOfRange(code) => write!(f, "keycode {code} is out of range"),
        }
    }
}

impl std::error::Error for InjectError {}

impl From<io::Error> for InjectError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A virtual keyboard backed by `/dev/uinput`.
#[derive(Debug)]
pub struct UinputInjector {
    device: VirtualDevice,
    pressed: Vec<u16>,
}

impl UinputInjector {
    /// Create the virtual keyboard.
    ///
    /// Requires write access to `/dev/uinput`; install the provided udev rule
    /// or grant the active session user access with `uaccess`.
    pub fn new() -> Result<Self, InjectError> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=127 {
            keys.insert(KeyCode::new(code));
        }
        let device = VirtualDevice::builder()?
            .name(DEVICE_NAME)
            .with_keys(&keys)?
            .build()?;
        Ok(Self {
            device,
            pressed: Vec::new(),
        })
    }

    /// Tap Backspace `count` times.
    pub fn tap_backspace(&mut self, count: usize) -> Result<(), InjectError> {
        for _ in 0..count {
            self.tap_key(EVDEV_KEY_BACKSPACE + EVDEV_TO_XKB_OFFSET, false)?;
        }
        Ok(())
    }

    /// Delete the previous word with one Ctrl+Backspace.
    pub fn tap_ctrl_backspace(&mut self) -> Result<(), InjectError> {
        self.press(EVDEV_KEY_LEFTCTRL)?;
        self.press(EVDEV_KEY_BACKSPACE)?;
        self.release(EVDEV_KEY_BACKSPACE)?;
        self.release(EVDEV_KEY_LEFTCTRL)?;
        Ok(())
    }

    /// Type `text` using the physical keys of `layout`.
    ///
    /// The whole string is validated before the first event, so an unsupported
    /// character cannot leave a half-typed replacement.
    pub fn tap_text(&mut self, text: &str, layout: &LayoutTable) -> Result<(), InjectError> {
        let keys = encode_text(text, layout)?;
        for (keycode, shift) in keys {
            self.tap_key(keycode, shift)?;
        }
        Ok(())
    }

    /// uinput does not need a compositor permission prompt.
    pub fn request_input_permission(&mut self) -> Result<(), InjectError> {
        Ok(())
    }

    fn tap_key(&mut self, keycode: u16, shift: bool) -> Result<(), InjectError> {
        let evdev = keycode
            .checked_sub(EVDEV_TO_XKB_OFFSET)
            .ok_or(InjectError::KeycodeOutOfRange(keycode))?;
        if shift {
            self.press(EVDEV_KEY_LEFTSHIFT)?;
        }
        self.press(evdev)?;
        self.release(evdev)?;
        if shift {
            self.release(EVDEV_KEY_LEFTSHIFT)?;
        }
        Ok(())
    }

    fn press(&mut self, evdev: u16) -> Result<(), InjectError> {
        self.emit(evdev, 1)?;
        self.pressed.push(evdev);
        Ok(())
    }

    fn release(&mut self, evdev: u16) -> Result<(), InjectError> {
        self.emit(evdev, 0)?;
        if let Some(index) = self.pressed.iter().rposition(|&code| code == evdev) {
            self.pressed.remove(index);
        }
        Ok(())
    }

    fn emit(&mut self, evdev: u16, value: i32) -> Result<(), InjectError> {
        // One event per emit so press and release are separate frames. Some
        // compositors drop the last event of a tightly batched series; the
        // extra synchronization is cheap compared with a lost character.
        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, evdev, value)])?;
        Ok(())
    }

    fn release_all(&mut self) {
        for evdev in self.pressed.clone().into_iter().rev() {
            let _ = self.emit(evdev, 0);
        }
        self.pressed.clear();
    }
}

impl Drop for UinputInjector {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Translate text into XKB keycodes plus Shift flags before emitting anything.
pub fn encode_text(text: &str, layout: &LayoutTable) -> Result<Vec<(u16, bool)>, InjectError> {
    let mut keys = Vec::with_capacity(text.chars().count());
    for ch in text.chars() {
        match ch {
            '\n' => keys.push((EVDEV_KEY_ENTER + EVDEV_TO_XKB_OFFSET, false)),
            '\t' => keys.push((EVDEV_KEY_TAB + EVDEV_TO_XKB_OFFSET, false)),
            ' ' => keys.push((EVDEV_KEY_SPACE + EVDEV_TO_XKB_OFFSET, false)),
            _ => {
                let Keystroke { keycode, level } = layout
                    .keystroke_for(ch)
                    .ok_or(InjectError::UnsupportedChar(ch))?;
                keys.push((keycode, level == 2));
            }
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use layoutpilot_core::LayoutId;

    fn table() -> LayoutTable {
        LayoutTable::from_entries(
            LayoutId::Us,
            &[(38, 1, 'a'), (38, 2, 'A'), (57, 1, ' '), (52, 1, '.')],
        )
    }

    #[test]
    fn encodes_text_and_controls() {
        let keys = encode_text("aA .\n\t", &table()).expect("encodable");
        assert_eq!(
            keys,
            vec![
                (38, false),
                (38, true),
                (SPACE_KEYCODE, false),
                (52, false),
                (RETURN_KEYCODE, false),
                (TAB_KEYCODE, false),
            ]
        );
    }

    #[test]
    fn rejects_unknown_char_before_emitting() {
        let error = encode_text("aб", &table()).expect_err("unknown char");
        assert!(matches!(error, InjectError::UnsupportedChar('б')));
    }

    #[test]
    fn evdev_offsets_match_x11_constants() {
        assert_eq!(BACKSPACE_KEYCODE - EVDEV_TO_XKB_OFFSET, EVDEV_KEY_BACKSPACE);
        assert_eq!(RETURN_KEYCODE - EVDEV_TO_XKB_OFFSET, EVDEV_KEY_ENTER);
        assert_eq!(TAB_KEYCODE - EVDEV_TO_XKB_OFFSET, EVDEV_KEY_TAB);
        assert_eq!(SHIFT_KEYCODE - EVDEV_TO_XKB_OFFSET, EVDEV_KEY_LEFTSHIFT);
    }

    #[test]
    fn device_name_is_shared_with_capture() {
        assert_eq!(DEVICE_NAME, layoutpilot_core::VIRTUAL_KEYBOARD_NAME);
    }
}
