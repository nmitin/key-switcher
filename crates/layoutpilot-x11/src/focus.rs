//! Active window, application id and title on X11.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

use crate::error::X11Error;

/// Focus snapshot for policy and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusInfo {
    /// Active window.
    pub window: Window,
    /// Best-effort `app_id` from `WM_CLASS`.
    pub app_id: Option<String>,
    /// Window title.
    pub title: Option<String>,
}

/// Queries the active X11 window.
#[derive(Debug)]
pub struct X11Focus {
    connection: RustConnection,
    root: Window,
    net_active_window: Atom,
    wm_class: Atom,
    net_wm_name: Atom,
    utf8_string: Atom,
}

impl X11Focus {
    /// Connect to `$DISPLAY` and intern the required atoms.
    pub fn new() -> Result<Self, X11Error> {
        let (connection, screen) = RustConnection::connect(None)?;
        let root = connection.setup().roots[screen].root;
        let net_active_window = intern(&connection, b"_NET_ACTIVE_WINDOW")?;
        let wm_class = intern(&connection, b"WM_CLASS")?;
        let net_wm_name = intern(&connection, b"_NET_WM_NAME")?;
        let utf8_string = intern(&connection, b"UTF8_STRING")?;
        Ok(Self {
            connection,
            root,
            net_active_window,
            wm_class,
            net_wm_name,
            utf8_string,
        })
    }

    /// Active window id, or the input focus as a fallback.
    pub fn active_window(&self) -> Result<Window, X11Error> {
        let reply = self
            .connection
            .get_property(
                false,
                self.root,
                self.net_active_window,
                AtomEnum::WINDOW,
                0,
                1,
            )?
            .reply()?;
        if let Some(window) = reply.value32().and_then(|mut values| values.next()) {
            if window != 0 {
                return Ok(window);
            }
        }

        let focus = self.connection.get_input_focus()?.reply()?.focus;
        // 0 is None, 1 is PointerRoot; neither is a real window.
        if focus <= 1 {
            Err(X11Error::NoActiveWindow)
        } else {
            Ok(focus)
        }
    }

    /// `WM_CLASS` class part for a window (the second NUL-separated field).
    pub fn app_id(&self, window: Window) -> Result<Option<String>, X11Error> {
        let reply = self
            .connection
            .get_property(false, window, self.wm_class, AtomEnum::STRING, 0, 1024)?
            .reply()?;
        let parts: Vec<&[u8]> = reply
            .value
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .collect();
        Ok(parts
            .get(1)
            .or_else(|| parts.first())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned()))
    }

    /// Window title from `_NET_WM_NAME`, falling back to `WM_NAME`.
    pub fn title(&self, window: Window) -> Result<Option<String>, X11Error> {
        let utf8 = self
            .connection
            .get_property(false, window, self.net_wm_name, self.utf8_string, 0, 1024)?
            .reply()?;
        if !utf8.value.is_empty() {
            return Ok(Some(String::from_utf8_lossy(&utf8.value).into_owned()));
        }

        let legacy = self
            .connection
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)?
            .reply()?;
        if legacy.value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(String::from_utf8_lossy(&legacy.value).into_owned()))
        }
    }

    /// Complete focus snapshot.
    pub fn snapshot(&self) -> Result<FocusInfo, X11Error> {
        let window = self.active_window()?;
        Ok(FocusInfo {
            window,
            app_id: self.app_id(window)?,
            title: self.title(window)?,
        })
    }
}

fn intern(connection: &RustConnection, name: &[u8]) -> Result<Atom, X11Error> {
    Ok(connection.intern_atom(false, name)?.reply()?.atom)
}
