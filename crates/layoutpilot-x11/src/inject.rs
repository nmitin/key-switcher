//! XTest injection backend.

use layoutpilot_core::{Keystroke, LayoutTable};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::error::X11Error;

/// Standard evdev keycode for Backspace.
pub const BACKSPACE_KEYCODE: u8 = 22;
/// Standard evdev keycode for Return.
pub const RETURN_KEYCODE: u8 = 36;
/// Standard evdev keycode for Tab.
pub const TAB_KEYCODE: u8 = 23;
/// Standard evdev keycode for Shift.
pub const SHIFT_KEYCODE: u8 = 50;

/// Injects key events through the XTEST extension.
#[derive(Debug)]
pub struct X11Injector {
    connection: RustConnection,
    root: Window,
    sent: Vec<(u8, bool)>,
}

impl X11Injector {
    /// Connect to `$DISPLAY` and check that XTEST is available.
    pub fn new() -> Result<Self, X11Error> {
        let (connection, screen) = RustConnection::connect(None)?;
        let root = connection.setup().roots[screen].root;

        if connection
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err(X11Error::MissingExtension("XTEST"));
        }
        connection.xtest_get_version(2, 2)?.reply()?;

        Ok(Self {
            connection,
            root,
            sent: Vec::new(),
        })
    }

    /// Tap one physical key, optionally with Shift.
    pub fn tap_keycode(&mut self, keycode: u8, shift: bool) -> Result<(), X11Error> {
        self.queue_keycode(keycode, shift)?;
        self.connection.flush()?;
        Ok(())
    }

    /// Tap a [`Keystroke`] from the layout table.
    pub fn tap_keystroke(&mut self, keystroke: Keystroke) -> Result<(), X11Error> {
        let keycode = u8::try_from(keystroke.keycode)
            .map_err(|_| X11Error::KeycodeOutOfRange(keystroke.keycode))?;
        self.tap_keycode(keycode, keystroke.needs_shift())
    }

    /// Type text using the physical keys of `layout`.
    ///
    /// The method batches requests and flushes once. If a character has no key
    /// in the layout, nothing is flushed and `UnsupportedChar` is returned.
    pub fn tap_text(&mut self, text: &str, layout: &LayoutTable) -> Result<(), X11Error> {
        // Validate and translate everything first so that an unsupported
        // character cannot leave a partially queued replacement.
        let mut keycodes = Vec::with_capacity(text.chars().count());
        for ch in text.chars() {
            if ch == '\n' {
                keycodes.push((RETURN_KEYCODE, false));
                continue;
            }
            if ch == '\t' {
                keycodes.push((TAB_KEYCODE, false));
                continue;
            }
            let keystroke = layout
                .keystroke_for(ch)
                .ok_or(X11Error::UnsupportedChar(ch))?;
            let keycode = u8::try_from(keystroke.keycode)
                .map_err(|_| X11Error::KeycodeOutOfRange(keystroke.keycode))?;
            keycodes.push((keycode, keystroke.needs_shift()));
        }
        for (keycode, shift) in keycodes {
            self.queue_keycode(keycode, shift)?;
        }
        self.connection.flush()?;
        Ok(())
    }

    /// Trigger the compositor's input-injection permission prompt.
    ///
    /// A single Shift press/release is used: it does not produce text or open a
    /// menu, but it is a real XTest key event, so KDE's XWayland permission
    /// gate can ask the user once at daemon startup instead of during typing.
    /// The event is recorded and then cleared so own-event suppression is not
    /// polluted.
    pub fn request_input_permission(&mut self) -> Result<(), X11Error> {
        self.tap_keycode(SHIFT_KEYCODE, false)?;
        self.clear_sent_events();
        Ok(())
    }

    /// Press Backspace `count` times.
    pub fn tap_backspace(&mut self, count: usize) -> Result<(), X11Error> {
        for _ in 0..count {
            self.queue_keycode(BACKSPACE_KEYCODE, false)?;
        }
        self.connection.flush()?;
        Ok(())
    }

    /// Send a key press.
    pub fn press_keycode(&mut self, keycode: u8) -> Result<(), X11Error> {
        self.fake(xproto::KEY_PRESS_EVENT, keycode)?;
        self.connection.flush()?;
        Ok(())
    }

    /// Send a key release.
    pub fn release_keycode(&mut self, keycode: u8) -> Result<(), X11Error> {
        self.fake(xproto::KEY_RELEASE_EVENT, keycode)?;
        self.connection.flush()?;
        Ok(())
    }

    fn queue_keycode(&mut self, keycode: u8, shift: bool) -> Result<(), X11Error> {
        if shift {
            self.fake(xproto::KEY_PRESS_EVENT, SHIFT_KEYCODE)?;
        }
        self.fake(xproto::KEY_PRESS_EVENT, keycode)?;
        self.fake(xproto::KEY_RELEASE_EVENT, keycode)?;
        if shift {
            self.fake(xproto::KEY_RELEASE_EVENT, SHIFT_KEYCODE)?;
        }
        Ok(())
    }

    /// Events sent since the last call, for own-event suppression (FR-09).
    #[must_use]
    pub fn sent_events(&self) -> &[(u8, bool)] {
        &self.sent
    }

    /// Drain the recorded events.
    pub fn take_sent_events(&mut self) -> Vec<(u8, bool)> {
        std::mem::take(&mut self.sent)
    }

    /// Forget recorded events without returning them.
    pub fn clear_sent_events(&mut self) {
        self.sent.clear();
    }

    fn fake(&mut self, event_type: u8, keycode: u8) -> Result<(), X11Error> {
        self.sent
            .push((keycode, event_type == xproto::KEY_PRESS_EVENT));
        self.connection.xtest_fake_input(
            event_type,
            keycode,
            x11rb::CURRENT_TIME,
            self.root,
            0,
            0,
            0,
        )?;
        Ok(())
    }
}
