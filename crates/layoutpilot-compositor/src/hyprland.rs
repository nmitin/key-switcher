//! Hyprland layout provider over the compositor IPC sockets.
//!
//! `$XDG_RUNTIME_DIR/hypr/<signature>/.socket.sock` serves requests, and
//! `.socket2.sock` carries events. We only need the request socket here:
//! `j/devices`, `j/getoption input:kb_layout` and `switchxkblayout all N`.
//!
//! Hyprland keeps a layout per keyboard; our virtual keyboard is ignored so we
//! follow the real one, preferring the keyboard marked `main`.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use layoutpilot_core::LayoutId;
use serde_json::Value;

use crate::layout_id_from_name;

/// Hyprland IPC error.
#[derive(Debug)]
pub enum HyprlandError {
    /// Socket I/O failed.
    Io(io::Error),
    /// No Hyprland instance socket was found.
    NoSocket,
    /// The reply is not valid JSON or has an unexpected shape.
    Protocol(&'static str),
    /// JSON reply could not be parsed.
    Json(serde_json::Error),
    /// No keyboard with a usable layout was found.
    NoKeyboard,
    /// The active layout index is outside the layout list.
    InvalidIndex(u64),
    /// The requested layout is not configured in Hyprland.
    LayoutNotFound(LayoutId),
    /// Hyprland rejected the switch command.
    Rejected(String),
}

impl std::fmt::Display for HyprlandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Hyprland IPC I/O error: {error}"),
            Self::NoSocket => f.write_str("no Hyprland IPC socket"),
            Self::Protocol(message) => write!(f, "Hyprland IPC protocol error: {message}"),
            Self::Json(error) => write!(f, "Hyprland IPC JSON error: {error}"),
            Self::NoKeyboard => f.write_str("Hyprland has no keyboard with a layout"),
            Self::InvalidIndex(index) => {
                write!(f, "Hyprland reported layout index {index} out of range")
            }
            Self::LayoutNotFound(layout) => {
                write!(f, "layout {layout} is not configured in Hyprland")
            }
            Self::Rejected(message) => write!(f, "Hyprland rejected the switch: {message}"),
        }
    }
}

impl std::error::Error for HyprlandError {}

impl From<io::Error> for HyprlandError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for HyprlandError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn valid_signature(signature: &str) -> bool {
    !signature.is_empty()
        && signature
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Find the socket directory of the current Hyprland instance.
#[must_use]
pub fn find_dir() -> Option<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    if let Some(signature) = env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        if let Some(signature) = signature.to_str() {
            if valid_signature(signature) {
                let candidate = runtime.join("hypr").join(signature);
                if candidate.join(".socket.sock").exists() {
                    return Some(candidate);
                }
            }
        }
    }
    let mut dirs: Vec<(SystemTime, PathBuf)> = fs::read_dir(runtime.join("hypr"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.join(".socket.sock").exists() {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    dirs.sort_by_key(|(modified, _)| *modified);
    dirs.pop().map(|(_, path)| path)
}

/// One request/response exchange with Hyprland.
#[derive(Debug, Clone)]
pub struct HyprIpc {
    directory: PathBuf,
}

impl HyprIpc {
    /// Create a client for an instance directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// Send a raw command and return the raw reply.
    pub fn request(&self, command: &str) -> Result<String, HyprlandError> {
        let socket = self.directory.join(".socket.sock");
        let mut stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(command.as_bytes())?;
        stream.flush()?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply)?;
        Ok(reply)
    }

    /// Request a JSON reply (`j/...`).
    pub fn json(&self, command: &str) -> Result<Value, HyprlandError> {
        let reply = self.request(&format!("j/{command}"))?;
        Ok(serde_json::from_str(&reply)?)
    }
}

/// Hyprland layout provider.
#[derive(Debug)]
pub struct HyprlandLayouts {
    ipc: HyprIpc,
    layouts: Vec<Option<LayoutId>>,
}

impl HyprlandLayouts {
    /// Connect to the running Hyprland and read its layout list.
    pub fn new() -> Result<Self, HyprlandError> {
        let directory = find_dir().ok_or(HyprlandError::NoSocket)?;
        Self::with_ipc(HyprIpc::new(directory))
    }

    /// Use an existing IPC client (also useful in tests).
    pub fn with_ipc(ipc: HyprIpc) -> Result<Self, HyprlandError> {
        let layouts = read_layout_list(&ipc)?;
        if layouts.len() < 2 {
            return Err(HyprlandError::Protocol("fewer than two layouts"));
        }
        Ok(Self { ipc, layouts })
    }

    /// Layouts in Hyprland's index order; unsupported entries are `None`.
    #[must_use]
    pub fn layouts(&self) -> &[Option<LayoutId>] {
        &self.layouts
    }

    /// Current layout, if it is one of the supported pair.
    pub fn current(&self) -> Result<Option<LayoutId>, HyprlandError> {
        let devices = self.ipc.json("devices")?;
        let keyboard = main_keyboard(&devices).ok_or(HyprlandError::NoKeyboard)?;
        if let Some(index) = keyboard.get("active_layout_index").and_then(Value::as_u64) {
            let id = self
                .layouts
                .get(index as usize)
                .copied()
                .ok_or(HyprlandError::InvalidIndex(index))?;
            return Ok(id);
        }
        if let Some(description) = keyboard.get("active_keymap").and_then(Value::as_str) {
            return Ok(layout_id_from_name(description));
        }
        Err(HyprlandError::Protocol("keyboard has no active layout"))
    }

    /// Whether Hyprland has this layout.
    #[must_use]
    pub fn supports(&self, layout: LayoutId) -> bool {
        self.layouts.contains(&Some(layout))
    }

    /// Switch every keyboard to the layout at `index`.
    pub fn switch_to(&mut self, layout: LayoutId) -> Result<(), HyprlandError> {
        let index = self
            .layouts
            .iter()
            .position(|entry| *entry == Some(layout))
            .ok_or(HyprlandError::LayoutNotFound(layout))?;
        let reply = self.ipc.request(&format!("switchxkblayout all {index}"))?;
        if reply.trim() == "ok" {
            return Ok(());
        }
        // Hyprland may complain about keyboards that have only one layout while
        // still switching the rest; accept the command if our keyboard moved.
        if self.current()? == Some(layout) {
            return Ok(());
        }
        Err(HyprlandError::Rejected(reply.trim().to_owned()))
    }
}

fn read_layout_list(ipc: &HyprIpc) -> Result<Vec<Option<LayoutId>>, HyprlandError> {
    let option = ipc.json("getoption input:kb_layout")?;
    let value = option
        .get("str")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && *value != "[[EMPTY]]");
    let value = match value {
        Some(value) => value.to_owned(),
        None => {
            let devices = ipc.json("devices")?;
            main_keyboard(&devices)
                .and_then(|keyboard| keyboard.get("layout").and_then(Value::as_str))
                .ok_or(HyprlandError::Protocol("no input:kb_layout"))?
                .to_owned()
        }
    };
    Ok(parse_layout_list(&value))
}

/// Parse a comma-separated layout list (`us,ru`) into supported ids.
#[must_use]
pub fn parse_layout_list(value: &str) -> Vec<Option<LayoutId>> {
    value
        .split(',')
        .map(|name| layout_id_from_name(name.trim()))
        .collect()
}

fn main_keyboard(devices: &Value) -> Option<&Value> {
    let keyboards = devices.get("keyboards")?.as_array()?;
    let real = keyboards.iter().filter(|keyboard| {
        !keyboard
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| {
                name.eq_ignore_ascii_case(layoutpilot_core::VIRTUAL_KEYBOARD_NAME)
                    || name.eq_ignore_ascii_case("layoutpilot-virtual-keyboard")
            })
    });
    real.clone()
        .find(|keyboard| keyboard.get("main").and_then(Value::as_bool) == Some(true))
        .or_else(|| {
            real.into_iter().find(|keyboard| {
                keyboard
                    .get("layout")
                    .and_then(Value::as_str)
                    .is_some_and(|layout| layout.contains(','))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("layoutpilot-hypr-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn parses_layout_list() {
        assert_eq!(
            parse_layout_list("us,ru"),
            vec![Some(LayoutId::Us), Some(LayoutId::Ru)]
        );
        assert_eq!(
            parse_layout_list("English (US), Russian"),
            vec![Some(LayoutId::Us), Some(LayoutId::Ru)]
        );
        assert_eq!(parse_layout_list("de,ru"), vec![None, Some(LayoutId::Ru)]);
    }

    #[test]
    fn picks_main_real_keyboard() {
        let devices = json!({
            "keyboards": [
                {"name": layoutpilot_core::VIRTUAL_KEYBOARD_NAME, "main": true, "layout": "us,ru"},
                {"name": "at-translated-set-2-keyboard", "layout": "us,ru", "active_layout_index": 1},
                {"name": "extra", "main": true, "layout": "us,ru", "active_layout_index": 0}
            ]
        });
        let keyboard = main_keyboard(&devices).expect("keyboard");
        assert_eq!(keyboard["name"], "extra");
        assert_eq!(keyboard["active_layout_index"], 0);
    }

    #[test]
    fn ipc_json_round_trip() {
        let directory = temp_dir();
        let path = directory.join(".socket.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0_u8; 64];
            let read = stream.read(&mut buffer).expect("read");
            assert_eq!(&buffer[..read], b"j/devices");
            stream.write_all(br#"{"keyboards":[]}"#).expect("write");
        });
        let ipc = HyprIpc::new(&directory);
        let reply = ipc.json("devices").expect("json");
        assert_eq!(reply["keyboards"], json!([]));
        server.join().expect("server");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn rejects_bad_signature() {
        assert!(valid_signature("abc_DEF-123"));
        assert!(!valid_signature("../escape"));
        assert!(!valid_signature(""));
    }
}
