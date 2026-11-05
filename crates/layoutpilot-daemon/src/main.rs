//! `layoutpilot` daemon.
//!
//! Data flow: XRecord/evdev key events → layout lookup → character
//! normalization → `CorrectionManager` (engine + dictionaries) → injection
//! (XTest on X11, uinput on Wayland) with a compositor layout switch.
//!
//! The daemon loads `~/.config/layoutpilot/config.toml` (or `--config PATH`)
//! and falls back to built-in defaults.

use std::collections::{HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::time::{Duration, Instant};

use layoutpilot_capture::{EvdevCapture, EvdevKeyEvent};
use layoutpilot_compositor::{HyprlandLayouts, KdeLayouts, SwayLayouts};
use layoutpilot_config::{
    CaptureBackend, Config, DeleteMode, Hotkey, InjectBackend, LayoutBackend, Trigger,
};
use layoutpilot_core::{
    CorrectionManager, CorrectionPlan, InputContext, LayoutId, LayoutTable, Profile,
    ProfileResolver, ResetReason,
};
use layoutpilot_dict::{DictionaryDetector, Language};
use layoutpilot_inject::UinputInjector;
use layoutpilot_x11::{
    char_for, modifiers, KeyEvent, X11Focus, X11HotkeyGrabber, X11Injector, X11LayoutState,
    XRecordCapture,
};

const BACKSPACE_KEYCODE: u8 = 22;
const RETURN_KEYCODE: u8 = 36;
const TAB_KEYCODE: u8 = 23;
const SPACE_KEYCODE: u8 = 65;
const ESCAPE_KEYCODE: u8 = 9;
const IGNORE_WINDOW: Duration = Duration::from_millis(750);

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

/// XKB keycodes for left and right Shift.
const SHIFT_KEYCODES: [u8; 2] = [50, 62];

/// Double Shift fixes the last word; any other key breaks the sequence.
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

    /// Feed one key event; returns true when the second Shift is released.
    fn on_key(&mut self, keycode: u8, pressed: bool, now: Instant) -> bool {
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
                self.reset();
                return true;
            } else if self.count >= 3 {
                // Phrases are not implemented yet; do not let a third tap
                // leave the counter in a state where it fires later.
                self.reset();
            }
            return false;
        }
        if pressed {
            self.reset();
            self.broken = true;
        }
        false
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
    pending_enter: bool,
    pending_space: bool,
    enter_ignore_until: Instant,
    evdev_modifiers: EvdevModifiers,
    shift_gesture: ShiftGesture,
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
            pending_enter: false,
            pending_space: false,
            enter_ignore_until: Instant::now(),
            evdev_modifiers: EvdevModifiers::default(),
            shift_gesture: ShiftGesture::default(),
            dry_run,
        };
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
            if Instant::now() >= self.ignore_until && !self.ignore.is_empty() {
                self.ignore.clear();
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
        match event {
            CapturedEvent::X11(event) => self.handle_key(event),
            CapturedEvent::Evdev(event) => self.handle_evdev(event),
        }
    }

    fn handle_evdev(&mut self, event: EvdevKeyEvent) -> Result<(), Box<dyn Error>> {
        if event.device == layoutpilot_core::VIRTUAL_KEYBOARD_NAME {
            // Own uinput output. Capture normally filters the device out, but
            // this also protects against a device opened before injection.
            return Ok(());
        }
        let Some(evdev_keycode) = event.keycode.checked_sub(8) else {
            return Ok(());
        };
        let modifier = self.evdev_modifiers.update(evdev_keycode, event.value);
        let Ok(keycode) = u8::try_from(event.keycode) else {
            return Ok(());
        };
        let x11_event = KeyEvent {
            keycode,
            state: self.evdev_modifiers.state(),
            time: 0,
            pressed: event.pressed(),
        };
        if modifier {
            // Modifiers never reach handle_key, so feed the Shift gesture here.
            if self.paused {
                self.shift_gesture.reset();
            } else if self.config.general.double_shift_fix
                && self
                    .shift_gesture
                    .on_key(keycode, event.pressed(), Instant::now())
            {
                self.manual_fix(&x11_event)?;
            }
            return Ok(());
        }
        self.handle_key(x11_event)
    }

    fn handle_key(&mut self, event: KeyEvent) -> Result<(), Box<dyn Error>> {
        if self.consume_own(&event) {
            return Ok(());
        }
        if event.keycode == RETURN_KEYCODE
            && event.pressed
            && Instant::now() < self.enter_ignore_until
        {
            return Ok(());
        }
        if !event.pressed {
            if self.config.general.double_shift_fix
                && !self.paused
                && self
                    .shift_gesture
                    .on_key(event.keycode, false, Instant::now())
            {
                self.manual_fix(&event)?;
                return Ok(());
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

        if self.config.general.double_shift_fix {
            let _ = self
                .shift_gesture
                .on_key(event.keycode, true, Instant::now());
        }

        if self.pending_space && event.keycode != SPACE_KEYCODE {
            // A key arrived before the space release: preserve input order.
            self.pending_space = false;
            self.handle_char(' ', &event)?;
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

    fn manual_fix(&mut self, event: &KeyEvent) -> Result<(), Box<dyn Error>> {
        if self.manager.undo_applied() {
            if let Some(plan) = self.manager.undo() {
                self.apply(plan)?;
                return Ok(());
            }
        }
        let app_id = self.app_id.clone();
        let context = self.context(event, app_id.as_deref());
        match self.manager.on_fix_last_word(&context) {
            Some(plan) => self.apply(plan)?,
            None => println!("layoutpilot: no word to fix"),
        }
        Ok(())
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
            HotkeyAction::Pause => {
                self.paused = !self.paused;
                if self.paused {
                    self.manager.pause();
                    println!("layoutpilot: paused");
                } else {
                    self.manager.resume();
                    println!("layoutpilot: resumed");
                }
            }
            HotkeyAction::Undo => {
                if self.paused {
                    return Ok(());
                }
                match self.manager.undo() {
                    Some(plan) => self.apply(plan)?,
                    None => println!("layoutpilot: undo unavailable"),
                }
            }
            HotkeyAction::Fix => {
                if self.paused {
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

        let previous_layout = self.active_layout;
        if let Some(delay) = self.pre_inject_delay() {
            std::thread::sleep(delay);
        }
        // Delete first, while the old layout is still active: switching the
        // layout reloads the XWayland keymap and can swallow one Backspace.
        match self.config.inject.delete_mode {
            DeleteMode::Backspace => self.injector.tap_backspace(plan.backspaces)?,
            DeleteMode::CtrlBackspace if plan.backspaces > 0 => {
                self.injector.tap_ctrl_backspace()?;
            }
            DeleteMode::CtrlBackspace => {}
        }
        self.layouts.switch_to(plan.layout)?;
        if previous_layout != Some(plan.layout) || self.layouts.always_switch() {
            std::thread::sleep(self.layout_settle_delay());
        }
        self.injector.tap_text(&plan.text, plan.layout.table())?;
        if let Some(separator) = plan.separator {
            // Send the separator as a separate, delayed key event. Some
            // XWayland clients (Kate/Geany) drop it when it follows a layout
            // switch too closely.
            std::thread::sleep(Duration::from_millis(self.config.inject.separator_delay_ms));
            self.injector
                .tap_text(&separator.to_string(), plan.layout.table())?;
        }
        self.active_layout = Some(plan.layout);
        if previous_layout != Some(plan.layout) {
            self.sound.play();
        }

        self.ignore.extend(self.injector.take_sent_events());
        self.ignore_until = Instant::now() + IGNORE_WINDOW;
        if plan.separator == Some('\n') {
            // Synthetic Return can be captured again and start a second
            // correction. Ignore Return presses for a short window.
            self.enter_ignore_until = Instant::now() + Duration::from_millis(300);
        }
        println!(
            "layoutpilot: {:?} → layout {}, {} backspaces, {:?}",
            plan.kind, plan.layout, plan.backspaces, plan.text
        );
        Ok(())
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
            if self.active_layout.is_some() && active.is_some() {
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

    fn refresh_focus(&mut self, force: bool) {
        if !force && self.last_focus_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_focus_poll = Instant::now();

        match self.focus.snapshot() {
            Ok(info) => {
                let window_changed = self.window != Some(info.window);
                let app_changed = self.app_id != info.app_id;
                self.window = Some(info.window);
                self.app_id = info.app_id;
                self.profile = self.resolver.resolve(self.app_id.as_deref().unwrap_or(""));
                if window_changed || app_changed {
                    println!(
                        "layoutpilot: focus window={:#x}, app={:?}, profile={:?}",
                        info.window, self.app_id, self.profile
                    );
                    if self.clear_on_focus_change {
                        self.manager.reset(ResetReason::FocusChange);
                    }
                }
            }
            Err(_) => {
                if self.window.take().is_some() {
                    self.app_id = None;
                    self.profile = Profile::Auto;
                    if self.clear_on_focus_change {
                        self.manager.reset(ResetReason::FocusChange);
                    }
                }
            }
        }
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
                Ok(LayoutController::X11(X11LayoutState::new(
                    config.layout_order(),
                )?))
            }
            Err(error) => Err(error.into()),
        },
        LayoutBackend::Sway => Ok(LayoutController::Sway(SwayLayouts::new()?)),
        LayoutBackend::Hyprland => Ok(LayoutController::Hyprland(HyprlandLayouts::new()?)),
        LayoutBackend::X11 => Ok(LayoutController::X11(X11LayoutState::new(
            config.layout_order(),
        )?)),
        other => Err(format!("layout backend {other:?} is not wired yet").into()),
    }
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

    let detector = DictionaryDetector::new();
    println!(
        "dictionary: ru(привет)={}, en(hello)={}",
        detector.dict_count(Language::Ru, "привет"),
        detector.dict_count(Language::En, "hello")
    );

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
        assert!(!gesture.on_key(50, true, start));
        assert!(!gesture.on_key(50, false, start + Duration::from_millis(20)));
        assert!(!gesture.on_key(50, true, start + Duration::from_millis(40)));
        assert!(gesture.on_key(50, false, start + Duration::from_millis(60)));
        // The sequence is consumed, a third tap alone must not fire.
        assert!(!gesture.on_key(50, true, start + Duration::from_millis(80)));
        assert!(!gesture.on_key(50, false, start + Duration::from_millis(100)));
    }

    #[test]
    fn double_shift_gesture_breaks_on_other_key() {
        let start = Instant::now();
        let mut gesture = ShiftGesture::default();
        assert!(!gesture.on_key(50, true, start));
        assert!(!gesture.on_key(50, false, start + Duration::from_millis(20)));
        assert!(!gesture.on_key(38, true, start + Duration::from_millis(30)));
        assert!(!gesture.on_key(38, false, start + Duration::from_millis(40)));
        assert!(!gesture.on_key(50, true, start + Duration::from_millis(50)));
        assert!(!gesture.on_key(50, false, start + Duration::from_millis(60)));
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
