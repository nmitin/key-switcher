//! Active-window class for KDE Wayland.
//!
//! KWin does not expose the active window over a plain D-Bus property. A small
//! KWin script is loaded at startup; it calls back into this process whenever
//! the active window changes. This restores per-app profiles such as
//! `*terminal* -> manual` on native Wayland, where X11 focus is a dummy window.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use zbus::blocking::{Connection, Proxy};
use zbus::interface;

const BUS_NAME: &str = "org.kde.LayoutPilot";
const OBJECT_PATH: &str = "/Focus";
const SCRIPT: &str = r#"function report() {
    var w = workspace.activeWindow;
    callDBus("org.kde.LayoutPilot", "/Focus", "org.kde.LayoutPilot.Focus", "Update",
             w ? (w.resourceClass || "") : "",
             w ? (w.caption || "") : "");
}
workspace.windowActivated.connect(report);
report();
"#;

struct FocusBridge {
    app: Arc<Mutex<Option<String>>>,
}

#[interface(name = "org.kde.LayoutPilot.Focus")]
impl FocusBridge {
    fn update(&self, resource_class: String, _caption: String) {
        let value = if resource_class.trim().is_empty() {
            None
        } else {
            Some(resource_class)
        };
        if let Ok(mut app) = self.app.lock() {
            *app = value;
        }
    }
}

/// KWin-backed active application lookup.
pub(crate) struct KdeFocus {
    _connection: Connection,
    app: Arc<Mutex<Option<String>>>,
}

impl KdeFocus {
    /// Own the callback bus name and load the KWin script.
    pub(crate) fn start() -> Result<Self, String> {
        let app = Arc::new(Mutex::new(None));
        let connection = zbus::blocking::connection::Builder::session()
            .map_err(|error| format!("session bus: {error}"))?
            .name(BUS_NAME)
            .map_err(|error| format!("own {BUS_NAME}: {error}"))?
            .serve_at(
                OBJECT_PATH,
                FocusBridge {
                    app: Arc::clone(&app),
                },
            )
            .map_err(|error| format!("serve {OBJECT_PATH}: {error}"))?
            .build()
            .map_err(|error| format!("build focus service: {error}"))?;

        let path = write_script()?;

        let proxy = Proxy::new(
            &connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )
        .map_err(|error| format!("KWin scripting proxy: {error}"))?;
        let script = path.to_string_lossy().to_string();
        let _id: i32 = proxy
            .call("loadScript", &(script,))
            .map_err(|error| format!("KWin loadScript: {error}"))?;
        let _: () = proxy
            .call("start", &())
            .map_err(|error| format!("KWin start: {error}"))?;

        Ok(Self {
            _connection: connection,
            app,
        })
    }

    /// Last active application class reported by KWin.
    pub(crate) fn app_id(&self) -> Option<String> {
        self.app.lock().ok().and_then(|app| app.clone())
    }
}

fn write_script() -> Result<PathBuf, String> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        let path = runtime.join("layoutpilot").join("focus.js");
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_ok() && fs::write(&path, SCRIPT).is_ok() {
                return Ok(path);
            }
        }
    }
    // Sandboxes and minimal sessions may have a read-only runtime directory.
    let path = std::env::temp_dir().join(format!("layoutpilot-focus-{}.js", std::process::id()));
    fs::write(&path, SCRIPT).map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a KDE Wayland session with KWin scripting"]
    fn reports_active_window() {
        let focus = KdeFocus::start().expect("focus service");
        std::thread::sleep(std::time::Duration::from_millis(500));
        let app = focus.app_id();
        println!("active app: {app:?}");
        assert!(app.is_some(), "KWin did not report an active window");
    }
}
