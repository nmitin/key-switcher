//! XRecord capture of global key events.
//!
//! XRecord uses two connections: one for control and one for the blocking
//! stream of recorded data. The data connection is moved into a thread and
//! forwards parsed key events through a channel. `stop` disables the context
//! from the control connection, which ends the iterator.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::record::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, KeyPressEvent, KeyReleaseEvent};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::TryParse;

use crate::error::X11Error;

/// A key event captured from XRecord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// X11 keycode.
    pub keycode: u8,
    /// Modifier state (`KeyButMask` bits).
    pub state: u16,
    /// Server timestamp.
    pub time: u32,
    /// True for press, false for release.
    pub pressed: bool,
}

/// Global XRecord key capture.
#[derive(Debug)]
pub struct XRecordCapture {
    control: RustConnection,
    context: record::Context,
    receiver: Receiver<KeyEvent>,
    thread: Option<JoinHandle<()>>,
}

impl XRecordCapture {
    /// Connect to X11 and start recording key press/release events.
    pub fn new() -> Result<Self, X11Error> {
        let (control, _) = RustConnection::connect(None)?;
        if control
            .extension_information(record::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Err(X11Error::MissingExtension("RECORD"));
        }
        control
            .record_query_version(
                record::X11_XML_VERSION.0 as u16,
                record::X11_XML_VERSION.1 as u16,
            )?
            .reply()?;

        let (data, _) = RustConnection::connect(None)?;
        let context = control.generate_id()?;

        let empty = record::Range8 { first: 0, last: 0 };
        let empty_ext = record::ExtRange {
            major: empty,
            minor: record::Range16 { first: 0, last: 0 },
        };
        let range = record::Range {
            core_requests: empty,
            core_replies: empty,
            ext_requests: empty_ext,
            ext_replies: empty_ext,
            delivered_events: empty,
            device_events: record::Range8 {
                first: xproto::KEY_PRESS_EVENT,
                last: xproto::KEY_RELEASE_EVENT,
            },
            errors: empty,
            client_started: false,
            client_died: false,
        };
        control
            .record_create_context(context, 0, &[record::CS::ALL_CLIENTS.into()], &[range])?
            .check()?;

        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            let Ok(stream) = data.record_enable_context(context) else {
                return;
            };
            for reply in stream {
                let Ok(reply) = reply else {
                    break;
                };
                if reply.category == 4 {
                    let _ = ready_sender.send(());
                } else if reply.category == 0 {
                    parse_events(&reply.data, &sender);
                }
            }
        });

        match ready_receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => {}
            Err(_) => {
                let _ = control.record_disable_context(context);
                let _ = control.sync();
                let _ = thread.join();
                return Err(X11Error::CaptureStartTimeout);
            }
        }

        Ok(Self {
            control,
            context,
            receiver,
            thread: Some(thread),
        })
    }

    /// Wait for the next key event.
    pub fn recv(&self) -> Result<KeyEvent, X11Error> {
        self.receiver.recv().map_err(|_| X11Error::CaptureStopped)
    }

    /// Wait for the next key event with a timeout.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<KeyEvent>, X11Error> {
        match self.receiver.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(X11Error::CaptureStopped),
        }
    }

    /// Stop recording and join the data thread.
    pub fn stop(&mut self) -> Result<(), X11Error> {
        if let Some(thread) = self.thread.take() {
            let _ = self.control.record_disable_context(self.context);
            self.control.sync()?;
            let _ = thread.join();
        }
        Ok(())
    }
}

impl Drop for XRecordCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn parse_events(data: &[u8], sender: &Sender<KeyEvent>) {
    let mut offset = 0;
    while offset + 32 <= data.len() {
        match data[offset] {
            xproto::KEY_PRESS_EVENT => {
                if let Ok((event, _)) = KeyPressEvent::try_parse(&data[offset..]) {
                    let _ = sender.send(KeyEvent {
                        keycode: event.detail,
                        state: u16::from(event.state),
                        time: event.time,
                        pressed: true,
                    });
                }
            }
            xproto::KEY_RELEASE_EVENT => {
                if let Ok((event, _)) = KeyReleaseEvent::try_parse(&data[offset..]) {
                    let _ = sender.send(KeyEvent {
                        keycode: event.detail,
                        state: u16::from(event.state),
                        time: event.time,
                        pressed: false,
                    });
                }
            }
            _ => {}
        }
        offset += 32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a reachable X11 server"]
    fn records_an_xtest_key() {
        let mut capture = XRecordCapture::new().expect("XRecord");
        let mut injector = crate::X11Injector::new().expect("XTest");
        injector
            .tap_keycode(crate::SHIFT_KEYCODE, false)
            .expect("inject");

        let mut seen = false;
        for _ in 0..10 {
            match capture.recv_timeout(Duration::from_millis(500)) {
                Ok(Some(event)) if event.keycode == crate::SHIFT_KEYCODE => {
                    seen = true;
                    if !event.pressed {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => panic!("capture: {error}"),
            }
        }
        capture.stop().expect("stop");
        assert!(seen, "XTest event was not captured");
    }
}
