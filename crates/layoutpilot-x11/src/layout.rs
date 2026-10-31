//! XKB layout state and switching for X11.

use layoutpilot_core::LayoutId;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ModMask;
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
