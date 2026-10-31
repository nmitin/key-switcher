//! LayoutPilot configuration (TZ §10).
//!
//! The config is a TOML file loaded at startup. Unknown fields are ignored for
//! forward compatibility; invalid values are rejected by [`Config::validate`].
//! Hot reload is not wired yet.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use layoutpilot_core::{AppRule, EngineConfig, LayoutId, Profile, ProfileResolver};

pub mod hotkey;

pub use hotkey::{Hotkey, HotkeyError};
use serde::{Deserialize, Serialize};

/// Result type for configuration operations.
pub type ConfigResult<T> = Result<T, ConfigError>;

/// Configuration error.
#[derive(Debug)]
pub enum ConfigError {
    /// File could not be read.
    Io(std::io::Error),
    /// TOML could not be parsed.
    Parse(toml::de::Error),
    /// Values are syntactically valid but semantically wrong.
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cannot read config: {error}"),
            Self::Parse(error) => write!(f, "cannot parse config: {error}"),
            Self::Validation(message) => write!(f, "invalid config: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        Self::Parse(error)
    }
}

/// Capture backend choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaptureBackend {
    /// Pick automatically.
    #[default]
    Auto,
    /// evdev device reading.
    Evdev,
    /// X11 RECORD extension.
    Xrecord,
    /// XDG RemoteDesktop portal.
    Portal,
    /// Input method engine.
    Ime,
}

/// Injection backend choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InjectBackend {
    /// Pick automatically.
    #[default]
    Auto,
    /// X11 XTEST.
    Xtest,
    /// Wayland EIS/libei.
    Eis,
    /// wlroots virtual keyboard.
    VirtualKeyboard,
    /// Linux uinput.
    Uinput,
    /// Clipboard + paste.
    Clipboard,
}

/// Layout backend choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LayoutBackend {
    /// Pick automatically.
    #[default]
    Auto,
    /// KDE Plasma.
    Kde,
    /// GNOME.
    Gnome,
    /// Sway.
    Sway,
    /// Hyprland.
    Hyprland,
    /// Generic X11 XKB.
    X11,
}

/// Correction trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    /// Space.
    Space,
    /// Closing punctuation.
    Punct,
    /// Tab (aggressive only).
    Tab,
    /// Enter.
    Enter,
}

/// LLM mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LlmMode {
    /// Disabled.
    #[default]
    Off,
    /// Local llama.cpp model.
    Local,
}

/// Application mode, mapped to a core profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AppMode {
    /// Standard.
    #[default]
    Auto,
    /// Lower threshold and shorter words.
    Aggressive,
    /// Only obvious cases.
    Conservative,
    /// Manual only.
    Manual,
    /// Fully disabled.
    Off,
}

impl From<AppMode> for Profile {
    fn from(mode: AppMode) -> Self {
        match mode {
            AppMode::Auto => Self::Auto,
            AppMode::Aggressive => Self::Aggressive,
            AppMode::Conservative => Self::Conservative,
            AppMode::Manual => Self::Manual,
            AppMode::Off => Self::Off,
        }
    }
}

/// Layout code in the configured group order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutCode {
    /// English (US).
    Us,
    /// Russian.
    Ru,
}

impl From<LayoutCode> for LayoutId {
    fn from(code: LayoutCode) -> Self {
        match code {
            LayoutCode::Us => Self::Us,
            LayoutCode::Ru => Self::Ru,
        }
    }
}

/// `[general]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Master switch.
    pub enabled: bool,
    /// Configured languages.
    pub languages: Vec<String>,
    /// Correction triggers.
    pub triggers: Vec<Trigger>,
    /// Minimum word length.
    pub min_word_len: usize,
    /// Calibrated confidence threshold.
    pub confidence_threshold: f32,
    /// Undo hotkey string (not wired yet).
    pub undo_hotkey: String,
    /// Manual fix hotkey string (not wired yet).
    pub fix_hotkey: String,
    /// Pause hotkey string (not wired yet).
    pub pause_hotkey: String,
    /// Play a short sound when the active layout changes.
    pub sound_on_layout_change: bool,
    /// Sound file played on layout change.
    pub sound_file: String,
    /// Treat Backspace after a correction as undo.
    pub backspace_undo: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            languages: vec!["ru".to_owned(), "en".to_owned()],
            triggers: vec![Trigger::Space, Trigger::Enter],
            min_word_len: 3,
            confidence_threshold: 0.97,
            undo_hotkey: "Pause".to_owned(),
            fix_hotkey: "Ctrl+Shift+F".to_owned(),
            pause_hotkey: "Ctrl+Shift+P".to_owned(),
            sound_on_layout_change: true,
            sound_file: "/usr/share/sounds/freedesktop/stereo/camera-shutter.oga".to_owned(),
            backspace_undo: true,
        }
    }
}

/// `[capture]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CaptureConfig {
    /// Backend choice.
    pub backend: CaptureBackend,
}

/// `[privacy]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacyConfig {
    /// Must stay false in v1.
    pub store_keystrokes: bool,
    /// Learning enabled.
    pub learning: bool,
    /// History size, 0 means none.
    pub history_size: usize,
    /// Idle timeout.
    pub buffer_idle_timeout_ms: u64,
    /// Clear on focus change.
    pub clear_on_focus_change: bool,
    /// LLM mode.
    pub llm: LlmMode,
    /// Local model filename.
    pub llm_model: String,
    /// LLM timeout.
    pub llm_timeout_ms: u64,
    /// Async suggestions.
    pub llm_suggest: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            store_keystrokes: false,
            learning: true,
            history_size: 0,
            buffer_idle_timeout_ms: 60_000,
            clear_on_focus_change: true,
            llm: LlmMode::Off,
            llm_model: "qwen2.5-1.5b-instruct-q4_k_m.gguf".to_owned(),
            llm_timeout_ms: 300,
            llm_suggest: true,
        }
    }
}

/// `[inject]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InjectConfig {
    /// Backend choice.
    pub backend: InjectBackend,
    /// Allow uinput in games.
    pub allow_uinput_in_games: bool,
    /// Restore clipboard after paste.
    pub restore_clipboard: bool,
    /// Ask the compositor for input-injection permission at startup by
    /// sending a harmless Shift tap. Only used on Wayland.
    pub request_permission_on_start: bool,
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            backend: InjectBackend::Auto,
            allow_uinput_in_games: false,
            restore_clipboard: true,
            request_permission_on_start: true,
        }
    }
}

/// `[layout]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    /// Backend choice.
    pub backend: LayoutBackend,
    /// Per-window layout.
    pub per_window: bool,
    /// XKB group order, `Group1`, `Group2`, ...
    pub order: Vec<LayoutCode>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            backend: LayoutBackend::Auto,
            per_window: true,
            order: vec![LayoutCode::Us, LayoutCode::Ru],
        }
    }
}

/// `[ime]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImeConfig {
    /// Disable auto correction while an IME is active.
    pub disable_auto_when_active: bool,
}

impl Default for ImeConfig {
    fn default() -> Self {
        Self {
            disable_auto_when_active: true,
        }
    }
}

/// One `[apps."pattern"]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Profile mode.
    pub mode: AppMode,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mode: AppMode::Auto,
        }
    }
}

/// The complete configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `[general]`.
    pub general: GeneralConfig,
    /// `[capture]`.
    pub capture: CaptureConfig,
    /// `[privacy]`.
    pub privacy: PrivacyConfig,
    /// `[inject]`.
    pub inject: InjectConfig,
    /// `[layout]`.
    pub layout: LayoutConfig,
    /// `[ime]`.
    pub ime: ImeConfig,
    /// `[apps]`.
    #[serde(default = "default_apps")]
    pub apps: BTreeMap<String, AppConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            capture: CaptureConfig::default(),
            privacy: PrivacyConfig::default(),
            inject: InjectConfig::default(),
            layout: LayoutConfig::default(),
            ime: ImeConfig::default(),
            apps: default_apps(),
        }
    }
}

fn default_apps() -> BTreeMap<String, AppConfig> {
    let mut apps = BTreeMap::new();
    for pattern in ["*terminal*", "*konsole*", "*alacritty*", "*kitty*"] {
        apps.insert(
            pattern.to_owned(),
            AppConfig {
                mode: AppMode::Manual,
            },
        );
    }
    apps.insert("*keepass*".to_owned(), AppConfig { mode: AppMode::Off });
    apps.insert(
        "*".to_owned(),
        AppConfig {
            mode: AppMode::Auto,
        },
    );
    apps
}

impl Config {
    /// Parse a config from a TOML string and validate it.
    pub fn from_toml_str(text: &str) -> ConfigResult<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Read and validate a config file.
    pub fn load(path: impl AsRef<Path>) -> ConfigResult<Self> {
        let text = fs::read_to_string(path)?;
        Self::from_toml_str(&text)
    }

    /// Load the file if it exists, otherwise return defaults.
    pub fn load_or_default(path: impl AsRef<Path>) -> ConfigResult<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_toml_str(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Default config path: `$XDG_CONFIG_HOME/layoutpilot/config.toml` or
    /// `$HOME/.config/layoutpilot/config.toml`.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("layoutpilot/config.toml"));
            }
        }
        env::var("HOME")
            .ok()
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".config/layoutpilot/config.toml"))
    }

    /// Validate semantic constraints.
    pub fn validate(&self) -> ConfigResult<()> {
        if !self.general.enabled {
            // Disabled config is still valid.
        }
        if self.general.languages.is_empty() {
            return Err(ConfigError::Validation(
                "general.languages must not be empty".to_owned(),
            ));
        }
        if self.general.min_word_len == 0 || self.general.min_word_len > 64 {
            return Err(ConfigError::Validation(
                "general.min_word_len must be in 1..=64".to_owned(),
            ));
        }
        if !(0.0..=1.0).contains(&self.general.confidence_threshold)
            || !self.general.confidence_threshold.is_finite()
        {
            return Err(ConfigError::Validation(
                "general.confidence_threshold must be in [0, 1]".to_owned(),
            ));
        }
        if self.privacy.store_keystrokes {
            return Err(ConfigError::Validation(
                "privacy.store_keystrokes must be false in v1".to_owned(),
            ));
        }
        if self.privacy.llm_timeout_ms == 0 {
            return Err(ConfigError::Validation(
                "privacy.llm_timeout_ms must be positive".to_owned(),
            ));
        }
        if self.layout.order.len() < 2 || self.layout.order.len() > 4 {
            return Err(ConfigError::Validation(
                "layout.order must contain 2..=4 layouts".to_owned(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for code in &self.layout.order {
            if !seen.insert(*code) {
                return Err(ConfigError::Validation(
                    "layout.order contains duplicates".to_owned(),
                ));
            }
        }
        if self.apps.keys().any(|pattern| pattern.is_empty()) {
            return Err(ConfigError::Validation(
                "apps patterns must not be empty".to_owned(),
            ));
        }
        for (name, value) in [
            ("general.undo_hotkey", &self.general.undo_hotkey),
            ("general.fix_hotkey", &self.general.fix_hotkey),
            ("general.pause_hotkey", &self.general.pause_hotkey),
        ] {
            if value.trim().is_empty() {
                continue;
            }
            Hotkey::parse(value)
                .map_err(|error| ConfigError::Validation(format!("{name} is invalid: {error}")))?;
        }
        Ok(())
    }

    /// Layout order for the platform backend.
    #[must_use]
    pub fn layout_order(&self) -> Vec<LayoutId> {
        self.layout
            .order
            .iter()
            .copied()
            .map(LayoutId::from)
            .collect()
    }

    /// Convert to the engine config.
    #[must_use]
    pub fn to_engine_config(&self) -> EngineConfig {
        EngineConfig {
            min_word_len: self.general.min_word_len,
            confidence_threshold: self.general.confidence_threshold,
            tab_trigger_in_aggressive: self.general.triggers.contains(&Trigger::Tab),
            space_trigger: self.general.triggers.contains(&Trigger::Space),
            punct_trigger: self.general.triggers.contains(&Trigger::Punct),
            enter_trigger: self.general.triggers.contains(&Trigger::Enter),
            ..EngineConfig::default()
        }
    }

    /// Build a profile resolver from `[apps]`.
    ///
    /// `*` becomes the fallback; exact names and globs become rules. If `*` is
    /// absent, the fallback is `auto`.
    #[must_use]
    pub fn to_profile_resolver(&self) -> ProfileResolver {
        let fallback = self
            .apps
            .get("*")
            .map_or(Profile::Auto, |app| app.mode.into());
        let mut resolver = ProfileResolver::new(fallback);
        for (pattern, app) in &self.apps {
            if pattern != "*" {
                resolver.push_rule(AppRule::new(pattern.clone(), app.mode.into()));
            }
        }
        resolver
    }

    /// Serialize to TOML, useful for generating a default file.
    pub fn to_toml(&self) -> ConfigResult<String> {
        toml::to_string_pretty(self).map_err(|error| ConfigError::Validation(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layoutpilot_core::Profile;

    #[test]
    fn defaults_match_the_spec() {
        let config = Config::default();
        assert!(config.general.enabled);
        assert_eq!(config.general.languages, vec!["ru", "en"]);
        assert_eq!(
            config.general.triggers,
            vec![Trigger::Space, Trigger::Enter]
        );
        assert_eq!(config.general.min_word_len, 3);
        assert!((config.general.confidence_threshold - 0.97).abs() < f32::EPSILON);
        assert_eq!(config.capture.backend, CaptureBackend::Auto);
        assert_eq!(config.inject.backend, InjectBackend::Auto);
        assert!(config.inject.request_permission_on_start);
        assert!(config.general.sound_on_layout_change);
        assert!(config.general.sound_file.contains("camera-shutter"));
        assert!(config.general.backspace_undo);
        assert_eq!(config.layout.backend, LayoutBackend::Auto);
        assert_eq!(config.layout.order, vec![LayoutCode::Us, LayoutCode::Ru]);
        assert!(!config.privacy.store_keystrokes);
        config.validate().unwrap();
    }

    #[test]
    fn parses_the_tz_example() {
        let config = Config::from_toml_str(
            r#"
[general]
min_word_len = 4
confidence_threshold = 0.9
triggers = ["space", "punct", "tab"]

[capture]
backend = "evdev"

[inject]
backend = "virtual-keyboard"
allow_uinput_in_games = false

[layout]
order = ["ru", "us"]

[apps."org.telegram.desktop"]
mode = "aggressive"

[apps."*terminal*"]
mode = "manual"

[apps."*"]
mode = "auto"
"#,
        )
        .unwrap();

        assert_eq!(config.general.min_word_len, 4);
        assert_eq!(config.capture.backend, CaptureBackend::Evdev);
        assert_eq!(config.inject.backend, InjectBackend::VirtualKeyboard);
        assert_eq!(config.layout_order(), vec![LayoutId::Ru, LayoutId::Us]);
        let engine = config.to_engine_config();
        assert!(engine.tab_trigger_in_aggressive);
        assert!(engine.space_trigger);
        assert!(engine.punct_trigger);

        let resolver = config.to_profile_resolver();
        assert_eq!(
            resolver.resolve("org.telegram.desktop"),
            Profile::Aggressive
        );
        assert_eq!(resolver.resolve("org.gnome.Terminal"), Profile::Manual);
        assert_eq!(resolver.resolve("firefox"), Profile::Auto);
    }

    #[test]
    fn rejects_invalid_values() {
        let mut config = Config::default();
        config.general.confidence_threshold = 1.5;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.general.min_word_len = 0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.privacy.store_keystrokes = true;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.layout.order = vec![LayoutCode::Us];
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.layout.order = vec![LayoutCode::Us, LayoutCode::Us];
        assert!(config.validate().is_err());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let config = Config::from_toml_str(
            r#"
[general]
min_word_len = 3
future_option = true
"#,
        )
        .unwrap();
        assert_eq!(config.general.min_word_len, 3);
    }

    #[test]
    fn default_path_uses_xdg() {
        // This only checks shape; the environment may or may not have HOME.
        if let Some(path) = Config::default_path() {
            assert!(path.ends_with("layoutpilot/config.toml"));
        }
    }

    #[test]
    fn rejects_invalid_hotkeys() {
        let mut config = Config::default();
        config.general.fix_hotkey = "Ctrl+NoSuchKey".to_owned();
        assert!(config.validate().is_err());

        config.general.fix_hotkey = "Ctrl+Shift+F".to_owned();
        config.general.undo_hotkey.clear();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../config.example.toml");
        let config = Config::from_toml_str(text).unwrap();
        assert_eq!(config.capture.backend, CaptureBackend::Auto);
        assert_eq!(config.layout.order, vec![LayoutCode::Us, LayoutCode::Ru]);
    }

    #[test]
    fn toml_round_trip() {
        let config = Config::default();
        let text = config.to_toml().unwrap();
        let parsed = Config::from_toml_str(&text).unwrap();
        assert_eq!(config, parsed);
    }
}
