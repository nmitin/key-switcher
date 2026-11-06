//! evdev keyboard capture.
//!
//! The capture opens all readable `/dev/input/event*` devices that look like
//! keyboards, sets them to non-blocking mode and reads `EV_KEY` events in one
//! thread per device. No `EVIOCGRAB` is used: the daemon is a passive reader.
//!
//! Hotplug is not handled yet: devices are enumerated once at startup. This is
//! a deliberate stage-1 limitation, not a silent guarantee.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use evdev::{Device, EventType, KeyCode};

/// Directory with evdev nodes.
pub const INPUT_DIR: &str = "/dev/input";

/// Difference between XKB keycodes and evdev keycodes.
pub const EVDEV_TO_XKB_OFFSET: u16 = 8;

const LETTERS: [KeyCode; 26] = [
    KeyCode::KEY_A,
    KeyCode::KEY_B,
    KeyCode::KEY_C,
    KeyCode::KEY_D,
    KeyCode::KEY_E,
    KeyCode::KEY_F,
    KeyCode::KEY_G,
    KeyCode::KEY_H,
    KeyCode::KEY_I,
    KeyCode::KEY_J,
    KeyCode::KEY_K,
    KeyCode::KEY_L,
    KeyCode::KEY_M,
    KeyCode::KEY_N,
    KeyCode::KEY_O,
    KeyCode::KEY_P,
    KeyCode::KEY_Q,
    KeyCode::KEY_R,
    KeyCode::KEY_S,
    KeyCode::KEY_T,
    KeyCode::KEY_U,
    KeyCode::KEY_V,
    KeyCode::KEY_W,
    KeyCode::KEY_X,
    KeyCode::KEY_Y,
    KeyCode::KEY_Z,
];

/// A device that looks like a keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardInfo {
    /// `/dev/input/eventN` path.
    pub path: PathBuf,
    /// Kernel device name.
    pub name: Option<String>,
    /// Physical path, if reported by the kernel.
    pub physical_path: Option<String>,
}

/// A key event from evdev.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvdevKeyEvent {
    /// XKB keycode (`evdev + 8`).
    pub keycode: u16,
    /// 0 — release, 1 — press, 2 — repeat.
    pub value: i32,
    /// Kernel device name, for diagnostics.
    pub device: String,
}

impl EvdevKeyEvent {
    /// Whether this is a press or an autorepeat.
    #[must_use]
    pub const fn pressed(&self) -> bool {
        self.value > 0
    }
}

/// evdev capture error.
#[derive(Debug)]
pub enum EvdevError {
    /// `/dev/input` is missing.
    NoInputDirectory,
    /// Permission denied and no readable keyboard was found.
    PermissionDenied(PathBuf, io::Error),
    /// No readable device looks like a keyboard.
    NoKeyboards,
    /// Generic I/O error.
    Io(io::Error),
    /// The capture thread stopped.
    CaptureStopped,
    /// A device could not be opened.
    Device {
        /// Device path.
        path: PathBuf,
        /// Underlying error.
        error: io::Error,
    },
}

impl std::fmt::Display for EvdevError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoInputDirectory => write!(f, "{INPUT_DIR} does not exist"),
            Self::PermissionDenied(path, error) => {
                write!(f, "permission denied opening {}: {error}", path.display())
            }
            Self::NoKeyboards => f.write_str("no readable keyboard devices found"),
            Self::Io(error) => write!(f, "evdev I/O error: {error}"),
            Self::CaptureStopped => f.write_str("evdev capture stopped"),
            Self::Device { path, error } => {
                write!(f, "cannot open {}: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for EvdevError {}

impl From<io::Error> for EvdevError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Convert an evdev keycode to an XKB keycode.
#[must_use]
pub const fn xkb_keycode(evdev_keycode: u16) -> u16 {
    evdev_keycode + EVDEV_TO_XKB_OFFSET
}

/// Whether an evdev key set is keyboard-like (at least 20 letter keys).
#[must_use]
pub fn has_keyboard_keys(keys: impl IntoIterator<Item = KeyCode>) -> bool {
    keys.into_iter().filter(|key| LETTERS.contains(key)).count() >= 20
}

fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| has_keyboard_keys(keys.iter()))
}

/// Enumerate readable keyboards.
///
/// Returns [`EvdevError::NoInputDirectory`] when `/dev/input` is absent,
/// [`EvdevError::PermissionDenied`] when devices exist but cannot be opened,
/// and [`EvdevError::NoKeyboards`] when no device passes the keyboard filter.
pub fn probe_keyboards() -> Result<Vec<KeyboardInfo>, EvdevError> {
    let directory = match fs::read_dir(INPUT_DIR) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(EvdevError::NoInputDirectory)
        }
        Err(error) => return Err(EvdevError::Io(error)),
    };

    let mut keyboards = Vec::new();
    let mut permission_error = None;
    for entry in directory {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let is_event_node = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"));
        if !is_event_node {
            continue;
        }

        match Device::open(&path) {
            Ok(device) => {
                if device.name() == Some(layoutpilot_core::VIRTUAL_KEYBOARD_NAME) {
                    // Own injection device: feeding its events back would create
                    // a correction loop.
                    continue;
                }
                if is_keyboard(&device) {
                    keyboards.push(KeyboardInfo {
                        path: path.clone(),
                        name: device.name().map(str::to_owned),
                        physical_path: device.physical_path().map(str::to_owned),
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                permission_error = Some((path, error));
            }
            Err(_) => {}
        }
    }

    if keyboards.is_empty() {
        if let Some((path, error)) = permission_error {
            return Err(EvdevError::PermissionDenied(path, error));
        }
        return Err(EvdevError::NoKeyboards);
    }
    Ok(keyboards)
}

/// How often `/dev/input` is rescanned for keyboards plugged in later.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// A keyboard appeared or disappeared after capture started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceChange {
    /// A new keyboard is being read.
    Added(KeyboardInfo),
    /// A keyboard stopped delivering events (unplugged or failed).
    Removed(KeyboardInfo),
}

/// State shared by the reader threads and the hotplug scanner.
#[derive(Debug, Default)]
struct Shared {
    stop: AtomicBool,
    /// Paths with a live reader thread.
    open: Mutex<HashSet<PathBuf>>,
}

/// Capture from all readable keyboards, including ones plugged in later.
///
/// Every keyboard gets a reader thread doing blocking reads; a scanner thread
/// rescans `/dev/input` every [`RESCAN_INTERVAL`]. Kernel `SYN_DROPPED`
/// overflows are resynchronised by [`evdev::Device::fetch_events`], which
/// synthesises the missed key state changes.
#[derive(Debug)]
pub struct EvdevCapture {
    devices: Vec<KeyboardInfo>,
    receiver: Receiver<EvdevKeyEvent>,
    changes: Receiver<DeviceChange>,
    shared: Arc<Shared>,
}

impl EvdevCapture {
    /// Open all readable keyboards and start reader threads.
    pub fn open() -> Result<Self, EvdevError> {
        let devices = probe_keyboards()?;
        let (sender, receiver) = mpsc::channel();
        let (change_sender, changes) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        // Open everything first: if one device fails, no reader thread is left
        // behind with no owner to stop it.
        let mut opened = Vec::with_capacity(devices.len());
        for info in &devices {
            let device = Device::open(&info.path).map_err(|error| EvdevError::Device {
                path: info.path.clone(),
                error,
            })?;
            opened.push((info.clone(), device));
        }
        for (info, device) in opened {
            spawn_reader(info, device, &sender, &change_sender, &shared);
        }

        let scanner_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("evdev-hotplug".to_owned())
            .spawn(move || scan_hotplug(&sender, &change_sender, &scanner_shared))?;

        Ok(Self {
            devices,
            receiver,
            changes,
            shared,
        })
    }

    /// Devices opened at startup.
    #[must_use]
    pub fn devices(&self) -> &[KeyboardInfo] {
        &self.devices
    }

    /// Keyboards added or removed since the last call. Never blocks.
    #[must_use]
    pub fn take_device_changes(&self) -> Vec<DeviceChange> {
        self.changes.try_iter().collect()
    }

    /// Wait for the next key event.
    pub fn recv(&self) -> Result<EvdevKeyEvent, EvdevError> {
        self.receiver.recv().map_err(|_| EvdevError::CaptureStopped)
    }

    /// Wait for the next key event with a timeout.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<EvdevKeyEvent>, EvdevError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(EvdevError::CaptureStopped),
        }
    }

    /// Stop capturing. Reader threads block in `read(2)`; they notice the
    /// flag on their next event and exit with the process otherwise.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for EvdevCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_reader(
    info: KeyboardInfo,
    mut device: Device,
    sender: &Sender<EvdevKeyEvent>,
    changes: &Sender<DeviceChange>,
    shared: &Arc<Shared>,
) {
    if let Ok(mut open) = shared.open.lock() {
        open.insert(info.path.clone());
    }
    let path = info.path.clone();
    let sender = sender.clone();
    let changes = changes.clone();
    let thread_shared = Arc::clone(shared);
    let spawned = thread::Builder::new()
        .name("evdev-reader".to_owned())
        .spawn(move || {
            let name = info
                .name
                .clone()
                .unwrap_or_else(|| info.path.display().to_string());
            read_device(&mut device, &name, &sender, &thread_shared.stop);
            if let Ok(mut open) = thread_shared.open.lock() {
                open.remove(&info.path);
            }
            if !thread_shared.stop.load(Ordering::Relaxed) {
                let _ = changes.send(DeviceChange::Removed(info));
            }
        });
    if spawned.is_err() {
        if let Ok(mut open) = shared.open.lock() {
            open.remove(&path);
        }
    }
}

/// Periodically open keyboards that are not read yet.
fn scan_hotplug(
    sender: &Sender<EvdevKeyEvent>,
    changes: &Sender<DeviceChange>,
    shared: &Arc<Shared>,
) {
    // Non-keyboards (mice, power buttons…) are probed once per node.
    let mut ignored: HashSet<PathBuf> = HashSet::new();
    loop {
        thread::sleep(RESCAN_INTERVAL);
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(paths) = event_nodes() else {
            continue;
        };
        ignored.retain(|path| paths.contains(path));
        let open = shared
            .open
            .lock()
            .map(|open| open.clone())
            .unwrap_or_default();
        for path in paths {
            if open.contains(&path) || ignored.contains(&path) {
                continue;
            }
            let Ok(device) = Device::open(&path) else {
                ignored.insert(path);
                continue;
            };
            if device.name() == Some(layoutpilot_core::VIRTUAL_KEYBOARD_NAME)
                || !is_keyboard(&device)
            {
                ignored.insert(path);
                continue;
            }
            let info = KeyboardInfo {
                path,
                name: device.name().map(str::to_owned),
                physical_path: device.physical_path().map(str::to_owned),
            };
            spawn_reader(info.clone(), device, sender, changes, shared);
            if changes.send(DeviceChange::Added(info)).is_err() {
                return;
            }
        }
    }
}

/// `/dev/input/event*` nodes.
fn event_nodes() -> io::Result<HashSet<PathBuf>> {
    Ok(fs::read_dir(INPUT_DIR)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .collect())
}

/// Blocking read loop; returns when the device fails (e.g. is unplugged), the
/// receiver is gone or capture is stopped.
fn read_device(device: &mut Device, name: &str, sender: &Sender<EvdevKeyEvent>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    if event.event_type() != EventType::KEY {
                        continue;
                    }
                    let key = EvdevKeyEvent {
                        keycode: xkb_keycode(event.code()),
                        value: event.value(),
                        device: name.to_owned(),
                    };
                    if sender.send(key).is_err() {
                        return;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_to_xkb_offset() {
        assert_eq!(xkb_keycode(30), 38);
        assert_eq!(xkb_keycode(0), 8);
    }

    #[test]
    fn keyboard_filter_needs_letters() {
        assert!(has_keyboard_keys(LETTERS));
        assert!(!has_keyboard_keys([
            KeyCode::KEY_A,
            KeyCode::KEY_B,
            KeyCode::KEY_ENTER,
        ]));
        assert!(!has_keyboard_keys([KeyCode::KEY_POWER]));
    }

    #[test]
    fn event_press_semantics() {
        let event = EvdevKeyEvent {
            keycode: 38,
            value: 1,
            device: "test".to_owned(),
        };
        assert!(event.pressed());
        let release = EvdevKeyEvent {
            value: 0,
            ..event.clone()
        };
        assert!(!release.pressed());
        let repeat = EvdevKeyEvent { value: 2, ..event };
        assert!(repeat.pressed());
    }
}
