//! XKB layout state and switching for X11.

use layoutpilot_core::LayoutId;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ModMask};
use x11rb::rust_connection::RustConnection;

use crate::error::X11Error;

/// XKB device spec for the core keyboard.
const CORE_KEYBOARD: u16 = 0x0100;

/// Reads and changes the active XKB group.
#[derive(Debug)]
pub struct X11LayoutState {
    connection: RustConnection,
    layouts: Vec<LayoutId>,
}

impl X11LayoutState {
    /// Connect to X11 and configure the group order.
    ///
    /// The order must match `XkbGroup1`, `XkbGroup2`, ... for the active keymap.
    pub fn new(layouts: Vec<LayoutId>) -> Result<Self, X11Error> {
        if layouts.len() > 4 {
            return Err(X11Error::TooManyGroups(layouts.len()));
        }
        let (connection, _) = RustConnection::connect(None)?;
        if connection
            .extension_information(xkb::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err(X11Error::MissingExtension("XKEYBOARD"));
        }
        connection.xkb_use_extension(1, 0)?.reply()?;
        Ok(Self {
            connection,
            layouts,
        })
    }

    /// Current XKB group index (0-based).
    pub fn group(&self) -> Result<u8, X11Error> {
        let reply = self.connection.xkb_get_state(CORE_KEYBOARD)?.reply()?;
        Ok(reply.group.into())
    }

    /// Layout corresponding to the current group, if known.
    pub fn active_layout(&self) -> Result<Option<LayoutId>, X11Error> {
        let group = usize::from(self.group()?);
        Ok(self.layouts.get(group).copied())
    }

    /// Switch to a configured layout by locking its XKB group.
    pub fn switch_to(&self, layout: LayoutId) -> Result<(), X11Error> {
        let index = self
            .layouts
            .iter()
            .position(|candidate| *candidate == layout)
            .ok_or_else(|| X11Error::UnknownLayout(layout.to_string()))?;
        let group = match index {
            0 => xkb::Group::M1,
            1 => xkb::Group::M2,
            2 => xkb::Group::M3,
            3 => xkb::Group::M4,
            _ => return Err(X11Error::TooManyGroups(self.layouts.len())),
        };
        self.connection
            .xkb_latch_lock_state(
                CORE_KEYBOARD,
                ModMask::default(),
                ModMask::default(),
                true,
                group,
                ModMask::default(),
                false,
                0,
            )?
            .check()?;
        self.connection.flush()?;
        Ok(())
    }

    /// Switch to a raw group index.
    pub fn switch_group(&self, group: u8) -> Result<(), X11Error> {
        let layout = self
            .layouts
            .get(usize::from(group))
            .copied()
            .ok_or_else(|| X11Error::UnknownLayout(group.to_string()))?;
        self.switch_to(layout)
    }

    /// Configured group order.
    #[must_use]
    pub fn layouts(&self) -> &[LayoutId] {
        &self.layouts
    }

    /// Replace the group order.
    pub fn set_layouts(&mut self, layouts: Vec<LayoutId>) {
        self.layouts = layouts;
    }

    /// Group order of the active keymap, read from the root window property
    /// `_XKB_RULES_NAMES` (what `setxkbmap -query` prints as `layout`).
    ///
    /// Returns `None` when the property is missing or a group is a layout
    /// without built-in tables; the configured order is kept then.
    pub fn detect_order(&self) -> Option<Vec<LayoutId>> {
        let atom = self
            .connection
            .intern_atom(true, b"_XKB_RULES_NAMES")
            .ok()?
            .reply()
            .ok()?
            .atom;
        if atom == 0 {
            return None;
        }
        let root = self.connection.setup().roots.first()?.root;
        let reply = self
            .connection
            .get_property(false, root, atom, AtomEnum::STRING, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        order_from_rules_names(&reply.value)
    }
}

/// Parse `_XKB_RULES_NAMES` (`rules\0model\0layouts\0variants\0options`)
/// into a group order of known layouts.
fn order_from_rules_names(value: &[u8]) -> Option<Vec<LayoutId>> {
    let field = value.split(|byte| *byte == 0).nth(2)?;
    let field = std::str::from_utf8(field).ok()?;
    let order = field
        .split(',')
        .map(|code| LayoutId::from_code(code.trim()))
        .collect::<Option<Vec<_>>>()?;
    let usable = (2..=4).contains(&order.len())
        && order.contains(&LayoutId::Us)
        && order.contains(&LayoutId::Ru);
    usable.then_some(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_names_give_the_group_order() {
        let order = |value: &[u8]| order_from_rules_names(value);
        assert_eq!(
            order(b"evdev\0pc105\0us,ru\0,\0grp:alt_shift_toggle\0"),
            Some(vec![LayoutId::Us, LayoutId::Ru])
        );
        assert_eq!(
            order(b"evdev\0pc105\0ru,us\0\0\0"),
            Some(vec![LayoutId::Ru, LayoutId::Us])
        );
        // Unknown layout in a group, a single layout, or no layouts at all.
        assert_eq!(order(b"evdev\0pc105\0us,ua,ru\0\0\0"), None);
        assert_eq!(order(b"evdev\0pc105\0us\0\0\0"), None);
        assert_eq!(order(b""), None);
    }

    #[test]
    #[ignore = "requires a reachable X11 server with two keyboard groups"]
    fn reads_and_restores_the_group() {
        let state = X11LayoutState::new(vec![LayoutId::Us, LayoutId::Ru]).expect("X11");
        let original = state.group().expect("group");
        let other = if original == 0 { 1 } else { 0 };
        state.switch_group(other).expect("switch");
        assert_eq!(state.group().expect("group"), other);
        state.switch_group(original).expect("restore");
        assert_eq!(state.group().expect("group"), original);
    }

    /// Prints what the live server reports. XWayland under KDE lists only
    /// `us` (KWin owns the layouts), so `None` is expected there.
    #[test]
    #[ignore = "requires a reachable X11 server"]
    fn detects_the_live_group_order() {
        let state = X11LayoutState::new(vec![LayoutId::Us, LayoutId::Ru]).expect("X11");
        println!("detected order: {:?}", state.detect_order());
    }
}
