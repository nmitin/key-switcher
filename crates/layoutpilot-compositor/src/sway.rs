//! Sway layout provider over the i3-ipc protocol.
//!
//! Sway keeps an XKB group per keyboard. We track one real keyboard (the first
//! one with at least two layouts), ignore our own virtual device, and switch
//! it with `input type:keyboard xkb_switch_layout N`.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use layoutpilot_core::LayoutId;
use serde_json::Value;

use crate::layout_id_from_name;

const MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_SIZE: usize = 14;
const RUN_COMMAND: u32 = 0;
const GET_INPUTS: u32 = 100;

/// Sway IPC error.
#[derive(Debug)]
pub enum SwayError {
    /// Socket I/O failed.
    Io(io::Error),
    /// No live `sway-ipc.*.sock` was found.
    NoSocket,
    /// The reply does not follow the i3-ipc protocol.
    Protocol(&'static str),
    /// JSON reply could not be parsed.
    Json(serde_json::Error),
    /// No keyboard with at least two layouts was found.
    NoKeyboard,
    /// The active layout index is outside the layout list.
    InvalidIndex(u64),
    /// The requested layout is not configured in Sway.
    LayoutNotFound(LayoutId),
    /// Sway rejected the switch command.
    Rejected(String),
}

impl std::fmt::Display for SwayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Sway IPC I/O error: {error}"),
            Self::NoSocket => f.write_str("no live Sway IPC socket (SWAYSOCK)"),
            Self::Protocol(message) => write!(f, "Sway IPC protocol error: {message}"),
            Self::Json(error) => write!(f, "Sway IPC JSON error: {error}"),
            Self::NoKeyboard => f.write_str("Sway has no keyboard with two layouts"),
            Self::InvalidIndex(index) => {
                write!(f, "Sway reported layout index {index} out of range")
            }
            Self::LayoutNotFound(layout) => {
                write!(f, "layout {layout} is not configured in Sway")
            }
            Self::Rejected(message) => write!(f, "Sway rejected the switch: {message}"),
        }
    }
}

impl std::error::Error for SwayError {}

impl From<io::Error> for SwayError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for SwayError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn alive(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Find a live Sway socket.
///
/// Prefers `$SWAYSOCK`; if the daemon was started from a stale systemd
/// environment, falls back to the newest `sway-ipc.*.sock` in
/// `$XDG_RUNTIME_DIR`.
#[must_use]
pub fn find_socket() -> Option<PathBuf> {
    if let Some(path) = env::var_os("SWAYSOCK").map(PathBuf::from) {
        if alive(&path) {
            return Some(path);
        }
    }
    let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    let mut sockets: Vec<(SystemTime, PathBuf)> = fs::read_dir(runtime)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !name.starts_with("sway-ipc.") || !name.ends_with(".sock") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    sockets.sort_by_key(|(modified, _)| *modified);
    sockets
        .into_iter()
        .rev()
        .map(|(_, path)| path)
        .find(|path| alive(path))
}

fn pack(kind: u32, payload: &str) -> Vec<u8> {
    let body = payload.as_bytes();
    let mut message = Vec::with_capacity(HEADER_SIZE + body.len());
    message.extend_from_slice(MAGIC);
    message.extend_from_slice(&(body.len() as u32).to_ne_bytes());
    message.extend_from_slice(&kind.to_ne_bytes());
    message.extend_from_slice(body);
    message
}

fn read_exact(stream: &mut UnixStream, buffer: &mut [u8]) -> Result<(), SwayError> {
    stream.read_exact(buffer).map_err(SwayError::Io)
}

/// One request/response exchange with Sway.
#[derive(Debug, Clone)]
pub struct SwayIpc {
    path: PathBuf,
}

impl SwayIpc {
    /// Create an IPC client for a socket path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Send one message and parse the JSON reply.
    pub fn request(&self, kind: u32, payload: &str) -> Result<Value, SwayError> {
        let mut stream = UnixStream::connect(&self.path)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(&pack(kind, payload))?;
        stream.flush()?;

        let mut header = [0_u8; HEADER_SIZE];
        read_exact(&mut stream, &mut header)?;
        if &header[..6] != MAGIC {
            return Err(SwayError::Protocol("bad i3-ipc magic"));
        }
        let length = u32::from_ne_bytes(header[6..10].try_into().expect("4 bytes")) as usize;
        let mut body = vec![0_u8; length];
        read_exact(&mut stream, &mut body)?;
        Ok(serde_json::from_slice(&body)?)
    }
}

/// Sway layout provider.
#[derive(Debug)]
pub struct SwayLayouts {
    ipc: SwayIpc,
    keyboard: Option<String>,
    layouts: Vec<Option<LayoutId>>,
    current_index: usize,
}

impl SwayLayouts {
    /// Connect to the running Sway and read its keyboard layouts.
    pub fn new() -> Result<Self, SwayError> {
        let path = find_socket().ok_or(SwayError::NoSocket)?;
        Self::with_ipc(SwayIpc::new(path))
    }

    /// Use an existing IPC client (also useful in tests).
    pub fn with_ipc(ipc: SwayIpc) -> Result<Self, SwayError> {
        let inputs = ipc.request(GET_INPUTS, "")?;
        let keyboard = select_keyboard(&inputs, None).ok_or(SwayError::NoKeyboard)?;
        let identifier = keyboard
            .get("identifier")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let layouts = parse_layouts(keyboard)?;
        let current_index = usize::try_from(parse_index(keyboard)?)
            .map_err(|_| SwayError::InvalidIndex(u64::MAX))?;
        Ok(Self {
            ipc,
            keyboard: identifier,
            layouts,
            current_index,
        })
    }

    /// Layouts in Sway's index order; unsupported entries are `None`.
    #[must_use]
    pub fn layouts(&self) -> &[Option<LayoutId>] {
        &self.layouts
    }

    /// Raw index of the active layout.
    #[must_use]
    pub const fn current_index(&self) -> usize {
        self.current_index
    }

    /// Current layout, if it is one of the supported pair.
    pub fn current(&self) -> Result<Option<LayoutId>, SwayError> {
        let inputs = self.ipc.request(GET_INPUTS, "")?;
        let keyboard =
            select_keyboard(&inputs, self.keyboard.as_deref()).ok_or(SwayError::NoKeyboard)?;
        let index = parse_index(keyboard)?;
        let layouts = parse_layouts(keyboard)?;
        let id = layouts
            .get(index as usize)
            .copied()
            .ok_or(SwayError::InvalidIndex(index))?;
        Ok(id)
    }

    /// Whether Sway has this layout.
    #[must_use]
    pub fn supports(&self, layout: LayoutId) -> bool {
        self.layouts.contains(&Some(layout))
    }

    /// Switch to a layout by its Sway index.
    pub fn switch_to(&mut self, layout: LayoutId) -> Result<(), SwayError> {
        let index = self
            .layouts
            .iter()
            .position(|entry| *entry == Some(layout))
            .ok_or(SwayError::LayoutNotFound(layout))?;
        let command = format!("input type:keyboard xkb_switch_layout {index}");
        let reply = self.ipc.request(RUN_COMMAND, &command)?;
        check_command_reply(&reply)?;
        self.current_index = index;
        Ok(())
    }
}

fn parse_layouts(keyboard: &Value) -> Result<Vec<Option<LayoutId>>, SwayError> {
    let names = keyboard
        .get("xkb_layout_names")
        .and_then(Value::as_array)
        .ok_or(SwayError::Protocol("keyboard has no xkb_layout_names"))?;
    Ok(names
        .iter()
        .map(|name| name.as_str().and_then(layout_id_from_name))
        .collect())
}

fn parse_index(keyboard: &Value) -> Result<u64, SwayError> {
    keyboard
        .get("xkb_active_layout_index")
        .and_then(Value::as_u64)
        .ok_or(SwayError::Protocol("keyboard has no active layout index"))
}

fn select_keyboard<'a>(inputs: &'a Value, preferred: Option<&str>) -> Option<&'a Value> {
    let devices = inputs.as_array()?;
    let candidates = devices.iter().filter(|device| {
        device.get("type").and_then(Value::as_str) == Some("keyboard")
            && device.get("name").and_then(Value::as_str)
                != Some(layoutpilot_core::VIRTUAL_KEYBOARD_NAME)
    });
    if let Some(identifier) = preferred {
        if let Some(device) = candidates
            .clone()
            .find(|device| device.get("identifier").and_then(Value::as_str) == Some(identifier))
        {
            return Some(device);
        }
    }
    candidates.into_iter().find(|device| {
        device
            .get("xkb_layout_names")
            .and_then(Value::as_array)
            .is_some_and(|names| names.len() >= 2)
    })
}

fn check_command_reply(reply: &Value) -> Result<(), SwayError> {
    let results = reply
        .as_array()
        .ok_or(SwayError::Protocol("command reply is not an array"))?;
    for result in results {
        if result.get("success").and_then(Value::as_bool) != Some(true) {
            let message = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(SwayError::Rejected(message.to_owned()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_socket() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("layoutpilot-sway-{}-{id}.sock", std::process::id()));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn parses_layouts_and_index() {
        let keyboard = json!({
            "type": "keyboard",
            "name": "AT Translated Set 2 keyboard",
            "identifier": "1133:49704:AT_Translated_Set_2_keyboard",
            "xkb_layout_names": ["English (US)", "Russian"],
            "xkb_active_layout_index": 1
        });
        assert_eq!(
            parse_layouts(&keyboard).expect("layouts"),
            vec![Some(LayoutId::Us), Some(LayoutId::Ru)]
        );
        assert_eq!(parse_index(&keyboard).expect("index"), 1);
    }

    #[test]
    fn skips_virtual_keyboard() {
        let inputs = json!([
            {
                "type": "keyboard",
                "name": layoutpilot_core::VIRTUAL_KEYBOARD_NAME,
                "xkb_layout_names": ["English (US)", "Russian"],
                "xkb_active_layout_index": 0
            },
            {
                "type": "keyboard",
                "name": "real",
                "xkb_layout_names": ["English (US)", "Russian"],
                "xkb_active_layout_index": 0
            }
        ]);
        let keyboard = select_keyboard(&inputs, None).expect("real keyboard");
        assert_eq!(keyboard["name"], "real");
    }

    #[test]
    fn ipc_round_trip() {
        let path = temp_socket();
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut header = [0_u8; HEADER_SIZE];
            stream.read_exact(&mut header).expect("header");
            let length = u32::from_ne_bytes(header[6..10].try_into().expect("length")) as usize;
            let mut body = vec![0_u8; length];
            stream.read_exact(&mut body).expect("body");
            let reply = br#"[{"type":"keyboard","name":"kbd","xkb_layout_names":["us","ru"],"xkb_active_layout_index":0}]"#;
            let mut message = Vec::new();
            message.extend_from_slice(MAGIC);
            message.extend_from_slice(&(reply.len() as u32).to_ne_bytes());
            message.extend_from_slice(&GET_INPUTS.to_ne_bytes());
            message.extend_from_slice(reply);
            stream.write_all(&message).expect("write");
        });
        let ipc = SwayIpc::new(&path);
        let reply = ipc.request(GET_INPUTS, "").expect("reply");
        assert_eq!(reply[0]["name"], "kbd");
        server.join().expect("server");
        let _ = fs::remove_file(&path);
    }
}
