//! Platform-independent core for LayoutPilot.
//!
//! This crate contains no OS calls and no I/O. It can be tested on any host and
//! reused by the daemon, the CLI and the test harness.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod buffer;
pub mod conversion;
pub mod detector;
pub mod engine;
pub mod manager;
pub mod profile;
pub mod protection;
pub mod segmenter;

pub use buffer::CharBuffer;
pub use conversion::{
    convert_last_word, convert_text, convert_word, reinterpret, ConversionError, LastWordConversion,
};
pub use detector::{DetectionContext, Detector, LexiconDetector, StaticDetector};
pub use engine::{Action, Correction, Engine, EngineState, InputContext, Modifiers, ResetReason};
pub use layoutpilot_layouts::{Keystroke, LayoutId, LayoutTable};
pub use manager::{CorrectionManager, CorrectionPlan, PlanKind};
pub use profile::{glob_match, AppRule, EngineConfig, Profile, ProfileResolver};
pub use protection::{is_protected_context, is_protected_token};
pub use segmenter::{is_word_char, last_word, tokenize, Token, TokenKind, WordSlice};
