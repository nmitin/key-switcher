//! Global hotkey parsing and matching.
//!
//! Hotkeys are matched against X11/evdev modifier state bits. Caps Lock and
//! Num Lock are ignored. Suppressing the key from the focused application
//! (XGrabKey) is a separate platform feature; the daemon currently matches the
//! key in the capture stream.

use std::fmt;

use layoutpilot_core::LayoutId;

/// Shift modifier bit (X11 `KeyButMask::SHIFT`).
pub const MOD_SHIFT: u16 = 1;
/// Caps Lock bit; ignored by matching.
pub const MOD_LOCK: u16 = 2;
/// Control bit.
pub const MOD_CONTROL: u16 = 4;
/// Alt/Mod1 bit.
pub const MOD_ALT: u16 = 8;
/// Super/Mod4 bit.
pub const MOD_SUPER: u16 = 64;

const MOD_MASK: u16 = MOD_SHIFT | MOD_CONTROL | MOD_ALT | MOD_SUPER;

/// A parsed global hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    /// XKB keycode (physical key, independent of the active layout).
    pub keycode: u16,
    /// Control is required.
    pub ctrl: bool,
    /// Alt is required.
    pub alt: bool,
    /// Shift is required.
    pub shift: bool,
    /// Super is required.
    pub super_key: bool,
}

impl Hotkey {
    /// Parse strings such as `"Pause"`, `"Ctrl+Shift+F"`, `"Alt+F12"`.
    ///
    /// Modifier names are case-insensitive: `Ctrl`, `Control`, `Alt`, `Shift`,
    /// `Super`, `Meta`. The final component is the key: a single printable
    /// character, `Pause`, `Escape`, `Space`, `Tab`, `Enter`, `Backspace`, or
    /// `F1`..`F12`.
    pub fn parse(input: &str) -> Result<Self, HotkeyError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(HotkeyError::Empty);
        }

        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut super_key = false;
        let mut key = None;

        for part in trimmed.split('+') {
            let part = part.trim();
            if part.is_empty() {
                return Err(HotkeyError::EmptyPart(input.to_owned()));
            }
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                "super" | "meta" => super_key = true,
                _ => {
                    if key.is_some() {
                        return Err(HotkeyError::MultipleKeys(input.to_owned()));
                    }
                    key = Some(part);
                }
            }
        }

        let key = key.ok_or_else(|| HotkeyError::NoKey(input.to_owned()))?;
        let keycode = keycode_for(key).ok_or_else(|| HotkeyError::UnknownKey(key.to_owned()))?;
        Ok(Self {
            keycode,
            ctrl,
            alt,
            shift,
            super_key,
        })
    }

    /// Whether the keycode and modifier state match this hotkey.
    ///
    /// Caps Lock and Num Lock are ignored; all other extra modifiers prevent a
    /// match.
    #[must_use]
    pub fn matches(&self, keycode: u16, state: u16) -> bool {
        if keycode != self.keycode {
            return false;
        }
        let expected = (u16::from(self.ctrl) * MOD_CONTROL)
            | (u16::from(self.alt) * MOD_ALT)
            | (u16::from(self.shift) * MOD_SHIFT)
            | (u16::from(self.super_key) * MOD_SUPER);
        (state & MOD_MASK) == expected
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.super_key {
            parts.push("Super");
        }
        write!(f, "{}+<{}>", parts.join("+"), self.keycode)
    }
}

fn keycode_for(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "pause" | "break" => Some(127),
        "escape" | "esc" => Some(9),
        "space" => Some(65),
        "tab" => Some(23),
        "enter" | "return" => Some(36),
        "backspace" => Some(22),
        "f1" => Some(67),
        "f2" => Some(68),
        "f3" => Some(69),
        "f4" => Some(70),
        "f5" => Some(71),
        "f6" => Some(72),
        "f7" => Some(73),
        "f8" => Some(74),
        "f9" => Some(75),
        "f10" => Some(76),
        "f11" => Some(95),
        "f12" => Some(96),
        _ => None,
    };
    if named.is_some() {
        return named;
    }

    let mut chars = name.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // Use the US keycodes: hotkeys are physical, not layout-dependent.
    LayoutId::Us
        .table()
        .keystroke_for(ch)
        .map(|key| key.keycode)
}

/// Hotkey parsing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// Empty string.
    Empty,
    /// Empty component, e.g. `Ctrl++F`.
    EmptyPart(String),
    /// More than one key component.
    MultipleKeys(String),
    /// No key component.
    NoKey(String),
    /// Unknown key name.
    UnknownKey(String),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("hotkey is empty"),
            Self::EmptyPart(hotkey) => write!(f, "empty component in hotkey {hotkey:?}"),
            Self::MultipleKeys(hotkey) => write!(f, "multiple keys in hotkey {hotkey:?}"),
            Self::NoKey(hotkey) => write!(f, "no key in hotkey {hotkey:?}"),
            Self::UnknownKey(key) => write!(f, "unknown hotkey key {key:?}"),
        }
    }
}

impl std::error::Error for HotkeyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_letter_combinations() {
        let hotkey = Hotkey::parse("Ctrl+Shift+F").unwrap();
        assert_eq!(hotkey.keycode, 41);
        assert!(hotkey.ctrl);
        assert!(hotkey.shift);
        assert!(!hotkey.alt);

        assert!(hotkey.matches(41, MOD_CONTROL | MOD_SHIFT));
        assert!(hotkey.matches(41, MOD_CONTROL | MOD_SHIFT | MOD_LOCK));
        assert!(!hotkey.matches(41, MOD_CONTROL));
        assert!(!hotkey.matches(41, MOD_CONTROL | MOD_SHIFT | MOD_ALT));
        assert!(!hotkey.matches(42, MOD_CONTROL | MOD_SHIFT));
    }

    #[test]
    fn parses_special_keys_and_function_keys() {
        assert_eq!(Hotkey::parse("Pause").unwrap().keycode, 127);
        assert_eq!(Hotkey::parse("Alt+F12").unwrap().keycode, 96);
        assert_eq!(Hotkey::parse("Super+Escape").unwrap().keycode, 9);
        assert!(Hotkey::parse("Ctrl+Space").unwrap().ctrl);
        assert!(Hotkey::parse("Meta+Tab").unwrap().super_key);
    }

    #[test]
    fn rejects_invalid_hotkeys() {
        assert_eq!(Hotkey::parse(""), Err(HotkeyError::Empty));
        assert!(Hotkey::parse("Ctrl+").is_err());
        assert!(Hotkey::parse("F+N").is_err());
        assert!(Hotkey::parse("Ctrl+NoSuchKey").is_err());
        assert!(Hotkey::parse("Ctrl+Shift").is_err());
    }
}
