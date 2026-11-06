//! Screen lock state from the session bus.
//!
//! KDE (`org.freedesktop.ScreenSaver`) and GNOME (`org.gnome.ScreenSaver`)
//! emit `ActiveChanged(bool)` when the screen locks or unlocks. While locked
//! the daemon ignores keyboard input entirely: the lock screen is where the
//! password is typed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type;
use zbus::MatchRule;

const INTERFACES: [&str; 2] = ["org.freedesktop.ScreenSaver", "org.gnome.ScreenSaver"];

/// Watches the screen saver's `ActiveChanged` signal.
pub(crate) struct ScreenLock {
    locked: Arc<AtomicBool>,
}

impl ScreenLock {
    /// Read the current state and follow changes in a background thread.
    pub(crate) fn start() -> Result<Self, zbus::Error> {
        let connection = Connection::session()?;
        let locked = Arc::new(AtomicBool::new(initial_state(&connection)));
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .member("ActiveChanged")?
            .build();
        let messages = MessageIterator::for_match_rule(rule, &connection, None)?;
        let flag = Arc::clone(&locked);
        thread::Builder::new()
            .name("screen-lock".to_owned())
            .spawn(move || {
                for message in messages.flatten() {
                    let header = message.header();
                    let ours = header
                        .interface()
                        .is_some_and(|interface| INTERFACES.contains(&interface.as_str()));
                    if !ours {
                        continue;
                    }
                    if let Ok(active) = message.body().deserialize::<bool>() {
                        flag.store(active, Ordering::Release);
                    }
                }
            })
            .map_err(|error| zbus::Error::InputOutput(Arc::new(error)))?;
        Ok(Self { locked })
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }
}

/// `GetActive` from whichever screen saver answers; unlocked if none does.
fn initial_state(connection: &Connection) -> bool {
    for (service, path) in [
        ("org.freedesktop.ScreenSaver", "/ScreenSaver"),
        (
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
        ),
        ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
    ] {
        let interface = if service.starts_with("org.gnome") {
            "org.gnome.ScreenSaver"
        } else {
            "org.freedesktop.ScreenSaver"
        };
        let active = Proxy::new(connection, service, path, interface)
            .and_then(|proxy| proxy.call::<_, _, bool>("GetActive", &()));
        if let Ok(active) = active {
            return active;
        }
    }
    false
}
