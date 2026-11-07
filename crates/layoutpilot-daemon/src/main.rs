//! `layoutpilot` daemon.
//!
//! Data flow: XRecord/evdev key events → layout lookup → character
//! normalization → `CorrectionManager` (engine + dictionaries) → injection
//! (XTest on X11, uinput on Wayland) with a compositor layout switch.
//!
//! The daemon loads `~/.config/layoutpilot/config.toml` (or `--config PATH`)
//! and falls back to built-in defaults.

mod kde_focus;
mod learned;
mod screen_lock;
mod selection;
mod tail;
mod tray;

use std::collections::{HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::time::{Duration, Instant};

use layoutpilot_capture::{DeviceChange, EvdevCapture, EvdevKeyEvent};
use layoutpilot_compositor::{HyprlandLayouts, KdeLayouts, SwayLayouts};
use layoutpilot_config::{
    CaptureBackend, Config, DeleteMode, Hotkey, InjectBackend, LayoutBackend, Trigger,
};
use layoutpilot_core::{
    CorrectionManager, CorrectionPlan, InputContext, LayoutId, LayoutTable, PlanKind, Profile,
    ProfileResolver, ResetReason,
};
use layoutpilot_dict::{DictionaryDetector, Language};
use layoutpilot_inject::UinputInjector;
use layoutpilot_x11::{
    char_for, modifiers, KeyEvent, X11Focus, X11HotkeyGrabber, X11Injector, X11LayoutState,
    XRecordCapture,
};
use screen_lock::ScreenLock;
use selection::SelectionWatch;
use tail::{tail_text, Blocked, PendingKey};
use tray::{TrayCommand, TrayHandle, TrayState};

const BACKSPACE_KEYCODE: u8 = 22;
const RETURN_KEYCODE: u8 = 36;
const TAB_KEYCODE: u8 = 23;
const SPACE_KEYCODE: u8 = 65;
const ESCAPE_KEYCODE: u8 = 9;
const IGNORE_WINDOW: Duration = Duration::from_millis(750);
/// How long a correction waits for a physical modifier to be released.
/// Modifiers are seat-wide on Wayland: a held Shift or Ctrl would combine with
/// the synthetic Backspace/letters.
const MODIFIER_RELEASE_WAIT: Duration = Duration::from_millis(300);
/// How long injection waits for physically held keys to be released.
///
/// The compositor ignores a press of a key that is already down on the
/// seat. An early-trigger correction fires on the press of the word's last
/// key, and the replacement ends with a letter on that same key (`ghbd` →
/// `прив`, `d` = `в`): typed while the physical key is still held, that
/// letter was lost (`Приет`).
const KEY_RELEASE_WAIT: Duration = Duration::from_millis(300);
/// Capture threads poll devices every few ms; give them time to deliver keys
/// pressed during injection before checking for them.
const CAPTURE_LAG: Duration = Duration::from_millis(10);
/// Repair passes for keys typed during injection before giving up.
const MAX_REPAIRS: usize = 3;
/// A layout change this soon after a window activation comes from per-window
/// layouts, not from the user: no sound.
const FOCUS_LAYOUT_WINDOW: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum HotkeyAction {
    Undo,
    Fix,
    Pause,
}

#[derive(Debug)]
struct Hotkeys {
    undo: Option<Hotkey>,
    fix: Option<Hotkey>,
    pause: Option<Hotkey>,
}

impl Hotkeys {
    fn from_config(config: &Config) -> Result<Self, Box<dyn Error>> {
        fn parse(value: &str) -> Result<Option<Hotkey>, Box<dyn Error>> {
            if value.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(Hotkey::parse(value)?))
            }
        }
        Ok(Self {
            undo: parse(&config.general.undo_hotkey)?,
            fix: parse(&config.general.fix_hotkey)?,
            pause: parse(&config.general.pause_hotkey)?,
        })
    }

    fn action(&self, keycode: u16, state: u16) -> Option<HotkeyAction> {
        if self
            .pause
            .is_some_and(|hotkey| hotkey.matches(keycode, state))
        {
            Some(HotkeyAction::Pause)
        } else if self
            .undo
            .is_some_and(|hotkey| hotkey.matches(keycode, state))
        {
            Some(HotkeyAction::Undo)
        } else if self
            .fix
            .is_some_and(|hotkey| hotkey.matches(keycode, state))
        {
            Some(HotkeyAction::Fix)
        } else {
            None
        }
    }
}

/// Longest selection the gesture converts.
const MAX_SELECTION_CHARS: usize = 10_000;
/// Longest pasted fragment that is selected again with Shift+Left.
const MAX_RESELECT_CHARS: usize = 2_000;
/// Time the application gets to read the clipboard after Ctrl+V.
const PASTE_SETTLE: Duration = Duration::from_millis(300);

/// A selection conversion that can still be toggled back.
#[derive(Debug, Clone)]
struct SelectionFix {
    /// Text before the last paste.
    original: String,
    /// Text now in the document.
    converted: String,
    /// Layout of `original`.
    source: LayoutId,
    /// Layout of `converted`.
    target: LayoutId,
}

impl SelectionFix {
    fn reversed(self) -> Self {
        Self {
            original: self.converted,
            converted: self.original,
            source: self.target,
            target: self.source,
        }
    }
}

/// XKB keycodes for left and right Shift.
const SHIFT_KEYCODES: [u8; 2] = [50, 62];

/// What a completed Shift sequence should fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShiftGestureAction {
    /// Double Shift: the last word.
    Word,
    /// Triple Shift: the whole current phrase.
    Phrase,
}

/// Double Shift fixes the last word, triple Shift the phrase.
///
/// Any other key breaks the sequence. The action fires on release so that no
/// synthetic character is typed while a physical Shift is still held.
#[derive(Debug)]
struct ShiftGesture {
    count: u8,
    last_press: Option<Instant>,
    broken: bool,
    window: Duration,
}

impl Default for ShiftGesture {
    fn default() -> Self {
        Self {
            count: 0,
            last_press: None,
            broken: false,
            window: Duration::from_millis(400),
        }
    }
}

impl ShiftGesture {
    fn reset(&mut self) {
        self.count = 0;
        self.last_press = None;
        self.broken = false;
    }

    /// Feed one key event; returns an action when a sequence is released.
    fn on_key(&mut self, keycode: u8, pressed: bool, now: Instant) -> Option<ShiftGestureAction> {
        if SHIFT_KEYCODES.contains(&keycode) {
            if pressed {
                self.count = if self
                    .last_press
                    .is_some_and(|last| now.duration_since(last) <= self.window)
                {
                    self.count.saturating_add(1)
                } else {
                    1
                };
                self.last_press = Some(now);
                self.broken = false;
            } else if self.broken {
                self.reset();
            } else if self.count == 2 {
                // Keep the counter: a third quick tap upgrades the gesture to
                // a phrase. A new sequence after the window starts at 1.
                return Some(ShiftGestureAction::Word);
            } else if self.count >= 3 {
                self.reset();
                return Some(ShiftGestureAction::Phrase);
            }
            return None;
        }
        if pressed {
            self.reset();
            self.broken = true;
        }
        None
    }
}

const SHIFT_MASK: u16 = 1;
const LOCK_MASK: u16 = 2;
const CONTROL_MASK: u16 = 4;
const ALT_MASK: u16 = 8;
const SUPER_MASK: u16 = 64;

const EVDEV_LEFTSHIFT: u16 = 42;
const EVDEV_RIGHTSHIFT: u16 = 54;
const EVDEV_LEFTCTRL: u16 = 29;
const EVDEV_RIGHTCTRL: u16 = 97;
const EVDEV_LEFTALT: u16 = 56;
const EVDEV_RIGHTALT: u16 = 100;
const EVDEV_LEFTMETA: u16 = 125;
const EVDEV_RIGHTMETA: u16 = 126;
const EVDEV_CAPSLOCK: u16 = 58;

#[derive(Debug)]
struct SoundPlayer {
    enabled: bool,
    failed: bool,
    file: PathBuf,
}

impl SoundPlayer {
    fn new(enabled: bool, file: PathBuf) -> Self {
        Self {
            enabled,
            failed: false,
            file,
        }
    }

    fn play(&mut self) {
        if !self.enabled || self.failed {
            return;
        }
        let Some(sound) = self.file.to_str() else {
            self.failed = true;
            eprintln!("layoutpilot: sound file path is not UTF-8; disabling layout sound");
            return;
        };
        if self.file.is_file() && self.spawn("paplay", &[sound]) {
            return;
        }
        if self.file.is_file() && self.spawn("pw-play", &[sound]) {
            return;
        }
        if self.spawn("canberra-gtk-play", &["--id=bell"]) {
            return;
        }
        self.failed = true;
        eprintln!("layoutpilot: sound player unavailable; disabling layout sound");
    }

    fn spawn(&self, program: &str, args: &[&str]) -> bool {
        let mut command = ProcessCommand::new(program);
        command
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(mut child) => {
                println!("layoutpilot: layout sound via {program}");
                std::thread::spawn(move || {
                    if let Ok(status) = child.wait() {
                        if !status.success() {
                            eprintln!("layoutpilot: sound player exited with {status}");
                        }
                    }
                });
                true
            }
            Err(_) => false,
        }
    }
}

enum LayoutController {
    X11(X11LayoutState),
    Kde {
        kde: KdeLayouts,
        xkb: Option<X11LayoutState>,
    },
    Sway(SwayLayouts),
    Hyprland(HyprlandLayouts),
}

impl LayoutController {
    fn name(&self) -> &'static str {
        match self {
            Self::X11(_) => "x11-xkb",
            Self::Kde { .. } => "kde-dbus+xkb",
            Self::Sway(_) => "sway-ipc",
            Self::Hyprland(_) => "hyprland-ipc",
        }
    }

    fn active_layout(&self) -> Result<Option<LayoutId>, Box<dyn Error>> {
        match self {
            Self::X11(state) => Ok(state.active_layout()?),
            Self::Kde { kde, .. } => Ok(kde.current()?),
            Self::Sway(sway) => Ok(sway.current()?),
            Self::Hyprland(hyprland) => Ok(hyprland.current()?),
        }
    }

    fn switch_to(&mut self, layout: LayoutId) -> Result<(), Box<dyn Error>> {
        match self {
            Self::X11(state) => {
                state.switch_to(layout)?;
            }
            Self::Kde { kde, xkb } => {
                kde.switch_to(layout)?;
                if let Some(xkb) = xkb {
                    if let Err(error) = xkb.switch_to(layout) {
                        eprintln!("layoutpilot: cannot sync XKB group: {error}");
                    }
                }
            }
            Self::Sway(sway) => sway.switch_to(layout)?,
            Self::Hyprland(hyprland) => hyprland.switch_to(layout)?,
        }
        Ok(())
    }

    fn supports(&self, layout: LayoutId) -> bool {
        match self {
            Self::X11(state) => state.layouts().contains(&layout),
            Self::Kde { kde, .. } => kde.supports(layout),
            Self::Sway(sway) => sway.supports(layout),
            Self::Hyprland(hyprland) => hyprland.supports(layout),
        }
    }

    fn always_switch(&self) -> bool {
        // Sway/Hyprland keep a layout per keyboard; our uinput device may have
        // stayed in another group even when the tracked layout looks unchanged.
        matches!(self, Self::Sway(_) | Self::Hyprland(_))
    }

    fn sync_xkb(&self, layout: LayoutId) {
        if let Self::Kde { xkb: Some(xkb), .. } = self {
            if let Err(error) = xkb.switch_to(layout) {
                eprintln!("layoutpilot: cannot sync XKB group: {error}");
            }
        }
    }
}

/// Injection backend selected at startup.
enum Injector {
    X11(Box<X11Injector>),
    Uinput(UinputInjector),
}

impl Injector {
    fn name(&self) -> &'static str {
        match self {
            Self::X11(_) => "xtest",
            Self::Uinput(_) => "uinput",
        }
    }

    fn tap_backspace(&mut self, count: usize) -> Result<(), Box<dyn Error>> {
        match self {
            Self::X11(injector) => Ok(injector.tap_backspace(count)?),
            Self::Uinput(injector) => Ok(injector.tap_backspace(count)?),
        }
    }

    fn tap_ctrl_backspace(&mut self) -> Result<(), Box<dyn Error>> {
        match self {
            Self::X11(injector) => Ok(injector.tap_ctrl_backspace()?),
            Self::Uinput(injector) => Ok(injector.tap_ctrl_backspace()?),
        }
    }

    fn select_left(&mut self, count: usize) -> Result<(), Box<dyn Error>> {
        match self {
            Self::X11(injector) => {
                // XKB keycodes: Shift_L 50, Left 113.
                injector.press_keycode(50)?;
                for _ in 0..count {
                    injector.press_keycode(113)?;
                    injector.release_keycode(113)?;
                }
                injector.release_keycode(50)?;
                Ok(())
            }
            Self::Uinput(injector) => Ok(injector.select_left(count)?),
        }
    }

    fn tap_paste(&mut self) -> Result<(), Box<dyn Error>> {
        match self {
            Self::X11(injector) => {
                // XKB keycodes: Control_L 37, V 55.
                injector.press_keycode(37)?;
                injector.press_keycode(55)?;
                injector.release_keycode(55)?;
                injector.release_keycode(37)?;
                Ok(())
            }
            Self::Uinput(injector) => Ok(injector.tap_paste()?),
        }
    }

    fn tap_text(&mut self, text: &str, layout: &LayoutTable) -> Result<(), Box<dyn Error>> {
        match self {
            Self::X11(injector) => Ok(injector.tap_text(text, layout)?),
            Self::Uinput(injector) => Ok(injector.tap_text(text, layout)?),
        }
    }

    fn request_input_permission(&mut self) -> Result<(), Box<dyn Error>> {
        match self {
            Self::X11(injector) => Ok(injector.request_input_permission()?),
            Self::Uinput(injector) => Ok(injector.request_input_permission()?),
        }
    }

    fn take_sent_events(&mut self) -> Vec<(u8, bool)> {
        match self {
            Self::X11(injector) => injector.take_sent_events(),
            // uinput events are filtered by device name, not by keycode:
            // putting them in the XTest journal could swallow a real key with
            // the same code pressed shortly after the correction.
            Self::Uinput(_) => Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
struct EvdevModifiers {
    pressed: HashSet<u16>,
    caps_lock: bool,
}

impl EvdevModifiers {
    fn update(&mut self, evdev_keycode: u16, value: i32) -> bool {
        if evdev_keycode == EVDEV_CAPSLOCK {
            if value == 1 {
                self.caps_lock = !self.caps_lock;
            }
            return true;
        }
        if is_modifier(evdev_keycode) {
            if value == 0 {
                self.pressed.remove(&evdev_keycode);
            } else {
                self.pressed.insert(evdev_keycode);
            }
            return true;
        }
        false
    }

    /// Forget held keys, e.g. when their keyboard was unplugged mid-press.
    fn release_all(&mut self) {
        self.pressed.clear();
    }

    fn any_held(&self) -> bool {
        !self.pressed.is_empty()
    }

    fn state(&self) -> u16 {
        let mut state = 0;
        if self.pressed.contains(&EVDEV_LEFTSHIFT) || self.pressed.contains(&EVDEV_RIGHTSHIFT) {
            state |= SHIFT_MASK;
        }
        if self.pressed.contains(&EVDEV_LEFTCTRL) || self.pressed.contains(&EVDEV_RIGHTCTRL) {
            state |= CONTROL_MASK;
        }
        if self.pressed.contains(&EVDEV_LEFTALT) || self.pressed.contains(&EVDEV_RIGHTALT) {
            state |= ALT_MASK;
        }
        if self.pressed.contains(&EVDEV_LEFTMETA) || self.pressed.contains(&EVDEV_RIGHTMETA) {
            state |= SUPER_MASK;
        }
        if self.caps_lock {
            state |= LOCK_MASK;
        }
        state
    }
}

fn is_modifier(evdev_keycode: u16) -> bool {
    matches!(
        evdev_keycode,
        EVDEV_LEFTSHIFT
            | EVDEV_RIGHTSHIFT
            | EVDEV_LEFTCTRL
            | EVDEV_RIGHTCTRL
            | EVDEV_LEFTALT
            | EVDEV_RIGHTALT
            | EVDEV_LEFTMETA
            | EVDEV_RIGHTMETA
    )
}

enum Capture {
    X11(XRecordCapture),
    Evdev(EvdevCapture),
}

enum CapturedEvent {
    X11(KeyEvent),
    Evdev(EvdevKeyEvent),
}

impl Capture {
    fn backend_name(&self) -> &'static str {
        match self {
            Self::X11(_) => "xrecord",
            Self::Evdev(_) => "evdev",
        }
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<CapturedEvent>, Box<dyn Error>> {
        match self {
            Self::X11(capture) => Ok(capture.recv_timeout(timeout)?.map(CapturedEvent::X11)),
            Self::Evdev(capture) => Ok(capture.recv_timeout(timeout)?.map(CapturedEvent::Evdev)),
        }
    }

    fn take_device_changes(&self) -> Vec<DeviceChange> {
        match self {
            Self::X11(_) => Vec::new(),
            Self::Evdev(capture) => capture.take_device_changes(),
        }
    }

    fn try_recv(&mut self) -> Result<Option<CapturedEvent>, Box<dyn Error>> {
        self.recv_timeout(Duration::ZERO)
    }

    fn stop(&mut self) {
        match self {
            Self::X11(capture) => {
                let _ = capture.stop();
            }
            Self::Evdev(capture) => capture.stop(),
        }
    }
}

struct Daemon {
    config: Config,
    capture: Capture,
    injector: Injector,
    layouts: LayoutController,
    sound: SoundPlayer,
    focus: X11Focus,
    kde_focus: Option<kde_focus::KdeFocus>,
    manager: CorrectionManager<DictionaryDetector>,
    resolver: ProfileResolver,
    hotkeys: Hotkeys,
    hotkey_grabber: Option<X11HotkeyGrabber>,
    grabbed_hotkeys: HashSet<HotkeyAction>,
    last_hotkey: Option<(HotkeyAction, Instant)>,
    paused: bool,
    active_layout: Option<LayoutId>,
    profile: Profile,
    app_id: Option<String>,
    window: Option<u32>,
    last_focus_poll: Instant,
    last_input: Instant,
    idle_timeout: Duration,
    clear_on_focus_change: bool,
    ignore: VecDeque<(u8, bool)>,
    ignore_until: Instant,
    /// Physical keys taken out of capture by a correction (its tail) and not
    /// yet fed to the engine. Always processed before new capture events.
    backlog: VecDeque<PendingKey>,
    pending_enter: bool,
    pending_space: bool,
    enter_ignore_until: Instant,
    evdev_modifiers: EvdevModifiers,
    /// Physically held non-modifier keys (XKB keycodes).
    held_keys: HashSet<u16>,
    shift_gesture: ShiftGesture,
    last_word_fix_at: Option<Instant>,
    /// Primary selection watcher, when selection conversion is available.
    selection: Option<SelectionWatch>,
    /// Selection generation at the last key press or click; a higher current
    /// generation means the user selected text after that.
    selection_seen: u64,
    /// Last selection conversion, while nothing was typed after it.
    selection_fix: Option<SelectionFix>,
    last_selection_fix_at: Option<Instant>,
    tray: Option<TrayHandle>,
    /// Runtime override from the tray: `manual` apps (terminals) become `auto`.
    terminals_auto: bool,
    focus_changed_at: Option<Instant>,
    /// Learning journal, when `privacy.learning` is on.
    learned_path: Option<PathBuf>,
    screen_lock: Option<ScreenLock>,
    /// Screen is locked: input is not processed at all.
    locked: bool,
    dry_run: bool,
}

impl Daemon {
    fn new(dry_run: bool, config: Config) -> Result<Self, Box<dyn Error>> {
        let mut injector = open_injector(&config)?;
        if config.inject.request_permission_on_start && !dry_run && is_wayland_session() {
            println!("layoutpilot: requesting input-injection permission (Shift tap)");
            if let Err(error) = injector.request_input_permission() {
                eprintln!("layoutpilot: permission request failed: {error}");
            }
        }
        let capture = open_capture(&config)?;
        let layouts = open_layouts(&config)?;
        let focus = X11Focus::new()?;
        let active_layout = layouts.active_layout().ok().flatten();
        let sound = SoundPlayer::new(
            config.general.sound_on_layout_change,
            PathBuf::from(&config.general.sound_file),
        );
        let hotkeys = Hotkeys::from_config(&config)?;
        let (hotkey_grabber, grabbed_hotkeys) = setup_hotkey_grabber(&hotkeys);
        let resolver = config.to_profile_resolver();
        let profile = resolver.resolve("");
        let kde_focus = if is_kde_session() && is_wayland_session() {
            match kde_focus::KdeFocus::start() {
                Ok(focus) => {
                    println!("layoutpilot: KDE focus via KWin script");
                    Some(focus)
                }
                Err(error) => {
                    eprintln!("layoutpilot: KDE focus unavailable: {error}");
                    None
                }
            }
        } else {
            None
        };
        let idle_timeout = Duration::from_millis(config.privacy.buffer_idle_timeout_ms.max(1));
        let clear_on_focus_change = config.privacy.clear_on_focus_change;

        let mut daemon = Self {
            manager: CorrectionManager::new(config.to_engine_config(), DictionaryDetector::new()),
            hotkeys,
            hotkey_grabber,
            grabbed_hotkeys,
            last_hotkey: None,
            paused: false,
            config,
            capture,
            injector,
            layouts,
            sound,
            focus,
            kde_focus,
            resolver,
            active_layout,
            profile,
            app_id: None,
            window: None,
            last_focus_poll: Instant::now() - Duration::from_secs(2),
            last_input: Instant::now(),
            idle_timeout,
            clear_on_focus_change,
            ignore: VecDeque::new(),
            ignore_until: Instant::now(),
            backlog: VecDeque::new(),
            pending_enter: false,
            pending_space: false,
            enter_ignore_until: Instant::now(),
            evdev_modifiers: EvdevModifiers::default(),
            held_keys: HashSet::new(),
            shift_gesture: ShiftGesture::default(),
            last_word_fix_at: None,
            selection: None,
            selection_seen: 0,
            selection_fix: None,
            last_selection_fix_at: None,
            tray: None,
            terminals_auto: false,
            focus_changed_at: None,
            learned_path: None,
            screen_lock: None,
            locked: false,
            dry_run,
        };
        match ScreenLock::start() {
            Ok(lock) => daemon.screen_lock = Some(lock),
            Err(error) => eprintln!("layoutpilot: screen lock state unavailable: {error}"),
        }
        if daemon.config.privacy.learning {
            daemon.load_learning();
        }
        if daemon.config.general.double_shift_selection
            && daemon.config.general.double_shift_fix
            && is_wayland_session()
        {
            match SelectionWatch::start() {
                Ok(watch) => {
                    println!("layoutpilot: selection conversion via wl-paste");
                    daemon.selection = Some(watch);
                }
                Err(error) => {
                    eprintln!("layoutpilot: selection conversion unavailable (wl-clipboard): {error}");
                }
            }
        }
        if daemon.config.general.tray {
            let state = TrayState {
                pause_hotkey: daemon.config.general.pause_hotkey.clone(),
                ..TrayState::default()
            };
            match TrayHandle::spawn(state) {
                Ok(tray) => daemon.tray = Some(tray),
                Err(error) => eprintln!("layoutpilot: tray icon unavailable: {error}"),
            }
        }
        daemon.refresh_focus(true);
        Ok(daemon)
    }

    fn run(mut self) -> Result<(), Box<dyn Error>> {
        println!(
            "layoutpilot: capture={} ({}), inject={}, layout={} ({:?}), dry_run={}, app={:?}",
            self.capture.backend_name(),
            config_backend_name(self.config.capture.backend),
            self.injector.name(),
            self.layouts.name(),
            self.active_layout,
            self.dry_run,
            self.app_id
        );
        loop {
            self.poll_hotkeys()?;
            if !self.poll_tray() {
                println!("layoutpilot: quit from tray");
                break;
            }
            if Instant::now() >= self.ignore_until && !self.ignore.is_empty() {
                self.ignore.clear();
            }

            self.persist_learning();
            self.poll_devices();
            self.poll_screen_lock();
            if let Some(key) = self.backlog.pop_front() {
                self.dispatch(key)?;
                continue;
            }

            match self.capture.recv_timeout(Duration::from_millis(50)) {
                Ok(Some(event)) => {
                    self.refresh_layout();
                    self.refresh_focus(false);
                    self.handle_captured(event)?;
                }
                Ok(None) => {
                    self.refresh_layout();
                    self.refresh_focus(false);
                }
                Err(error) => {
                    eprintln!("layoutpilot: capture error: {error}");
                    break;
                }
            }

            if self.last_input.elapsed() >= self.idle_timeout {
                let engine = self.manager.engine();
                if !engine.buffer_text().is_empty() || engine.undo_available() {
                    self.manager.reset(ResetReason::IdleTimeout);
                }
                self.last_input = Instant::now();
            }
        }
        Ok(())
    }

    fn handle_captured(&mut self, event: CapturedEvent) -> Result<(), Box<dyn Error>> {
        // Normalize even while locked so modifier state stays accurate.
        match self.normalize(event) {
            Some(key) if !self.locked => self.dispatch(key),
            _ => Ok(()),
        }
    }

    fn poll_devices(&mut self) {
        for change in self.capture.take_device_changes() {
            match change {
                DeviceChange::Added(info) => {
                    println!("layoutpilot: keyboard added: {}", device_label(&info));
                }
                DeviceChange::Removed(info) => {
                    println!("layoutpilot: keyboard removed: {}", device_label(&info));
                    self.evdev_modifiers.release_all();
                    self.held_keys.clear();
                }
            }
            self.manager.reset(ResetReason::DeviceChange);
        }
    }

    fn poll_screen_lock(&mut self) {
        let locked = self.screen_lock.as_ref().is_some_and(ScreenLock::is_locked);
        if locked == self.locked {
            return;
        }
        self.locked = locked;
        if locked {
            self.manager.reset(ResetReason::ScreenLock);
            self.backlog.clear();
            self.pending_space = false;
            self.pending_enter = false;
            self.shift_gesture.reset();
            println!("layoutpilot: screen locked, input ignored");
        } else {
            println!("layoutpilot: screen unlocked");
        }
    }

    /// Filter own events and resolve modifier state. Must be called exactly
    /// once per captured event, in capture order.
    fn normalize(&mut self, event: CapturedEvent) -> Option<PendingKey> {
        match event {
            CapturedEvent::X11(event) => {
                if self.consume_own(&event) {
                    return None;
                }
                // X11 events carry their own state; track held modifiers
                // only so that injection can wait for their release.
                if let Some(evdev_keycode) = u16::from(event.keycode).checked_sub(8) {
                    let modifier = self
                        .evdev_modifiers
                        .update(evdev_keycode, i32::from(event.pressed));
                    if !modifier {
                        self.track_held(u16::from(event.keycode), event.pressed);
                    }
                }
                Some(PendingKey {
                    event,
                    evdev_modifier: false,
                })
            }
            CapturedEvent::Evdev(event) => {
                if event.device == layoutpilot_core::VIRTUAL_KEYBOARD_NAME {
                    // Own uinput output. Capture normally filters the device
                    // out, but this also protects against a device opened
                    // before injection.
                    return None;
                }
                if layoutpilot_capture::is_pointer_button(event.keycode) {
                    if event.pressed() {
                        self.on_pointer_press();
                    }
                    return None;
                }
                let evdev_keycode = event.keycode.checked_sub(8)?;
                let modifier = self.evdev_modifiers.update(evdev_keycode, event.value);
                if !modifier {
                    self.track_held(event.keycode, event.pressed());
                }
                let keycode = u8::try_from(event.keycode).ok()?;
                Some(PendingKey {
                    event: KeyEvent {
                        keycode,
                        state: self.evdev_modifiers.state(),
                        time: 0,
                        pressed: event.pressed(),
                    },
                    evdev_modifier: modifier,
                })
            }
        }
    }

    fn dispatch(&mut self, key: PendingKey) -> Result<(), Box<dyn Error>> {
        if !key.evdev_modifier {
            return self.handle_key(key.event);
        }
        // Modifiers never reach handle_key, so feed the Shift gesture here.
        if self.paused {
            self.shift_gesture.reset();
        } else if self.config.general.double_shift_fix {
            if let Some(action) =
                self.shift_gesture
                    .on_key(key.event.keycode, key.event.pressed, Instant::now())
            {
                self.handle_shift_gesture(action, &key.event)?;
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, event: KeyEvent) -> Result<(), Box<dyn Error>> {
        if event.keycode == RETURN_KEYCODE
            && event.pressed
            && Instant::now() < self.enter_ignore_until
        {
            return Ok(());
        }
        if !event.pressed {
            if self.config.general.double_shift_fix && !self.paused {
                if let Some(action) =
                    self.shift_gesture
                        .on_key(event.keycode, false, Instant::now())
                {
                    self.handle_shift_gesture(action, &event)?;
                    return Ok(());
                }
            }
            if event.keycode == RETURN_KEYCODE && self.pending_enter {
                self.pending_enter = false;
                self.handle_char('\n', &event)?;
            } else if event.keycode == SPACE_KEYCODE && self.pending_space {
                // Trigger on release: by now the application has processed
                // the separator, so the plan's trailing-space Backspace hits
                // the real space instead of the previous word's separator.
                self.pending_space = false;
                self.handle_char(' ', &event)?;
            }
            return Ok(());
        }
        self.last_input = Instant::now();
        // X11 capture delivers Shift here too; it must not end a selection.
        if !SHIFT_KEYCODES.contains(&event.keycode) {
            self.mark_selection_seen();
            self.selection_fix = None;
        }

        if let Some(action) = self.hotkeys.action(u16::from(event.keycode), event.state) {
            if !self.grabbed_hotkeys.contains(&action) {
                self.handle_hotkey(action, &event)?;
            }
            return Ok(());
        }
        if self.paused {
            self.shift_gesture.reset();
            return Ok(());
        }

        if self.pending_space && event.keycode != SPACE_KEYCODE {
            // A key arrived before the space release. It has already reached
            // the application after the space, so it becomes the first key of
            // the correction's tail and is processed afterwards.
            self.pending_space = false;
            self.backlog.push_front(PendingKey {
                event,
                evdev_modifier: false,
            });
            return self.handle_char(' ', &event);
        }

        if self.config.general.double_shift_fix {
            let _ = self
                .shift_gesture
                .on_key(event.keycode, true, Instant::now());
        }
        if event.keycode == SPACE_KEYCODE && self.config.general.triggers.contains(&Trigger::Space)
        {
            self.pending_space = true;
            return Ok(());
        }

        match event.keycode {
            BACKSPACE_KEYCODE => {
                if self.config.general.backspace_undo && self.manager.undo_applied() {
                    if let Some(plan) = self.manager.undo() {
                        self.apply(plan)?;
                        return Ok(());
                    }
                }
                self.manager.on_backspace();
                return Ok(());
            }
            RETURN_KEYCODE => {
                if self.config.general.triggers.contains(&Trigger::Enter) {
                    // Wait for the physical Enter release before injecting:
                    // injecting while Enter is held makes the final newline
                    // unreliable and can desynchronise own-event suppression.
                    self.pending_enter = true;
                } else {
                    self.handle_char('\n', &event)?;
                }
                return Ok(());
            }
            TAB_KEYCODE => {
                self.handle_char('\t', &event)?;
                return Ok(());
            }
            ESCAPE_KEYCODE => {
                self.manager.reset(ResetReason::Manual);
                return Ok(());
            }
            _ => {}
        }

        let Some(layout) = self.active_layout else {
            return Ok(());
        };
        let Some(ch) = char_for(&event, layout.table()) else {
            return Ok(());
        };
        self.handle_char(ch, &event)
    }

    fn poll_hotkeys(&mut self) -> Result<(), Box<dyn Error>> {
        let events = match self.hotkey_grabber.as_ref() {
            Some(grabber) => grabber.poll_events()?,
            None => return Ok(()),
        };
        for event in events {
            self.last_input = Instant::now();
            if let Some(action) = self.hotkeys.action(u16::from(event.keycode), event.state) {
                let key = KeyEvent {
                    keycode: event.keycode,
                    state: event.state,
                    time: 0,
                    pressed: true,
                };
                self.handle_hotkey(action, &key)?;
            }
        }
        Ok(())
    }

    fn handle_shift_gesture(
        &mut self,
        action: ShiftGestureAction,
        event: &KeyEvent,
    ) -> Result<(), Box<dyn Error>> {
        match action {
            ShiftGestureAction::Word => {
                if self.fix_selection()? {
                    self.last_word_fix_at = None;
                    self.last_selection_fix_at = Some(Instant::now());
                } else if self.manual_fix_word(event)? {
                    self.last_word_fix_at = Some(Instant::now());
                }
            }
            // The third tap of a selection conversion is not a phrase fix.
            ShiftGestureAction::Phrase
                if self
                    .last_selection_fix_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(800)) => {}
            ShiftGestureAction::Phrase if self.config.general.triple_shift_phrase => {
                // A third tap arrives after the second one already fixed the
                // word. Roll that fix back first so the phrase is converted
                // from the text as the user typed it, not from a half-fixed
                // buffer.
                let undo_word = self
                    .last_word_fix_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(800))
                    && self.manager.undo_applied();
                if undo_word {
                    if let Some(plan) = self.manager.undo() {
                        self.apply(plan)?;
                    }
                }
                let applied = self.manual_fix_phrase(event)?;
                if !applied && undo_word {
                    if let Some(plan) = self.manager.undo() {
                        self.apply(plan)?;
                    }
                }
                self.last_word_fix_at = None;
            }
            ShiftGestureAction::Phrase => {}
        }
        Ok(())
    }

    /// A click moves the cursor or the selection: typed context is stale.
    fn on_pointer_press(&mut self) {
        self.mark_selection_seen();
        self.selection_fix = None;
        self.shift_gesture.reset();
        self.pending_space = false;
        self.manager.reset(ResetReason::PointerActivity);
    }

    fn mark_selection_seen(&mut self) {
        if let Some(watch) = self.selection.as_ref() {
            self.selection_seen = watch.generation();
        }
    }

    /// Convert text selected since the last key press or click, or flip
    /// the fragment converted just before back. Returns `true` when the
    /// gesture was consumed; `false` lets it fall back to the word fix.
    fn fix_selection(&mut self) -> Result<bool, Box<dyn Error>> {
        let Some(watch) = self.selection.as_ref() else {
            return Ok(false);
        };
        let generation = watch.generation();
        let fresh = generation != self.selection_seen;
        self.selection_seen = generation;
        if self.profile == Profile::Off
            || self.app_id.as_deref().is_some_and(selection::is_terminal)
        {
            return Ok(false);
        }
        if !fresh && self.selection_fix.is_none() {
            return Ok(false);
        }
        // An empty primary selection means it was cleared, not made.
        let Some(text) = selection::read_primary() else {
            return Ok(false);
        };
        // The converted fragment is still selected: flip it back.
        if let Some(fix) = self.selection_fix.take() {
            if text == fix.converted {
                self.apply_selection_fix(fix.reversed(), "Selection undo");
                return Ok(true);
            }
        }
        if !fresh {
            return Ok(false);
        }
        // From here on text is selected: a word fix would send Backspace and
        // delete the selection, so the gesture is consumed either way.
        if text.chars().count() > MAX_SELECTION_CHARS {
            println!("layoutpilot: selection too long to convert");
            return Ok(true);
        }
        let app_id = self.app_id.clone();
        let Some(conversion) = self
            .manager
            .engine()
            .convert_fragment(&text, app_id.as_deref())
        else {
            println!("layoutpilot: selection has nothing to convert");
            return Ok(true);
        };
        let fix = SelectionFix {
            original: text,
            converted: conversion.converted,
            source: conversion.source,
            target: conversion.target,
        };
        self.apply_selection_fix(fix, "Selection");
        self.manager.reset(ResetReason::Manual);
        Ok(true)
    }

    /// Flip the converted fragment back (undo hotkey).
    fn toggle_selection_fix(&mut self) -> Result<(), Box<dyn Error>> {
        let Some(fix) = self.selection_fix.take() else {
            return Ok(());
        };
        if selection::read_primary().as_deref() == Some(fix.converted.as_str()) {
            self.apply_selection_fix(fix.reversed(), "Selection undo");
        } else {
            println!("layoutpilot: converted fragment is no longer selected");
        }
        Ok(())
    }

    /// Replace the selection with `fix.converted` and remember the fix when
    /// the new text ends up selected again.
    fn apply_selection_fix(&mut self, fix: SelectionFix, label: &str) {
        match self.paste_over_selection(&fix.converted, fix.target) {
            Ok(reselected) => {
                println!(
                    "layoutpilot: {label} → layout {}, {} chars{}",
                    fix.target,
                    fix.converted.chars().count(),
                    if reselected { "" } else { ", not reselected" }
                );
                if let Some(tray) = self.tray.as_mut() {
                    tray.correction(&fix.original, &fix.converted);
                }
                self.last_selection_fix_at = Some(Instant::now());
                self.selection_fix = reselected.then_some(fix);
            }
            Err(error) => eprintln!("layoutpilot: selection conversion failed: {error}"),
        }
    }

    /// Paste `text` over the current selection through the clipboard, switch
    /// to `layout` and select the pasted text again. Returns whether the
    /// pasted text is selected afterwards.
    fn paste_over_selection(
        &mut self,
        text: &str,
        layout: LayoutId,
    ) -> Result<bool, Box<dyn Error>> {
        if self.dry_run {
            println!("[dry-run] Selection: layout={layout}, text={text:?}");
            return Ok(false);
        }
        if self.evdev_modifiers.any_held() {
            return Err("modifier held".into());
        }
        let saved = if self.config.inject.restore_clipboard {
            selection::save_clipboard()
        } else {
            None
        };
        if !selection::set_clipboard(text) {
            if self.config.inject.restore_clipboard {
                selection::restore_clipboard(saved);
            }
            return Err("could not put the conversion on the clipboard".into());
        }
        // Paste before switching: the shortcut is safest in the layout the
        // application already has.
        self.injector.tap_paste()?;
        self.remember_own_events();
        let previous_layout = self.active_layout;
        self.layouts.switch_to(layout)?;
        self.active_layout = Some(layout);
        if previous_layout != Some(layout) {
            self.sound.play();
        }
        // The application reads the clipboard asynchronously after Ctrl+V;
        // selecting before the text arrives would select the wrong range.
        std::thread::sleep(PASTE_SETTLE);
        if self.config.inject.restore_clipboard {
            selection::restore_clipboard(saved);
        }
        let chars = text.chars().count();
        let reselected = chars <= MAX_RESELECT_CHARS && {
            self.injector.select_left(chars)?;
            self.remember_own_events();
            selection::wait_primary(text)
        };
        // Our own selection is not a selection made by the user.
        if let Some(watch) = self.selection.as_ref() {
            watch.wait_change(self.selection_seen);
        }
        self.mark_selection_seen();
        Ok(reselected)
    }

    fn manual_fix_word(&mut self, event: &KeyEvent) -> Result<bool, Box<dyn Error>> {
        if self.manager.undo_applied() {
            if let Some(plan) = self.manager.undo() {
                self.apply(plan)?;
                return Ok(true);
            }
        }
        let app_id = self.app_id.clone();
        let context = self.context(event, app_id.as_deref());
        match self.manager.on_fix_last_word(&context) {
            Some(plan) => {
                self.apply(plan)?;
                Ok(true)
            }
            None => {
                println!("layoutpilot: no word to fix");
                Ok(false)
            }
        }
    }

    fn manual_fix_phrase(&mut self, event: &KeyEvent) -> Result<bool, Box<dyn Error>> {
        let app_id = self.app_id.clone();
        let context = self.context(event, app_id.as_deref());
        match self.manager.on_fix_last_phrase(&context) {
            Some(plan) => {
                self.apply(plan)?;
                Ok(true)
            }
            None => {
                println!("layoutpilot: no phrase to fix");
                Ok(false)
            }
        }
    }

    fn load_learning(&mut self) {
        let Some(path) = learned::path() else {
            eprintln!("layoutpilot: learning disabled: no state directory");
            return;
        };
        match learned::load(&path) {
            Ok(events) => {
                let detector = self.manager.engine_mut().detector_mut();
                for event in &events {
                    detector.apply_learning(event);
                }
                let (kept, accepted) = detector.learned_counts();
                println!(
                    "layoutpilot: learning from {} ({kept} kept, {accepted} confirmed)",
                    path.display()
                );
            }
            Err(error) => eprintln!("layoutpilot: cannot read {}: {error}", path.display()),
        }
        self.learned_path = Some(path);
    }

    /// Write learning events produced since the last call.
    fn persist_learning(&mut self) {
        let events = self.manager.engine_mut().take_learning();
        if events.is_empty() || self.dry_run {
            return;
        }
        let Some(path) = self.learned_path.as_deref() else {
            return;
        };
        match learned::append(path, &events) {
            // Counts only: the words themselves stay out of the journal.
            Ok(()) => println!("layoutpilot: learned {} event(s)", events.len()),
            Err(error) => eprintln!("layoutpilot: cannot write {}: {error}", path.display()),
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if self.paused {
            self.manager.pause();
            self.shift_gesture.reset();
            println!("layoutpilot: paused");
        } else {
            self.manager.resume();
            println!("layoutpilot: resumed");
        }
        if let Some(tray) = self.tray.as_mut() {
            tray.set_paused(self.paused);
        }
        let _ = std::io::stdout().flush();
    }

    /// Handle tray commands; returns `false` when the daemon should exit.
    fn poll_tray(&mut self) -> bool {
        let Some(tray) = self.tray.as_mut() else {
            return true;
        };
        tray.tick();
        let Some(command) = tray.try_command() else {
            return true;
        };
        match command {
            TrayCommand::TogglePause => {
                self.toggle_pause();
                true
            }
            TrayCommand::ToggleTerminals => {
                self.terminals_auto = !self.terminals_auto;
                self.profile = self.effective_profile();
                println!(
                    "layoutpilot: terminals auto={}, profile={:?}",
                    self.terminals_auto, self.profile
                );
                if let Some(tray) = self.tray.as_mut() {
                    tray.set_terminals_auto(self.terminals_auto);
                }
                true
            }
            TrayCommand::Quit => false,
        }
    }

    fn handle_hotkey(
        &mut self,
        action: HotkeyAction,
        event: &KeyEvent,
    ) -> Result<(), Box<dyn Error>> {
        if let Some((last_action, at)) = self.last_hotkey {
            if last_action == action && at.elapsed() < Duration::from_millis(400) {
                return Ok(());
            }
        }
        self.last_hotkey = Some((action, Instant::now()));
        match action {
            HotkeyAction::Pause => self.toggle_pause(),
            HotkeyAction::Undo => {
                if self.paused {
                    return Ok(());
                }
                if self.selection_fix.is_some() {
                    self.toggle_selection_fix()?;
                    return Ok(());
                }
                match self.manager.undo() {
                    Some(plan) => self.apply(plan)?,
                    None => println!("layoutpilot: undo unavailable"),
                }
            }
            HotkeyAction::Fix => {
                if self.paused || self.fix_selection()? {
                    return Ok(());
                }
                let app_id = self.app_id.clone();
                let context = self.context(event, app_id.as_deref());
                match self.manager.on_fix_last_word(&context) {
                    Some(plan) => self.apply(plan)?,
                    None => println!("layoutpilot: no word to fix"),
                }
            }
        }
        let _ = std::io::stdout().flush();
        Ok(())
    }

    fn handle_char(&mut self, ch: char, event: &KeyEvent) -> Result<(), Box<dyn Error>> {
        let app_id = self.app_id.clone();
        let context = self.context(event, app_id.as_deref());
        if let Some(plan) = self.manager.on_char(ch, &context) {
            self.apply(plan)?;
        }
        Ok(())
    }

    fn context<'a>(&self, event: &KeyEvent, app_id: Option<&'a str>) -> InputContext<'a> {
        let mut context =
            InputContext::new(self.profile, self.active_layout.unwrap_or(LayoutId::Us))
                .with_layout_trusted(self.active_layout.is_some())
                .with_modifiers(modifiers(event));
        if let Some(app_id) = app_id {
            context = context.with_app_id(app_id);
        }
        context
    }

    fn layout_settle_delay(&self) -> Duration {
        // uinput events go through the kernel and the compositor's keymap;
        // 20 ms is enough for XTest but can lose the first character on
        // Wayland. Keep a floor without changing the configured value for X11.
        let floor = if matches!(&self.injector, Injector::Uinput(_)) {
            100
        } else {
            0
        };
        Duration::from_millis(self.config.inject.layout_settle_ms.max(floor))
    }

    fn pre_inject_delay(&self) -> Option<Duration> {
        // evdev gives us the trigger before the compositor has necessarily
        // delivered the word to the focused client. Without this pause uinput
        // can type the replacement at the document start and the physical
        // word arrives afterwards ("приветghbdtn"). XTest is ordered by the
        // X server, so the delay is only needed for uinput.
        if matches!(&self.injector, Injector::Uinput(_))
            && self.config.inject.pre_inject_delay_ms > 0
        {
            Some(Duration::from_millis(
                self.config.inject.pre_inject_delay_ms,
            ))
        } else {
            None
        }
    }

    fn apply(&mut self, plan: CorrectionPlan) -> Result<(), Box<dyn Error>> {
        if self.dry_run {
            println!(
                "[dry-run] {:?}: layout={}, backspaces={}, text={:?}",
                plan.kind, plan.layout, plan.backspaces, plan.text
            );
            return Ok(());
        }

        if !plan.is_typeable() {
            eprintln!(
                "layoutpilot: skip plan, text {:?} is not typeable in {}",
                plan.text, plan.layout
            );
            self.manager.reset(ResetReason::Manual);
            return Ok(());
        }
        if !self.layouts.supports(plan.layout) {
            eprintln!(
                "layoutpilot: skip plan, layout {} is not configured",
                plan.layout
            );
            self.manager.reset(ResetReason::Manual);
            return Ok(());
        }

        if let Some(delay) = self.pre_inject_delay() {
            std::thread::sleep(delay);
        }
        // Keys typed after the trigger have already reached the application.
        let mut keys = Vec::new();
        self.collect_pending(&mut keys)?;
        let tail = match tail_text(&keys, plan.layout.table()) {
            Ok(_) if self.evdev_modifiers.any_held() => Err("modifier held"),
            Ok(tail) => Ok(tail),
            Err(Blocked::Shortcut) => Err("shortcut typed"),
            Err(Blocked::NonText(_)) => Err("editing key typed"),
        };
        let tail = match tail {
            Ok(tail) => tail,
            Err(reason) => {
                self.skip_plan(reason, keys);
                return Ok(());
            }
        };
        let tail_len = tail.chars().count();

        let previous_layout = self.active_layout;
        // Delete first, while the old layout is still active: switching the
        // layout reloads the XWayland keymap and can swallow one Backspace.
        match self.config.inject.delete_mode {
            DeleteMode::CtrlBackspace if tail_len == 0 => {
                if plan.backspaces > 0 {
                    self.injector.tap_ctrl_backspace()?;
                }
            }
            // Ctrl+Backspace cannot delete "word + tail" reliably.
            DeleteMode::CtrlBackspace | DeleteMode::Backspace => {
                self.injector.tap_backspace(plan.backspaces + tail_len)?;
            }
        }
        self.layouts.switch_to(plan.layout)?;
        if previous_layout != Some(plan.layout) || self.layouts.always_switch() {
            std::thread::sleep(self.layout_settle_delay());
        }
        self.injector.tap_text(&plan.text, plan.layout.table())?;
        // Everything typed after the deletion point; used by repairs.
        let mut typed = plan.text.clone();
        if let Some(separator) = plan.separator {
            // Send the separator as a separate, delayed key event. Some
            // XWayland clients (Kate/Geany) drop it when it follows a layout
            // switch too closely.
            std::thread::sleep(Duration::from_millis(self.config.inject.separator_delay_ms));
            self.injector
                .tap_text(&separator.to_string(), plan.layout.table())?;
            typed.push(separator);
        }
        if !tail.is_empty() {
            self.injector.tap_text(&tail, plan.layout.table())?;
            typed.push_str(&tail);
        }
        self.active_layout = Some(plan.layout);
        if previous_layout != Some(plan.layout) {
            self.sound.play();
        }
        self.remember_own_events();
        if plan.separator == Some('\n') {
            // Synthetic Return can be captured again and start a second
            // correction. Ignore Return presses for a short window.
            self.enter_ignore_until = Instant::now() + Duration::from_millis(300);
        }
        println!(
            "layoutpilot: {:?} → layout {}, {} backspaces, {} chars, tail {}",
            plan.kind,
            plan.layout,
            plan.backspaces,
            plan.text.chars().count(),
            tail_len
        );

        self.show_correction(&plan);
        self.repair_interleaved(&plan, &mut typed, &mut keys)?;
        // Feed the tail to the engine; it is now text in the target layout.
        self.backlog.extend(keys);
        Ok(())
    }

    fn show_correction(&mut self, plan: &CorrectionPlan) {
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        let Some(correction) = self.manager.last_correction() else {
            return;
        };
        let (typed, converted) = match plan.kind {
            PlanKind::Undo => (&correction.converted, &correction.original),
            PlanKind::Replace | PlanKind::Redo => (&correction.original, &correction.converted),
        };
        tray.correction(typed, converted);
    }

    /// Keys pressed while the replacement was being typed landed somewhere
    /// inside it. Each produced exactly one character after the deletion
    /// point, so deleting `typed + late` characters and retyping restores a
    /// consistent text.
    fn repair_interleaved(
        &mut self,
        plan: &CorrectionPlan,
        typed: &mut String,
        keys: &mut Vec<PendingKey>,
    ) -> Result<(), Box<dyn Error>> {
        let mut repairs = 0;
        loop {
            std::thread::sleep(CAPTURE_LAG);
            let start = keys.len();
            self.collect_pending(keys)?;
            let late = match tail_text(&keys[start..], plan.layout.table()) {
                Ok(late) if late.is_empty() => return Ok(()),
                Ok(late) if repairs < MAX_REPAIRS && !self.evdev_modifiers.any_held() => late,
                _ => {
                    eprintln!(
                        "layoutpilot: input during correction could not be repaired; \
                         check the text"
                    );
                    self.manager.reset(ResetReason::Manual);
                    return Ok(());
                }
            };
            let late_len = late.chars().count();
            self.injector
                .tap_backspace(typed.chars().count() + late_len)?;
            typed.push_str(&late);
            self.injector.tap_text(typed, plan.layout.table())?;
            self.remember_own_events();
            repairs += 1;
            println!("layoutpilot: repaired {late_len} chars typed during correction");
        }
    }

    /// Move pending physical keys (backlog first, then capture) into `keys`,
    /// waiting briefly for held modifiers to be released.
    fn collect_pending(&mut self, keys: &mut Vec<PendingKey>) -> Result<(), Box<dyn Error>> {
        keys.extend(self.backlog.drain(..));
        while let Some(event) = self.capture.try_recv()? {
            keys.extend(self.normalize(event));
        }
        let deadline = Instant::now() + MODIFIER_RELEASE_WAIT;
        while self.evdev_modifiers.any_held() && Instant::now() < deadline {
            if let Some(event) = self.capture.recv_timeout(Duration::from_millis(5))? {
                keys.extend(self.normalize(event));
            }
        }
        // Keys pressed meanwhile join the tail; their release is awaited too.
        let deadline = Instant::now() + KEY_RELEASE_WAIT;
        while !self.held_keys.is_empty() && Instant::now() < deadline {
            if let Some(event) = self.capture.recv_timeout(Duration::from_millis(5))? {
                keys.extend(self.normalize(event));
            }
        }
        Ok(())
    }

    fn track_held(&mut self, keycode: u16, pressed: bool) {
        if pressed {
            self.held_keys.insert(keycode);
        } else {
            self.held_keys.remove(&keycode);
        }
    }

    /// Abandon a plan before anything was injected and process the keys as
    /// ordinary input.
    fn skip_plan(&mut self, reason: &str, keys: Vec<PendingKey>) {
        println!("layoutpilot: correction skipped: {reason}");
        self.manager.reset(ResetReason::Manual);
        self.backlog.extend(keys);
    }

    fn remember_own_events(&mut self) {
        self.ignore.extend(self.injector.take_sent_events());
        self.ignore_until = Instant::now() + IGNORE_WINDOW;
    }

    fn consume_own(&mut self, event: &KeyEvent) -> bool {
        // Do not rely on strict front-of-queue order: XRecord may interleave
        // physical releases or lose a synthetic event. Matching anywhere in the
        // window prevents an injected Backspace from being treated as a real
        // Backspace (which would trigger backspace-undo and corrupt the text).
        let found = self
            .ignore
            .iter()
            .position(|&(keycode, pressed)| keycode == event.keycode && pressed == event.pressed);
        if let Some(index) = found {
            self.ignore.remove(index);
            true
        } else {
            false
        }
    }

    fn refresh_layout(&mut self) {
        let active = self.layouts.active_layout().ok().flatten();
        if active != self.active_layout {
            if self.active_layout.is_some() && active.is_some() && !self.layout_follows_focus() {
                self.sound.play();
            }
            self.active_layout = active;
            if let Some(active) = active {
                self.layouts.sync_xkb(active);
            }
            println!("layoutpilot: active layout → {active:?}");
            self.manager.reset(ResetReason::LayoutChange);
        }
    }

    /// Whether the layout change was caused by a window switch (KDE and other
    /// desktops keep a layout per window) rather than by the user.
    fn layout_follows_focus(&mut self) -> bool {
        self.refresh_focus(true);
        let recent = |at: Option<Instant>| at.is_some_and(|at| at.elapsed() < FOCUS_LAYOUT_WINDOW);
        recent(self.focus_changed_at)
            || recent(
                self.kde_focus
                    .as_ref()
                    .and_then(kde_focus::KdeFocus::changed_at),
            )
    }

    fn effective_profile(&self) -> Profile {
        let profile = self.resolver.resolve(self.app_id.as_deref().unwrap_or(""));
        if self.terminals_auto && profile == Profile::Manual {
            Profile::Auto
        } else {
            profile
        }
    }

    fn refresh_focus(&mut self, force: bool) {
        if !force && self.last_focus_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_focus_poll = Instant::now();

        let (window, mut app_id) = match self.focus.snapshot() {
            Ok(info) => (Some(info.window), info.app_id),
            Err(_) => (None, None),
        };
        // Native Wayland windows are invisible to X11Focus; KWin reports their
        // resource class through the loaded script.
        if app_id.is_none() {
            app_id = self
                .kde_focus
                .as_ref()
                .and_then(kde_focus::KdeFocus::app_id);
        }
        let window_changed = self.window != window;
        let app_changed = self.app_id != app_id;
        self.window = window;
        self.app_id = app_id;
        self.profile = self.effective_profile();
        if window_changed || app_changed {
            self.focus_changed_at = Some(Instant::now());
            println!(
                "layoutpilot: focus window={:?}, app={:?}, profile={:?}",
                self.window, self.app_id, self.profile
            );
            self.selection_fix = None;
            if self.clear_on_focus_change {
                self.manager.reset(ResetReason::FocusChange);
            }
        }
    }
}

fn device_label(info: &layoutpilot_capture::KeyboardInfo) -> String {
    match &info.name {
        Some(name) => format!("{name} ({})", info.path.display()),
        None => info.path.display().to_string(),
    }
}

fn is_wayland_session() -> bool {
    env::var("XDG_SESSION_TYPE").is_ok_and(|value| value.eq_ignore_ascii_case("wayland"))
        || env::var_os("WAYLAND_DISPLAY").is_some()
}

fn setup_hotkey_grabber(hotkeys: &Hotkeys) -> (Option<X11HotkeyGrabber>, HashSet<HotkeyAction>) {
    let mut grabbed = HashSet::new();
    if is_wayland_session() {
        // XGrabKey talks to XWayland, but native Wayland clients never deliver
        // those grabs. If we treated the grab as active, handle_key would skip
        // passive matching and the hotkey would do nothing at all. Match them
        // in the evdev stream instead; the focused app may also see the key.
        println!("layoutpilot: Wayland: hotkeys are passive (evdev)");
        return (None, grabbed);
    }
    if env::var_os("DISPLAY").is_none() {
        return (None, grabbed);
    }
    let mut grabber = match X11HotkeyGrabber::new() {
        Ok(grabber) => grabber,
        Err(error) => {
            eprintln!("layoutpilot: XGrabKey unavailable: {error}; hotkeys stay passive");
            return (None, grabbed);
        }
    };
    for (action, hotkey) in [
        (HotkeyAction::Undo, hotkeys.undo),
        (HotkeyAction::Fix, hotkeys.fix),
        (HotkeyAction::Pause, hotkeys.pause),
    ] {
        let Some(hotkey) = hotkey else {
            continue;
        };
        match grabber.grab(hotkey.keycode, hotkey.modifier_mask()) {
            Ok(()) => {
                grabbed.insert(action);
            }
            Err(error) => {
                eprintln!("layoutpilot: cannot grab {action:?} hotkey ({error}); it stays passive");
            }
        }
    }
    println!(
        "layoutpilot: grabbed {} of 3 hotkeys (XGrabKey)",
        grabbed.len()
    );
    (Some(grabber), grabbed)
}

fn is_kde_session() -> bool {
    env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase()
        .contains("KDE")
        || env::var_os("KDE_FULL_SESSION").is_some()
}

fn is_sway_session() -> bool {
    env::var_os("SWAYSOCK").is_some()
        || env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_uppercase()
            .contains("SWAY")
}

fn is_hyprland_session() -> bool {
    env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
        || env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_uppercase()
            .contains("HYPRLAND")
}

fn open_layouts(config: &Config) -> Result<LayoutController, Box<dyn Error>> {
    let requested = config.layout.backend;
    let backend = match requested {
        LayoutBackend::Auto if is_kde_session() => LayoutBackend::Kde,
        LayoutBackend::Auto if is_sway_session() => LayoutBackend::Sway,
        LayoutBackend::Auto if is_hyprland_session() => LayoutBackend::Hyprland,
        LayoutBackend::Auto => LayoutBackend::X11,
        backend => backend,
    };
    match backend {
        LayoutBackend::Kde => match KdeLayouts::new() {
            Ok(kde) => {
                match kde.watch() {
                    Ok(()) => println!("layoutpilot: KDE layout changes via D-Bus signals"),
                    Err(error) => {
                        eprintln!("layoutpilot: KDE layout signals unavailable ({error}); polling");
                    }
                }
                let xkb = match X11LayoutState::new(config.layout_order()) {
                    Ok(xkb) => Some(xkb),
                    Err(error) => {
                        eprintln!("layoutpilot: XKB sync unavailable: {error}");
                        None
                    }
                };
                if let Some(current) = kde.current().ok().flatten() {
                    if let Some(xkb) = &xkb {
                        let _ = xkb.switch_to(current);
                    }
                }
                Ok(LayoutController::Kde { kde, xkb })
            }
            Err(error) if requested == LayoutBackend::Auto => {
                eprintln!("layoutpilot: KDE layout backend unavailable ({error}); using XKB");
                Ok(LayoutController::X11(open_xkb(config)?))
            }
            Err(error) => Err(error.into()),
        },
        LayoutBackend::Sway => Ok(LayoutController::Sway(SwayLayouts::new()?)),
        LayoutBackend::Hyprland => Ok(LayoutController::Hyprland(HyprlandLayouts::new()?)),
        LayoutBackend::X11 => Ok(LayoutController::X11(open_xkb(config)?)),
        other => Err(format!("layout backend {other:?} is not wired yet").into()),
    }
}

/// XKB layout state whose group order follows the active keymap: a desktop
/// configured as `ru,us` must not be read with the default `us,ru` order.
fn open_xkb(config: &Config) -> Result<X11LayoutState, Box<dyn Error>> {
    let mut state = X11LayoutState::new(config.layout_order())?;
    match state.detect_order() {
        Some(order) => {
            if order != state.layouts() {
                println!("layoutpilot: XKB group order from keymap: {order:?}");
            }
            state.set_layouts(order);
        }
        None => println!(
            "layoutpilot: XKB group order not detected; using config {:?}",
            state.layouts()
        ),
    }
    Ok(state)
}

fn open_capture(config: &Config) -> Result<Capture, Box<dyn Error>> {
    let backend = match config.capture.backend {
        // On Wayland DISPLAY usually points at XWayland, whose XRecord only
        // sees XWayland clients. Native Wayland needs evdev.
        CaptureBackend::Auto if is_wayland_session() => CaptureBackend::Evdev,
        CaptureBackend::Auto if env::var_os("DISPLAY").is_some() => CaptureBackend::Xrecord,
        CaptureBackend::Auto => CaptureBackend::Evdev,
        backend => backend,
    };
    match backend {
        CaptureBackend::Xrecord => Ok(Capture::X11(XRecordCapture::new()?)),
        CaptureBackend::Evdev => Ok(Capture::Evdev(EvdevCapture::open()?)),
        other => Err(format!(
            "capture backend {other:?} is not wired yet; use evdev, xrecord or auto"
        )
        .into()),
    }
}

fn config_backend_name(backend: CaptureBackend) -> &'static str {
    match backend {
        CaptureBackend::Auto => "auto",
        CaptureBackend::Evdev => "evdev",
        CaptureBackend::Xrecord => "xrecord",
        CaptureBackend::Portal => "portal",
        CaptureBackend::Ime => "ime",
    }
}

fn open_injector(config: &Config) -> Result<Injector, Box<dyn Error>> {
    match config.inject.backend {
        InjectBackend::Auto if is_wayland_session() => match UinputInjector::new() {
            Ok(injector) => Ok(Injector::Uinput(injector)),
            Err(error) => {
                eprintln!(
                    "layoutpilot: uinput unavailable ({error}); falling back to XTest/XWayland"
                );
                Ok(Injector::X11(Box::new(X11Injector::new()?)))
            }
        },
        InjectBackend::Auto | InjectBackend::Xtest => {
            Ok(Injector::X11(Box::new(X11Injector::new()?)))
        }
        InjectBackend::Uinput => Ok(Injector::Uinput(UinputInjector::new()?)),
        other => Err(format!("inject backend {other:?} is not wired yet").into()),
    }
}

fn load_config(explicit: Option<PathBuf>) -> Result<(Config, Option<PathBuf>), Box<dyn Error>> {
    let path = explicit.or_else(Config::default_path);
    match path {
        Some(path) => Ok((Config::load_or_default(&path)?, Some(path))),
        None => Ok((Config::default(), None)),
    }
}

fn doctor(config: &Config, path: Option<&PathBuf>) -> Result<(), Box<dyn Error>> {
    let mut failures = 0;
    let wayland = is_wayland_session();

    println!("LayoutPilot doctor");
    println!("session: {}", if wayland { "wayland" } else { "x11" });
    println!("DISPLAY: {:?}", env::var("DISPLAY").ok());
    match path {
        Some(path) => println!("config: {}", path.display()),
        None => println!("config: built-in defaults"),
    }
    println!(
        "backends: capture={:?}, inject={:?}, layout={:?}, order={:?}",
        config.capture.backend, config.inject.backend, config.layout.backend, config.layout.order
    );

    println!(
        "hotkeys: undo={:?}, fix={:?}, pause={:?}",
        config.general.undo_hotkey, config.general.fix_hotkey, config.general.pause_hotkey
    );
    println!(
        "triggers: {:?}, backspace_undo={}, sound={}{}",
        config.general.triggers,
        config.general.backspace_undo,
        config.general.sound_on_layout_change,
        if config.general.sound_on_layout_change {
            format!(" ({})", config.general.sound_file)
        } else {
            String::new()
        }
    );
    println!(
        "inject: delete_mode={:?}, pre_inject_delay_ms={}, layout_settle_ms={}, separator_delay_ms={}",
        config.inject.delete_mode,
        config.inject.pre_inject_delay_ms,
        config.inject.layout_settle_ms,
        config.inject.separator_delay_ms
    );

    if wayland {
        match UinputInjector::new() {
            Ok(_) => println!("uinput: ok"),
            Err(error) => {
                failures += 1;
                println!("uinput: FAIL ({error})");
            }
        }
    } else {
        match X11HotkeyGrabber::new() {
            Ok(mut grabber) => {
                println!("XGrabKey: ok");
                grabber.ungrab_all();
            }
            Err(error) => {
                failures += 1;
                println!("XGrabKey: FAIL ({error})");
            }
        }

        match X11Injector::new() {
            Ok(_) => println!("XTEST: ok"),
            Err(error) => {
                failures += 1;
                println!("XTEST: FAIL ({error})");
            }
        }

        match X11LayoutState::new(config.layout_order()) {
            Ok(layouts) => match (layouts.group(), layouts.active_layout()) {
                (Ok(group), Ok(active)) => {
                    println!("XKB: ok (group {group}, active {active:?})");
                }
                (Err(error), _) | (_, Err(error)) => {
                    failures += 1;
                    println!("XKB: FAIL ({error})");
                }
            },
            Err(error) => {
                failures += 1;
                println!("XKB: FAIL ({error})");
            }
        }
    }

    if is_kde_session() {
        match KdeLayouts::new() {
            Ok(kde) => match kde.current() {
                Ok(current) => println!(
                    "KDE layouts: ok (current={current:?}, list={:?})",
                    kde.layouts()
                        .iter()
                        .map(|layout| (&layout.code, layout.id))
                        .collect::<Vec<_>>()
                ),
                Err(error) => {
                    failures += 1;
                    println!("KDE layouts: FAIL ({error})");
                }
            },
            Err(error) => {
                failures += 1;
                println!("KDE layouts: FAIL ({error})");
            }
        }
    }

    if is_sway_session() {
        match SwayLayouts::new() {
            Ok(sway) => match sway.current() {
                Ok(current) => println!(
                    "Sway layouts: ok (current={current:?}, list={:?})",
                    sway.layouts()
                ),
                Err(error) => {
                    failures += 1;
                    println!("Sway layouts: FAIL ({error})");
                }
            },
            Err(error) => {
                failures += 1;
                println!("Sway layouts: FAIL ({error})");
            }
        }
    }

    if is_hyprland_session() {
        match HyprlandLayouts::new() {
            Ok(hyprland) => match hyprland.current() {
                Ok(current) => println!(
                    "Hyprland layouts: ok (current={current:?}, list={:?})",
                    hyprland.layouts()
                ),
                Err(error) => {
                    failures += 1;
                    println!("Hyprland layouts: FAIL ({error})");
                }
            },
            Err(error) => {
                failures += 1;
                println!("Hyprland layouts: FAIL ({error})");
            }
        }
    }

    match X11Focus::new() {
        Ok(focus) => match focus.snapshot() {
            Ok(info) => println!("focus: window={:#x}, app={:?}", info.window, info.app_id),
            Err(error) => println!("focus: unavailable ({error})"),
        },
        Err(error) => println!("focus: FAIL ({error})"),
    }

    match open_capture(config) {
        Ok(mut capture) => {
            println!("capture: ok ({})", capture.backend_name());
            capture.stop();
        }
        Err(error) => {
            failures += 1;
            println!("capture: FAIL ({error})");
        }
    }

    let mut detector = DictionaryDetector::new();
    println!(
        "dictionary: ru(привет)={}, en(hello)={}",
        detector.dict_count(Language::Ru, "привет"),
        detector.dict_count(Language::En, "hello")
    );
    if config.privacy.learning {
        match learned::path() {
            Some(path) => match learned::load(&path) {
                Ok(events) => {
                    for event in &events {
                        detector.apply_learning(event);
                    }
                    let (kept, accepted) = detector.learned_counts();
                    println!(
                        "learning: {} ({kept} kept, {accepted} confirmed)",
                        path.display()
                    );
                }
                Err(error) => {
                    println!("learning: cannot read {}: {error}", path.display());
                    failures += 1;
                }
            },
            None => println!("learning: no state directory"),
        }
    } else {
        println!("learning: off");
    }

    if failures == 0 {
        println!("result: ok");
        Ok(())
    } else {
        Err(format!("doctor found {failures} problem(s)").into())
    }
}

fn print_help() {
    println!(
        "layoutpilot — X11 layout auto-corrector\n\n\
         usage:\n\
         \x20 layoutpilot doctor [--config PATH]   check backends and dictionaries\n\
         \x20 layoutpilot run [--dry-run] [--config PATH]\n\n\
         Config defaults to $XDG_CONFIG_HOME/layoutpilot/config.toml or\n\
         $HOME/.config/layoutpilot/config.toml. Set DISPLAY for X11."
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Run { dry_run: bool },
    Doctor,
    Help,
}

fn parse_args(args: &[String]) -> Result<(Command, Option<PathBuf>), String> {
    let mut command = None;
    let mut dry_run = false;
    let mut config = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "run" => {
                if command.is_some() {
                    return Err("only one command is allowed".to_owned());
                }
                command = Some(Command::Run { dry_run: false });
            }
            "doctor" => {
                if command.is_some() {
                    return Err("only one command is allowed".to_owned());
                }
                command = Some(Command::Doctor);
            }
            "--dry-run" => dry_run = true,
            "--config" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--config requires a path".to_owned())?;
                config = Some(PathBuf::from(value));
            }
            "-h" | "--help" => command = Some(Command::Help),
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }

    let command = match command {
        Some(Command::Run { .. }) | None => Command::Run { dry_run },
        Some(command) => command,
    };
    Ok((command, config))
}

/// Make sure only one daemon per user corrects input: two would fix every
/// word twice. Autostart and the systemd unit can both be enabled by
/// mistake. The abstract socket disappears with the process, so a crash
/// leaves nothing stale behind.
fn claim_instance() -> Result<std::os::unix::net::UnixListener, Box<dyn Error>> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::{SocketAddr, UnixListener};

    let uid = std::fs::metadata("/proc/self")?.uid();
    let name = format!("layoutpilot-{uid}");
    let address = SocketAddr::from_abstract_name(name.as_bytes())?;
    UnixListener::bind_addr(&address).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            "another layoutpilot is already running for this user".into()
        } else {
            Box::<dyn Error>::from(format!("cannot claim instance lock: {error}"))
        }
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = (|| -> Result<(), Box<dyn Error>> {
        let (command, config_path) = parse_args(&args)?;
        match command {
            Command::Help => {
                print_help();
                Ok(())
            }
            Command::Doctor => {
                let (config, path) = load_config(config_path)?;
                doctor(&config, path.as_ref())
            }
            Command::Run { dry_run } => {
                let (config, path) = load_config(config_path)?;
                if !config.general.enabled {
                    println!("layoutpilot: disabled by config; exiting");
                    return Ok(());
                }
                match path {
                    Some(path) => println!("layoutpilot: config {}", path.display()),
                    None => println!("layoutpilot: built-in config"),
                }
                // Dry runs may sit next to the live daemon for testing.
                let _instance = if dry_run { None } else { Some(claim_instance()?) };
                Daemon::new(dry_run, config)?.run()
            }
        }
    })();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("layoutpilot: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_defaults_to_manual() {
        let resolver = Config::default().to_profile_resolver();
        assert_eq!(
            resolver.resolve("org.gnome.Terminal"),
            layoutpilot_core::Profile::Manual
        );
        assert_eq!(
            resolver.resolve("Alacritty"),
            layoutpilot_core::Profile::Manual
        );
        assert_eq!(
            resolver.resolve("org.keepassxc.KeePassXC"),
            layoutpilot_core::Profile::Off
        );
        assert_eq!(
            resolver.resolve("org.gnome.TextEditor"),
            layoutpilot_core::Profile::Auto
        );
    }

    #[test]
    fn default_hotkeys_are_parsed_and_matched() {
        let hotkeys = Hotkeys::from_config(&Config::default()).unwrap();
        assert_eq!(hotkeys.action(127, 0), Some(HotkeyAction::Undo));
        assert_eq!(
            hotkeys.action(74, layoutpilot_config::hotkey::MOD_CONTROL),
            Some(HotkeyAction::Fix)
        );
        assert_eq!(
            hotkeys.action(75, layoutpilot_config::hotkey::MOD_CONTROL),
            Some(HotkeyAction::Pause)
        );
        assert_eq!(hotkeys.action(74, 0), None);
    }

    #[test]
    fn double_shift_gesture_fires_and_resets() {
        let start = Instant::now();
        let mut gesture = ShiftGesture::default();
        assert_eq!(gesture.on_key(50, true, start), None);
        assert_eq!(
            gesture.on_key(50, false, start + Duration::from_millis(20)),
            None
        );
        assert_eq!(
            gesture.on_key(50, true, start + Duration::from_millis(40)),
            None
        );
        assert_eq!(
            gesture.on_key(50, false, start + Duration::from_millis(60)),
            Some(ShiftGestureAction::Word)
        );
        // After the gesture window a new tap starts a fresh sequence and does
        // not turn the previous word gesture into a phrase.
        assert_eq!(
            gesture.on_key(50, true, start + Duration::from_millis(600)),
            None
        );
        assert_eq!(
            gesture.on_key(50, false, start + Duration::from_millis(620)),
            None
        );
    }

    #[test]
    fn triple_shift_gesture_fires_phrase() {
        let start = Instant::now();
        let mut gesture = ShiftGesture::default();
        assert_eq!(gesture.on_key(50, true, start), None);
        assert_eq!(
            gesture.on_key(50, false, start + Duration::from_millis(20)),
            None
        );
        assert_eq!(
            gesture.on_key(62, true, start + Duration::from_millis(40)),
            None
        );
        assert_eq!(
            gesture.on_key(62, false, start + Duration::from_millis(60)),
            Some(ShiftGestureAction::Word)
        );
        assert_eq!(
            gesture.on_key(50, true, start + Duration::from_millis(80)),
            None
        );
        assert_eq!(
            gesture.on_key(50, false, start + Duration::from_millis(100)),
            Some(ShiftGestureAction::Phrase)
        );
    }

    #[test]
    fn double_shift_gesture_breaks_on_other_key() {
        let start = Instant::now();
        let mut gesture = ShiftGesture::default();
        assert_eq!(gesture.on_key(50, true, start), None);
        assert_eq!(
            gesture.on_key(50, false, start + Duration::from_millis(20)),
            None
        );
        assert_eq!(
            gesture.on_key(38, true, start + Duration::from_millis(30)),
            None
        );
        assert_eq!(
            gesture.on_key(38, false, start + Duration::from_millis(40)),
            None
        );
        assert_eq!(
            gesture.on_key(50, true, start + Duration::from_millis(50)),
            None
        );
        assert_eq!(
            gesture.on_key(50, false, start + Duration::from_millis(60)),
            None
        );
    }

    #[test]
    fn default_config_keeps_punctuation_and_lowercase() {
        let config = Config::default();
        let mut manager =
            CorrectionManager::new(config.to_engine_config(), DictionaryDetector::new());
        let context = InputContext::new(Profile::Auto, LayoutId::Ru);
        let mut plan = None;
        for ch in "руддщ! ".chars() {
            if let Some(next) = manager.on_char(ch, &context) {
                plan = Some(next);
            }
        }
        let plan = plan.expect("plan");
        assert_eq!(plan.text, "hello!");
        assert_eq!(plan.separator, Some(' '));
        assert_eq!(plan.backspaces, 7);
    }

    #[test]
    fn evdev_modifiers_follow_press_and_release() {
        let mut modifiers = EvdevModifiers::default();
        assert!(modifiers.update(EVDEV_LEFTSHIFT, 1));
        assert_eq!(modifiers.state() & SHIFT_MASK, SHIFT_MASK);
        let _ = modifiers.update(EVDEV_LEFTSHIFT, 0);
        assert_eq!(modifiers.state() & SHIFT_MASK, 0);

        let _ = modifiers.update(EVDEV_LEFTCTRL, 1);
        let _ = modifiers.update(EVDEV_LEFTMETA, 1);
        assert_eq!(modifiers.state() & CONTROL_MASK, CONTROL_MASK);
        assert_eq!(modifiers.state() & SUPER_MASK, SUPER_MASK);

        assert!(modifiers.update(EVDEV_CAPSLOCK, 1));
        assert_eq!(modifiers.state() & LOCK_MASK, LOCK_MASK);
        let _ = modifiers.update(EVDEV_CAPSLOCK, 1);
        assert_eq!(modifiers.state() & LOCK_MASK, 0);
        assert!(!modifiers.update(30, 1));
    }

    #[test]
    fn parses_cli() {
        let args = vec!["run".to_owned(), "--dry-run".to_owned()];
        assert_eq!(
            parse_args(&args).unwrap(),
            (Command::Run { dry_run: true }, None)
        );

        let args = vec![
            "doctor".to_owned(),
            "--config".to_owned(),
            "x.toml".to_owned(),
        ];
        assert_eq!(
            parse_args(&args).unwrap(),
            (Command::Doctor, Some(PathBuf::from("x.toml")))
        );

        assert!(parse_args(&["bogus".to_owned()]).is_err());
    }
}
