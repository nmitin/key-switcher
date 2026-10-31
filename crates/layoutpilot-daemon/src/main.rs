//! `layoutpilot` daemon for the X11 vertical slice.
//!
//! Data flow: XRecord key events → XKB layout lookup → character
//! normalization → `CorrectionManager` (engine + dictionaries) → XTest
//! injection with an XKB group switch.
//!
//! The daemon loads `~/.config/layoutpilot/config.toml` (or `--config PATH`)
//! and falls back to built-in defaults. Hot reload, global hotkeys, tray and
//! evdev capture are not wired yet.

use std::collections::{HashSet, VecDeque};
use std::env;
use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::time::{Duration, Instant};

use layoutpilot_capture::{EvdevCapture, EvdevKeyEvent};
use layoutpilot_compositor::KdeLayouts;
use layoutpilot_config::{CaptureBackend, Config, Hotkey, InjectBackend, LayoutBackend};
use layoutpilot_core::{
    CorrectionManager, CorrectionPlan, InputContext, LayoutId, Profile, ProfileResolver,
    ResetReason,
};
use layoutpilot_dict::{DictionaryDetector, Language};
use layoutpilot_x11::{
    char_for, modifiers, KeyEvent, X11Focus, X11HotkeyGrabber, X11Injector, X11LayoutState,
    XRecordCapture,
};

const BACKSPACE_KEYCODE: u8 = 22;
const RETURN_KEYCODE: u8 = 36;
const TAB_KEYCODE: u8 = 23;
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
}

impl SoundPlayer {
    const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            failed: false,
        }
    }

    fn play(&mut self) {
        if !self.enabled || self.failed {
            return;
        }
        const SOUND: &str = "/usr/share/sounds/freedesktop/stereo/dialog-information.oga";
        if std::path::Path::new(SOUND).exists() && self.spawn("paplay", &[SOUND]) {
            return;
        }
        if self.spawn("pw-play", &[SOUND]) {
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
}

impl LayoutController {
    fn name(&self) -> &'static str {
        match self {
            Self::X11(_) => "x11-xkb",
            Self::Kde { .. } => "kde-dbus+xkb",
        }
    }

    fn active_layout(&self) -> Result<Option<LayoutId>, Box<dyn Error>> {
        match self {
            Self::X11(state) => Ok(state.active_layout()?),
            Self::Kde { kde, .. } => Ok(kde.current()?),
        }
    }

    fn switch_to(&self, layout: LayoutId) -> Result<(), Box<dyn Error>> {
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
        }
        Ok(())
    }

    fn supports(&self, layout: LayoutId) -> bool {
        match self {
            Self::X11(state) => state.layouts().contains(&layout),
            Self::Kde { kde, .. } => kde.supports(layout),
        }
    }

    fn sync_xkb(&self, layout: LayoutId) {
        if let Self::Kde { xkb: Some(xkb), .. } = self {
            if let Err(error) = xkb.switch_to(layout) {
                eprintln!("layoutpilot: cannot sync XKB group: {error}");
            }
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
    injector: X11Injector,
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
    evdev_modifiers: EvdevModifiers,
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
        let sound = SoundPlayer::new(config.general.sound_on_layout_change);
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
            evdev_modifiers: EvdevModifiers::default(),
            dry_run,
        };
        daemon.refresh_focus(true);
        Ok(daemon)
    }

    fn run(mut self) -> Result<(), Box<dyn Error>> {
        println!(
            "layoutpilot: capture={} ({}), layout={} ({:?}), dry_run={}, app={:?}",
            self.capture.backend_name(),
            config_backend_name(self.config.capture.backend),
            self.layouts.name(),
            self.active_layout,
            self.dry_run,
            self.app_id
        );
        loop {
            self.poll_hotkeys()?;
            if Instant::now() >= self.ignore_until {
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
        let Some(evdev_keycode) = event.keycode.checked_sub(8) else {
            return Ok(());
        };
        if self.evdev_modifiers.update(evdev_keycode, event.value) {
            return Ok(());
        }
        let Ok(keycode) = u8::try_from(event.keycode) else {
            return Ok(());
        };
        let x11_event = KeyEvent {
            keycode,
            state: self.evdev_modifiers.state(),
            time: 0,
            pressed: event.pressed(),
        };
        self.handle_key(x11_event)
    }

    fn handle_key(&mut self, event: KeyEvent) -> Result<(), Box<dyn Error>> {
        if self.consume_own(&event) {
            return Ok(());
        }
        if !event.pressed {
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
            return Ok(());
        }

        match event.keycode {
            BACKSPACE_KEYCODE => {
                self.manager.on_backspace();
                return Ok(());
            }
            RETURN_KEYCODE => {
                self.handle_char('\n', &event)?;
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
        self.injector.tap_backspace(plan.backspaces)?;
        self.layouts.switch_to(plan.layout)?;
        self.injector.tap_text(&plan.text, plan.layout.table())?;
        if let Some(separator) = plan.separator {
            // Give the application a moment to finish the replacement before
            // sending the separator as a separate key event. This avoids the
            // final space being lost in the same fast XTest batch.
            std::thread::sleep(Duration::from_millis(2));
            self.injector
                .tap_text(&separator.to_string(), plan.layout.table())?;
        }
        self.active_layout = Some(plan.layout);
        if previous_layout != Some(plan.layout) {
            self.sound.play();
        }

        self.ignore.extend(self.injector.take_sent_events());
        self.ignore_until = Instant::now() + IGNORE_WINDOW;
        println!(
            "layoutpilot: {:?} → layout {}, {} backspaces, {:?}",
            plan.kind, plan.layout, plan.backspaces, plan.text
        );
        Ok(())
    }

    fn consume_own(&mut self, event: &KeyEvent) -> bool {
        let Some(&(keycode, pressed)) = self.ignore.front() else {
            return false;
        };
        if keycode == event.keycode && pressed == event.pressed {
            self.ignore.pop_front();
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

fn open_layouts(config: &Config) -> Result<LayoutController, Box<dyn Error>> {
    let requested = config.layout.backend;
    let backend = match requested {
        LayoutBackend::Auto if is_kde_session() => LayoutBackend::Kde,
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
        LayoutBackend::X11 => Ok(LayoutController::X11(X11LayoutState::new(
            config.layout_order(),
        )?)),
        other => Err(format!("layout backend {other:?} is not wired yet").into()),
    }
}

fn open_capture(config: &Config) -> Result<Capture, Box<dyn Error>> {
    let backend = match config.capture.backend {
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

fn open_injector(config: &Config) -> Result<X11Injector, Box<dyn Error>> {
    match config.inject.backend {
        InjectBackend::Auto | InjectBackend::Xtest => Ok(X11Injector::new()?),
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

    println!("LayoutPilot doctor");
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
            hotkeys.action(
                41,
                layoutpilot_config::hotkey::MOD_CONTROL | layoutpilot_config::hotkey::MOD_SHIFT
            ),
            Some(HotkeyAction::Fix)
        );
        assert_eq!(
            hotkeys.action(
                33,
                layoutpilot_config::hotkey::MOD_CONTROL | layoutpilot_config::hotkey::MOD_SHIFT
            ),
            Some(HotkeyAction::Pause)
        );
        assert_eq!(hotkeys.action(41, 0), None);
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
