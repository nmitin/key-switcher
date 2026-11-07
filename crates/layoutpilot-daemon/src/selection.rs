//! Selected text on Wayland, through `wl-clipboard`.
//!
//! The primary selection holds whatever the user selected last. A watcher
//! (`wl-paste --primary --watch`) counts selection changes, so the daemon can
//! tell a selection made just now from a stale one: the selection is fresh
//! when it changed after the last key press or click. The replacement is
//! pasted through the regular clipboard, which is saved and restored around
//! the paste.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Timeout for one `wl-paste`/`wl-copy` run.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
/// Preferred MIME type for text.
const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Counts primary selection changes in the background.
#[derive(Debug)]
pub struct SelectionWatch {
    generation: Arc<AtomicU64>,
    child: Child,
}

impl SelectionWatch {
    /// Start `wl-paste --primary --watch`.
    pub fn start() -> io::Result<Self> {
        let mut child = Command::new("wl-paste")
            .args(["--primary", "--watch", "echo"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("wl-paste has no stdout"))?;
        let generation = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&generation);
        let started = Instant::now();
        thread::Builder::new()
            .name("selection-watch".to_owned())
            .spawn(move || {
                let mut initial = true;
                for line in BufReader::new(stdout).lines() {
                    if line.is_err() {
                        break;
                    }
                    // The watcher reports the current selection once at
                    // startup; that one was not made by the user just now.
                    if initial && started.elapsed() < Duration::from_secs(1) {
                        initial = false;
                        continue;
                    }
                    initial = false;
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                eprintln!("layoutpilot: selection watcher stopped");
            })?;
        Ok(Self { generation, child })
    }

    /// Number of selection changes seen so far.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Wait briefly until the generation moves past `seen`, so a change
    /// caused by the daemon itself is counted before it is marked as seen.
    pub fn wait_change(&self, seen: u64) {
        let deadline = Instant::now() + Duration::from_millis(200);
        while self.generation() == seen && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for SelectionWatch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run a command with optional stdin and a timeout.
///
/// Without input, returns stdout on success. With input (`wl-copy`), stdout
/// is discarded: `wl-copy` leaves a background server that would hold the
/// pipe open, so success is reported with an empty vector.
fn run(args: &[&str], program: &str, input: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if input.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(input) = input {
        // Dropping stdin after the write closes it, so `wl-copy` sees EOF.
        let mut stdin = child.stdin.take()?;
        if stdin.write_all(input).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    }
    // Read on a thread so a large selection cannot fill the pipe and stall
    // the child until the timeout.
    let reader = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output).map(|_| output)
        })
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    match reader {
        Some(reader) => reader.join().ok()?.ok(),
        None => Some(Vec::new()),
    }
}

/// Text of the primary selection, if any.
pub fn read_primary() -> Option<String> {
    let bytes = run(
        &["--primary", "--no-newline", "--type", "text"],
        "wl-paste",
        None,
    )?;
    String::from_utf8(bytes).ok().filter(|text| !text.is_empty())
}

/// Wait until the primary selection is exactly `text`.
pub fn wait_primary(text: &str) -> bool {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    while Instant::now() < deadline {
        if read_primary().as_deref() == Some(text) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Clipboard contents saved before a paste.
#[derive(Debug)]
pub struct SavedClipboard {
    mime: String,
    data: Vec<u8>,
}

/// Save the clipboard in one MIME type; `None` when it is empty.
///
/// Only one representation survives: text is kept as text, an image as the
/// image, but rich text loses its formatting.
pub fn save_clipboard() -> Option<SavedClipboard> {
    let types = run(&["--list-types"], "wl-paste", None)?;
    let types = String::from_utf8_lossy(&types);
    let types: Vec<&str> = types.lines().map(str::trim).filter(|t| !t.is_empty()).collect();
    let mime = [TEXT_MIME, "text/plain", "UTF8_STRING", "STRING"]
        .into_iter()
        .find(|preferred| types.contains(preferred))
        .or_else(|| types.first().copied())?
        .to_owned();
    let data = run(&["--no-newline", "--type", &mime], "wl-paste", None)?;
    Some(SavedClipboard { mime, data })
}

/// Put `text` on the clipboard and wait until it is offered.
pub fn set_clipboard(text: &str) -> bool {
    if run(&["--type", TEXT_MIME], "wl-copy", Some(text.as_bytes())).is_none() {
        return false;
    }
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    while Instant::now() < deadline {
        let current = run(&["--no-newline", "--type", "text"], "wl-paste", None);
        if current.as_deref() == Some(text.as_bytes()) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Put saved contents back, or clear the clipboard if it was empty.
pub fn restore_clipboard(saved: Option<SavedClipboard>) {
    let restored = match saved {
        Some(saved) => run(&["--type", &saved.mime], "wl-copy", Some(&saved.data)),
        None => run(&["--clear"], "wl-copy", None),
    };
    if restored.is_none() {
        eprintln!("layoutpilot: could not restore the clipboard");
    }
}

/// Terminal emulators: selected text there is output, not editable input.
pub fn is_terminal(app_id: &str) -> bool {
    const TERMINALS: [&str; 11] = [
        "term", "konsole", "yakuake", "kitty", "alacritty", "foot", "ghostty", "tilix",
        "guake", "tilda", "urxvt",
    ];
    let app_id = app_id.to_ascii_lowercase();
    TERMINALS.iter().any(|name| app_id.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_are_recognised() {
        assert!(is_terminal("org.kde.konsole"));
        assert!(is_terminal("org.kde.yakuake"));
        assert!(is_terminal("org.gnome.Terminal"));
        assert!(is_terminal("kitty"));
        assert!(!is_terminal("geany"));
        assert!(!is_terminal("yandex-browser"));
    }

    /// Needs a Wayland session with wl-clipboard; touches the real
    /// clipboard and primary selection (both restored).
    #[test]
    #[ignore = "uses the live Wayland clipboard"]
    fn live_clipboard_round_trip() {
        let primary = read_primary();
        let watch = SelectionWatch::start().expect("wl-paste");
        thread::sleep(Duration::from_millis(300));
        let before = watch.generation();
        assert!(run(&["--primary"], "wl-copy", Some("ghbdtn vbh".as_bytes())).is_some());
        thread::sleep(Duration::from_millis(300));
        assert!(watch.generation() > before, "selection change not seen");
        assert_eq!(read_primary().as_deref(), Some("ghbdtn vbh"));

        let saved = save_clipboard();
        let saved_data = saved.as_ref().map(|saved| saved.data.clone());
        assert!(set_clipboard("привет мир"));
        restore_clipboard(saved);
        thread::sleep(Duration::from_millis(100));
        let restored = run(&["--no-newline", "--type", "text"], "wl-paste", None);
        if let Some(data) = saved_data {
            assert_eq!(restored, Some(data));
        }
        match primary {
            Some(text) => run(&["--primary"], "wl-copy", Some(text.as_bytes())),
            None => run(&["--primary", "--clear"], "wl-copy", None),
        };
    }
}
