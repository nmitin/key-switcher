//! Global X11 hotkey grabs (XGrabKey).
//!
//! The daemon still matches hotkeys in the capture stream, but an active grab
//! prevents the focused application from also receiving the key. Events are
//! read from the grabbing connection so its queue does not grow.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, EventMask, GrabMode, ModMask, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::error::X11Error;

/// A key event delivered to the grabbing connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyEvent {
    /// X11 keycode.
    pub keycode: u8,
    /// Modifier state.
    pub state: u16,
}

/// Owns XGrabKey grabs for the daemon's hotkeys.
#[derive(Debug)]
pub struct X11HotkeyGrabber {
    connection: RustConnection,
    root: Window,
    grabs: Vec<(u8, ModMask)>,
}

impl X11HotkeyGrabber {
    /// Connect and select key events on the root window.
    pub fn new() -> Result<Self, X11Error> {
        let (connection, screen) = RustConnection::connect(None)?;
        let root = connection.setup().roots[screen].root;
        let attributes = ChangeWindowAttributesAux::new()
            .event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE);
        connection
            .change_window_attributes(root, &attributes)?
            .check()?;
        Ok(Self {
            connection,
            root,
            grabs: Vec::new(),
        })
    }

    /// Grab a key combination, covering Caps Lock and Num Lock variants.
    ///
    /// If one variant is already grabbed by another client (`BadAccess`), all
    /// variants added by this call are released and an error is returned. The
    /// daemon can then fall back to passive matching for that hotkey.
    pub fn grab(&mut self, keycode: u16, modifiers: u16) -> Result<(), X11Error> {
        let keycode = u8::try_from(keycode).map_err(|_| X11Error::KeycodeOutOfRange(keycode))?;
        let base = ModMask::from(modifiers);
        let lock = ModMask::from(u16::from(ModMask::LOCK));
        let num_lock = ModMask::from(u16::from(ModMask::M2));

        let mut added = Vec::new();
        for extra in [ModMask::default(), lock, num_lock, lock | num_lock] {
            let mask = base | extra;
            let result = self
                .connection
                .grab_key(
                    false,
                    self.root,
                    mask,
                    keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?
                .check();
            match result {
                Ok(()) => added.push((keycode, mask)),
                Err(error) => {
                    for (key, mask) in added {
                        let _ = self.connection.ungrab_key(key, self.root, mask);
                    }
                    let _ = self.connection.flush();
                    return Err(error.into());
                }
            }
        }
        self.grabs.extend(added);
        self.connection.flush()?;
        Ok(())
    }

    /// Drain pending grabbed key events.
    pub fn poll_events(&self) -> Result<Vec<HotkeyEvent>, X11Error> {
        let mut events = Vec::new();
        while let Some(event) = self.connection.poll_for_event()? {
            if let Event::KeyPress(key) = event {
                events.push(HotkeyEvent {
                    keycode: key.detail,
                    state: u16::from(key.state),
                });
            }
        }
        Ok(events)
    }

    /// Release all grabs owned by this client.
    pub fn ungrab_all(&mut self) {
        for (keycode, mask) in self.grabs.drain(..) {
            let _ = self.connection.ungrab_key(keycode, self.root, mask);
        }
        let _ = self.connection.flush();
    }
}

impl Drop for X11HotkeyGrabber {
    fn drop(&mut self) {
        self.ungrab_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a reachable X11 server"]
    fn grabs_pause_and_receives_the_key() {
        let mut grabber = X11HotkeyGrabber::new().expect("grabber");
        grabber.grab(127, 0).expect("grab Pause");

        let mut injector = crate::X11Injector::new().expect("XTest");
        injector.tap_keycode(127, false).expect("inject Pause");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            let events = grabber.poll_events().expect("poll");
            if events.iter().any(|event| event.keycode == 127) {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        grabber.ungrab_all();
        assert!(seen, "grabbed key was not delivered to the grabber");
    }
}
