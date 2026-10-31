//! Errors for X11 backends.

use std::fmt;

use x11rb::errors::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError};

/// An X11 backend error.
#[derive(Debug)]
pub enum X11Error {
    /// Initial connection failure.
    Connect(ConnectError),
    /// Low-level connection failure.
    Connection(ConnectionError),
    /// Request that allocates an id failed.
    ReplyOrId(ReplyOrIdError),
    /// X11 request failed.
    Reply(ReplyError),
    /// The server does not provide a required extension.
    MissingExtension(&'static str),
    /// A character has no physical key in the selected layout.
    UnsupportedChar(char),
    /// A keycode does not fit into the X11 8-bit keycode space.
    KeycodeOutOfRange(u16),
    /// More than four keyboard groups are not supported yet.
    TooManyGroups(usize),
    /// The requested layout is not configured for this keyboard.
    UnknownLayout(String),
    /// No active window could be determined.
    NoActiveWindow,
    /// The XRecord capture thread stopped.
    CaptureStopped,
    /// The XRecord context did not become ready in time.
    CaptureStartTimeout,
}

impl fmt::Display for X11Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => write!(f, "cannot connect to X11: {error}"),
            Self::Connection(error) => write!(f, "X11 connection error: {error}"),
            Self::Reply(error) => write!(f, "X11 request failed: {error}"),
            Self::ReplyOrId(error) => write!(f, "X11 id allocation failed: {error}"),
            Self::MissingExtension(name) => write!(f, "X11 extension {name} is not available"),
            Self::UnsupportedChar(ch) => write!(f, "no physical key for {ch:?}"),
            Self::KeycodeOutOfRange(keycode) => write!(f, "keycode {keycode} is out of range"),
            Self::TooManyGroups(count) => write!(f, "too many keyboard groups: {count}"),
            Self::UnknownLayout(layout) => write!(f, "layout {layout:?} is not configured"),
            Self::NoActiveWindow => f.write_str("no active X11 window"),
            Self::CaptureStopped => f.write_str("X11 capture stopped"),
            Self::CaptureStartTimeout => f.write_str("X11 capture did not start in time"),
        }
    }
}

impl std::error::Error for X11Error {}

impl From<ConnectError> for X11Error {
    fn from(error: ConnectError) -> Self {
        Self::Connect(error)
    }
}

impl From<ConnectionError> for X11Error {
    fn from(error: ConnectionError) -> Self {
        Self::Connection(error)
    }
}

impl From<ReplyError> for X11Error {
    fn from(error: ReplyError) -> Self {
        Self::Reply(error)
    }
}

impl From<ReplyOrIdError> for X11Error {
    fn from(error: ReplyOrIdError) -> Self {
        Self::ReplyOrId(error)
    }
}
