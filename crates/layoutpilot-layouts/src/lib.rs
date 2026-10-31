//! XKB-derived layout tables and physical-key mappings.
//!
//! The tables are generated from the system XKB keymap `us,ru` by
//! `tools/gen_xkb_tables.py`; see `data/xkb/README.md`. Only letters are
//! stored: digits, punctuation and spaces are preserved by the conversion
//! layer (FR-06) and do not need a physical-key mapping.
//!
//! XKB keycodes are evdev keycodes plus [`XKB_TO_EVDEV_OFFSET`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod generated;

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;

/// XKB keycodes start at 8; evdev keycodes start at 0.
pub const XKB_TO_EVDEV_OFFSET: u16 = 8;

/// Supported keyboard layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LayoutId {
    /// English (US), XKB group 1.
    Us,
    /// Russian, XKB group 2.
    Ru,
}

impl LayoutId {
    /// All layouts supported by the built-in tables.
    pub const ALL: [Self; 2] = [Self::Us, Self::Ru];

    /// Stable lowercase identifier (`"us"`, `"ru"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Us => "us",
            Self::Ru => "ru",
        }
    }

    /// Parse a lowercase or uppercase identifier.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        if code.eq_ignore_ascii_case("us") {
            Some(Self::Us)
        } else if code.eq_ignore_ascii_case("ru") {
            Some(Self::Ru)
        } else {
            None
        }
    }

    /// The built-in table for this layout.
    #[must_use]
    pub fn table(self) -> &'static LayoutTable {
        match self {
            Self::Us => us(),
            Self::Ru => ru(),
        }
    }

    /// The other built-in layout of the ru/en pair.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Us => Self::Ru,
            Self::Ru => Self::Us,
        }
    }
}

impl fmt::Display for LayoutId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LayoutId {
    type Err = UnknownLayout;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_code(s).ok_or_else(|| UnknownLayout(s.to_owned()))
    }
}

/// Error returned when a layout code is not supported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownLayout(pub String);

impl fmt::Display for UnknownLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown layout {:?}; expected one of: us, ru", self.0)
    }
}

impl std::error::Error for UnknownLayout {}

/// A physical key press: XKB keycode plus Shift level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Keystroke {
    /// XKB keycode (evdev keycode + 8).
    pub keycode: u16,
    /// 1 — unshifted, 2 — Shift. Other levels are not used for ru/en letters.
    pub level: u8,
}

impl Keystroke {
    /// Construct a keystroke.
    #[must_use]
    pub const fn new(keycode: u16, level: u8) -> Self {
        Self { keycode, level }
    }

    /// Whether this level requires Shift.
    #[must_use]
    pub const fn needs_shift(self) -> bool {
        self.level == 2
    }

    /// Convert the XKB keycode to an evdev keycode.
    #[must_use]
    pub const fn evdev_keycode(self) -> Option<u16> {
        self.keycode.checked_sub(XKB_TO_EVDEV_OFFSET)
    }
}

impl fmt::Display for Keystroke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.needs_shift() {
            write!(f, "Shift+<{}>", self.keycode)
        } else {
            write!(f, "<{}>", self.keycode)
        }
    }
}

/// A bidirectional map between physical keystrokes and characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutTable {
    id: LayoutId,
    forward: BTreeMap<Keystroke, char>,
    reverse: BTreeMap<char, Keystroke>,
}

impl LayoutTable {
    /// Build a table from `(keycode, level, char)` entries.
    ///
    /// On conflicts the first entry wins; the generated tables are checked by
    /// their generator and contain no conflicts.
    #[must_use]
    pub fn from_entries(id: LayoutId, entries: &[(u16, u8, char)]) -> Self {
        let mut forward = BTreeMap::new();
        let mut reverse = BTreeMap::new();
        for &(keycode, level, ch) in entries {
            let keystroke = Keystroke::new(keycode, level);
            forward.entry(keystroke).or_insert(ch);
            reverse.entry(ch).or_insert(keystroke);
        }
        Self {
            id,
            forward,
            reverse,
        }
    }

    /// Layout identifier.
    #[must_use]
    pub const fn id(&self) -> LayoutId {
        self.id
    }

    /// Number of mapped characters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// The character produced by a physical keystroke.
    #[must_use]
    pub fn char_at(&self, keystroke: Keystroke) -> Option<char> {
        self.forward.get(&keystroke).copied()
    }

    /// The physical keystroke that produces a character.
    #[must_use]
    pub fn keystroke_for(&self, ch: char) -> Option<Keystroke> {
        self.reverse.get(&ch).copied()
    }

    /// Iterate over `(keystroke, character)` pairs.
    pub fn entries(&self) -> impl Iterator<Item = (Keystroke, char)> + '_ {
        self.forward.iter().map(|(&key, &ch)| (key, ch))
    }

    /// Strict keyboard reversibility: every character can be typed.
    ///
    /// The built-in tables contain letters only, so a string with punctuation
    /// is not strictly reversible. For word candidates use this method; for
    /// loose checks that ignore separators use [`Self::can_type_letters`].
    #[must_use]
    pub fn can_type(&self, text: &str) -> bool {
        text.chars().all(|ch| self.reverse.contains_key(&ch))
    }

    /// Loose reversibility: every alphabetic character can be typed.
    ///
    /// Non-alphabetic characters are preserved by conversion, so callers that
    /// only validate the letter part can use this method.
    #[must_use]
    pub fn can_type_letters(&self, text: &str) -> bool {
        text.chars()
            .all(|ch| !ch.is_alphabetic() || self.reverse.contains_key(&ch))
    }

    /// Alphabetic characters of `text` that this layout cannot type.
    #[must_use]
    pub fn untypeable_letters(&self, text: &str) -> Vec<char> {
        let mut missing: Vec<char> = text
            .chars()
            .filter(|ch| ch.is_alphabetic() && !self.reverse.contains_key(ch))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        missing
    }

    /// Characters of `text` that this layout cannot type at all.
    ///
    /// The built-in tables contain only letters, so punctuation is reported
    /// here even if the real layout can type it. This strict view is used for
    /// word candidates, where punctuation is not expected.
    #[must_use]
    pub fn untypeable(&self, text: &str) -> Vec<char> {
        let mut missing: Vec<char> = text
            .chars()
            .filter(|ch| !self.reverse.contains_key(ch))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        missing
    }
}

static US: OnceLock<LayoutTable> = OnceLock::new();
static RU: OnceLock<LayoutTable> = OnceLock::new();

/// The English (US) table.
#[must_use]
pub fn us() -> &'static LayoutTable {
    US.get_or_init(|| LayoutTable::from_entries(LayoutId::Us, generated::US))
}

/// The Russian table.
#[must_use]
pub fn ru() -> &'static LayoutTable {
    RU.get_or_init(|| LayoutTable::from_entries(LayoutId::Ru, generated::RU))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_not_empty() {
        assert_eq!(us().len(), 95);
        assert_eq!(ru().len(), 94);
        assert!(!us().is_empty());
    }

    #[test]
    fn shared_letters_live_on_the_same_physical_keys() {
        for (latin, cyrillic) in [('q', 'й'), ('w', 'ц'), ('e', 'у'), ('r', 'к')] {
            let latin_key = us().keystroke_for(latin).unwrap();
            let cyrillic_key = ru().keystroke_for(cyrillic).unwrap();
            assert_eq!(latin_key, cyrillic_key, "{latin} and {cyrillic}");
            assert_eq!(us().char_at(cyrillic_key), Some(latin));
            assert_eq!(ru().char_at(latin_key), Some(cyrillic));
        }
    }

    #[test]
    fn case_uses_shift_level() {
        let lower = us().keystroke_for('q').unwrap();
        let upper = us().keystroke_for('Q').unwrap();
        assert_eq!(lower.keycode, upper.keycode);
        assert_eq!(lower.level, 1);
        assert_eq!(upper.level, 2);
        assert!(upper.needs_shift());
        assert!(!lower.needs_shift());
    }

    #[test]
    fn xkb_keycode_is_evdev_plus_eight() {
        let key = Keystroke::new(24, 1);
        assert_eq!(key.evdev_keycode(), Some(16));
        assert_eq!(Keystroke::new(7, 1).evdev_keycode(), None);
    }

    #[test]
    fn strict_and_loose_reversibility_differ_on_punctuation() {
        assert!(us().can_type("hello!"));
        assert!(us().can_type_letters("hello🙂"));
        assert!(!us().can_type("hello🙂"));
        assert_eq!(us().untypeable_letters("hello🙂"), Vec::<char>::new());
        assert_eq!(us().untypeable("hello🙂"), vec!['🙂']);
        assert_eq!(
            us().untypeable_letters("привет"),
            vec!['в', 'е', 'и', 'п', 'р', 'т']
        );
    }

    #[test]
    fn layout_id_parses_case_insensitively() {
        assert_eq!(LayoutId::from_code("US"), Some(LayoutId::Us));
        assert_eq!("ru".parse(), Ok(LayoutId::Ru));
        assert!("de".parse::<LayoutId>().is_err());
        assert_eq!(LayoutId::Us.to_string(), "us");
        assert_eq!(LayoutId::Us.other(), LayoutId::Ru);
        assert_eq!(LayoutId::Ru.other(), LayoutId::Us);
    }
}
