//! Compositor-specific layout backends.
//!
//! KDE Plasma exposes the active layout through the session bus:
//! `org.kde.keyboard` → `/Layouts` → `org.kde.KeyboardLayouts`.
//! Sway and Hyprland expose it through their IPC sockets.
//! These are the Wayland-safe paths; locking an XKB group on XWayland does not
//! update the compositor's own layout indicator.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod hyprland;
mod sway;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use layoutpilot_core::LayoutId;
use zbus::blocking::{Connection, Proxy};

pub use hyprland::{HyprlandError, HyprlandLayouts};
pub use sway::{SwayError, SwayLayouts};

const KDE_SERVICE: &str = "org.kde.keyboard";
const KDE_PATH: &str = "/Layouts";
const KDE_INTERFACE: &str = "org.kde.KeyboardLayouts";
/// With a live signal subscription, `getLayout` is only a periodic sanity
/// check against a missed signal.
const KDE_RESYNC: Duration = Duration::from_secs(2);
const UNKNOWN_INDEX: u32 = u32::MAX;

/// Map an XKB layout description to a supported [`LayoutId`].
///
/// Sway reports descriptions such as `English (US)` / `Russian`, while
/// Hyprland usually reports short codes (`us,ru`). Anything else stays
/// unsupported and disables automatic correction for that group.
#[must_use]
pub fn layout_id_from_name(name: &str) -> Option<LayoutId> {
    let trimmed = name.trim();
    if let Some(id) = LayoutId::from_code(trimmed) {
        return Some(id);
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("russian")
        || lower.contains("(ru")
        || lower.starts_with("ru ")
        || lower.starts_with("ru(")
        || lower.starts_with("ru+")
    {
        Some(LayoutId::Ru)
    } else if lower.contains("english")
        || lower.contains("(us")
        || lower.starts_with("us ")
        || lower.starts_with("us(")
        || lower.starts_with("us+")
    {
        Some(LayoutId::Us)
    } else {
        None
    }
}

/// One layout entry reported by KDE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KdeLayout {
    /// Index used by `setLayout`.
    pub index: u32,
    /// XKB layout code, e.g. `us` or `ru`.
    pub code: String,
    /// Variant code.
    pub variant: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Mapped LayoutPilot layout, if supported.
    pub id: Option<LayoutId>,
}

/// KDE layout backend error.
#[derive(Debug)]
pub enum KdeError {
    /// D-Bus error.
    Zbus(zbus::Error),
    /// Requested layout is not configured in KDE.
    LayoutNotFound(LayoutId),
    /// KDE returned an index outside the cached list.
    InvalidIndex(u32),
    /// KDE refused the switch.
    Rejected(LayoutId),
}

impl fmt::Display for KdeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zbus(error) => write!(f, "KDE D-Bus error: {error}"),
            Self::LayoutNotFound(layout) => {
                write!(f, "layout {layout} is not configured in KDE")
            }
            Self::InvalidIndex(index) => write!(f, "KDE returned invalid layout index {index}"),
            Self::Rejected(layout) => write!(f, "KDE rejected switching to {layout}"),
        }
    }
}

impl std::error::Error for KdeError {}

impl From<zbus::Error> for KdeError {
    fn from(error: zbus::Error) -> Self {
        Self::Zbus(error)
    }
}

/// KDE Plasma layout provider.
#[derive(Debug)]
pub struct KdeLayouts {
    connection: Connection,
    layouts: Vec<KdeLayout>,
    /// Index from the last `layoutChanged` signal or query.
    cached: Arc<AtomicU32>,
    /// The signal thread is running.
    watching: Arc<AtomicBool>,
    last_sync: Mutex<Option<Instant>>,
}

impl KdeLayouts {
    /// Connect to the session bus and read the configured layouts.
    pub fn new() -> Result<Self, KdeError> {
        let connection = Connection::session()?;
        let mut provider = Self {
            connection,
            layouts: Vec::new(),
            cached: Arc::new(AtomicU32::new(UNKNOWN_INDEX)),
            watching: Arc::new(AtomicBool::new(false)),
            last_sync: Mutex::new(None),
        };
        provider.layouts = provider.read_layouts()?;
        Ok(provider)
    }

    /// Follow `layoutChanged` signals in a background thread, so that
    /// [`Self::current`] does not need a D-Bus round trip per call.
    pub fn watch(&self) -> Result<(), KdeError> {
        let connection = Connection::session()?;
        let cached = Arc::clone(&self.cached);
        let watching = Arc::clone(&self.watching);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("kde-layout-signals".to_owned())
            .spawn(move || {
                let signals = Proxy::new(&connection, KDE_SERVICE, KDE_PATH, KDE_INTERFACE)
                    .and_then(|proxy| proxy.receive_signal("layoutChanged"));
                let signals = match signals {
                    Ok(signals) => {
                        let _ = ready_tx.send(Ok(()));
                        signals
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                watching.store(true, Ordering::Release);
                for message in signals {
                    if let Ok(index) = message.body().deserialize::<u32>() {
                        cached.store(index, Ordering::Release);
                    }
                }
                watching.store(false, Ordering::Release);
            })
            .map_err(|error| KdeError::Zbus(zbus::Error::InputOutput(Arc::new(error))))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(KdeError::Zbus(zbus::Error::Failure(
                "layout signal thread exited".to_owned(),
            ))),
        }
    }

    /// Whether the signal subscription is alive.
    #[must_use]
    pub fn is_watching(&self) -> bool {
        self.watching.load(Ordering::Acquire)
    }

    /// Cached layout entries.
    #[must_use]
    pub fn layouts(&self) -> &[KdeLayout] {
        &self.layouts
    }

    /// Current KDE layout: from the signal cache while subscribed, with a
    /// D-Bus query at most every `KDE_RESYNC`.
    pub fn current(&self) -> Result<Option<LayoutId>, KdeError> {
        let cached = self.cached.load(Ordering::Acquire);
        let fresh = self
            .last_sync
            .lock()
            .ok()
            .and_then(|last| *last)
            .is_some_and(|at| at.elapsed() < KDE_RESYNC);
        let index = if self.is_watching() && fresh && cached != UNKNOWN_INDEX {
            cached
        } else {
            let index = self.current_index()?;
            self.cached.store(index, Ordering::Release);
            if let Ok(mut last) = self.last_sync.lock() {
                *last = Some(Instant::now());
            }
            index
        };
        self.layouts
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .map_or(Ok(None), |layout| Ok(layout.id))
    }

    /// Current raw KDE index.
    pub fn current_index(&self) -> Result<u32, KdeError> {
        let proxy = self.proxy()?;
        Ok(proxy.call("getLayout", &())?)
    }

    /// Whether KDE has this layout.
    #[must_use]
    pub fn supports(&self, layout: LayoutId) -> bool {
        self.layouts.iter().any(|entry| entry.id == Some(layout))
    }

    /// Switch to a layout using KDE's D-Bus API.
    pub fn switch_to(&self, layout: LayoutId) -> Result<(), KdeError> {
        let entry = self
            .layouts
            .iter()
            .find(|entry| entry.id == Some(layout))
            .ok_or(KdeError::LayoutNotFound(layout))?;
        let proxy = self.proxy()?;
        let accepted: bool = proxy.call("setLayout", &(entry.index,))?;
        if accepted {
            // Do not wait for our own signal to see the new layout.
            self.cached.store(entry.index, Ordering::Release);
            Ok(())
        } else {
            Err(KdeError::Rejected(layout))
        }
    }

    fn proxy(&self) -> Result<Proxy<'_>, KdeError> {
        Ok(Proxy::new(
            &self.connection,
            KDE_SERVICE,
            KDE_PATH,
            KDE_INTERFACE,
        )?)
    }

    fn read_layouts(&self) -> Result<Vec<KdeLayout>, KdeError> {
        let proxy = self.proxy()?;
        let list: Vec<(String, String, String)> = proxy.call("getLayoutsList", &())?;
        Ok(list
            .into_iter()
            .enumerate()
            .map(|(index, (code, variant, display_name))| KdeLayout {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                id: LayoutId::from_code(&code),
                code,
                variant,
                display_name,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_compositor_layout_names() {
        assert_eq!(layout_id_from_name("us"), Some(LayoutId::Us));
        assert_eq!(layout_id_from_name("RU"), Some(LayoutId::Ru));
        assert_eq!(layout_id_from_name("English (US)"), Some(LayoutId::Us));
        assert_eq!(layout_id_from_name("Russian"), Some(LayoutId::Ru));
        assert_eq!(layout_id_from_name("ru(phonetic)"), Some(LayoutId::Ru));
        assert_eq!(layout_id_from_name("German"), None);
    }

    #[test]
    #[ignore = "requires a KDE Plasma session with at least two layouts"]
    fn reads_and_switches_kde_layouts() {
        let provider = KdeLayouts::new().expect("KDE backend");
        assert!(!provider.layouts().is_empty());
        let current = provider.current().expect("current layout");
        let other = [LayoutId::Us, LayoutId::Ru]
            .into_iter()
            .find(|layout| Some(*layout) != current && provider.supports(*layout));
        let Some(other) = other else {
            return;
        };
        provider.switch_to(other).expect("switch");
        assert_eq!(provider.current().expect("current"), Some(other));
        if let Some(current) = current {
            provider.switch_to(current).expect("restore");
            assert_eq!(provider.current().expect("current"), Some(current));
        }
    }
}
