//! X11 backends for LayoutPilot.
//!
//! Implemented for the v1 vertical slice:
//! * XTest injection (`X11Injector`);
//! * XKB group query and switching (`X11LayoutState`);
//! * XRecord global key capture (`XRecordCapture`);
//! * active window / `WM_CLASS` / title (`X11Focus`);
//! * keycode-to-character normalization using the ru/en tables.
//!
//! Layout state is currently queried on demand (polling); the event-driven
//! `XkbStateNotify` provider is the next step from the stage 0 checklist.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod capture;
mod error;
mod focus;
mod inject;
mod layout;
mod normalize;

pub use capture::{KeyEvent, XRecordCapture};
pub use error::X11Error;
pub use focus::{FocusInfo, X11Focus};
pub use inject::{X11Injector, SHIFT_KEYCODE};
pub use layout::X11LayoutState;
pub use normalize::char_for;
