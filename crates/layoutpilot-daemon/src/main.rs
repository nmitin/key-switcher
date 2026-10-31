//! `layoutpilot` daemon for the X11 vertical slice.
//!
//! Data flow: XRecord key events → XKB layout lookup → character
//! normalization → `CorrectionManager` (engine + dictionaries) → XTest
//! injection with an XKB group switch.
//!
//! This is deliberately small: no config file, no tray, no global hotkeys yet.
//! `--dry-run` prints the plan without injecting anything.

use std::collections::VecDeque;
use std::env;
use std::error::Error;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use layoutpilot_core::{
    AppRule, CorrectionManager, CorrectionPlan, EngineConfig, InputContext, LayoutId, Profile,
    ProfileResolver, ResetReason,
};
use layoutpilot_dict::{DictionaryDetector, Language};
use layoutpilot_x11::{
    char_for, modifiers, KeyEvent, X11Error, X11Focus, X11Injector, X11LayoutState, XRecordCapture,
};

const BACKSPACE_KEYCODE: u8 = 22;
const RETURN_KEYCODE: u8 = 36;
const TAB_KEYCODE: u8 = 23;
const ESCAPE_KEYCODE: u8 = 9;
const IGNORE_WINDOW: Duration = Duration::from_millis(750);

struct Daemon {
    capture: XRecordCapture,
    injector: X11Injector,
    layouts: X11LayoutState,
    focus: X11Focus,
    manager: CorrectionManager<DictionaryDetector>,
    resolver: ProfileResolver,
    active_layout: Option<LayoutId>,
    profile: Profile,
    app_id: Option<String>,
    window: Option<u32>,
    last_focus_poll: Instant,
    ignore: VecDeque<(u8, bool)>,
    ignore_until: Instant,
    dry_run: bool,
}

impl Daemon {
    fn new(dry_run: bool) -> Result<Self, Box<dyn Error>> {
        let capture = XRecordCapture::new()?;
        let injector = X11Injector::new()?;
        let layouts = X11LayoutState::new(vec![LayoutId::Us, LayoutId::Ru])?;
        let focus = X11Focus::new()?;
        let active_layout = layouts.active_layout().ok().flatten();

        let mut daemon = Self {
            capture,
            injector,
            layouts,
            focus,
            manager: CorrectionManager::new(EngineConfig::default(), DictionaryDetector::new()),
            resolver: default_resolver(),
            active_layout,
            profile: Profile::Auto,
            app_id: None,
            window: None,
            last_focus_poll: Instant::now() - Duration::from_secs(2),
            ignore: VecDeque::new(),
            ignore_until: Instant::now(),
            dry_run,
        };
        daemon.refresh_focus(true);
        Ok(daemon)
    }

    fn run(mut self) -> Result<(), Box<dyn Error>> {
        println!(
            "layoutpilot: X11 capture active, dry_run={}, layout={:?}, app={:?}",
            self.dry_run, self.active_layout, self.app_id
        );
        loop {
            if Instant::now() >= self.ignore_until {
                self.ignore.clear();
            }

            match self.capture.recv_timeout(Duration::from_millis(200)) {
                Ok(Some(event)) => {
                    self.refresh_layout();
                    self.refresh_focus(false);
                    self.handle_key(event)?;
                }
                Ok(None) => {
                    self.refresh_layout();
                    self.refresh_focus(false);
                }
                Err(X11Error::CaptureStopped) => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, event: KeyEvent) -> Result<(), Box<dyn Error>> {
        if self.consume_own(&event) {
            return Ok(());
        }
        if !event.pressed {
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
        if !self.layouts.layouts().contains(&plan.layout) {
            eprintln!(
                "layoutpilot: skip plan, layout {} is not configured",
                plan.layout
            );
            self.manager.reset(ResetReason::Manual);
            return Ok(());
        }

        self.injector.tap_backspace(plan.backspaces)?;
        self.layouts.switch_to(plan.layout)?;
        self.injector.tap_text(&plan.text, plan.layout.table())?;
        self.active_layout = Some(plan.layout);

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
            self.active_layout = active;
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
                    self.manager.reset(ResetReason::FocusChange);
                }
            }
            Err(_) => {
                if self.window.take().is_some() {
                    self.app_id = None;
                    self.profile = Profile::Auto;
                    self.manager.reset(ResetReason::FocusChange);
                }
            }
        }
    }
}

fn default_resolver() -> ProfileResolver {
    ProfileResolver::new(Profile::Auto)
        .with_rule(AppRule::new("*terminal*", Profile::Manual))
        .with_rule(AppRule::new("*konsole*", Profile::Manual))
        .with_rule(AppRule::new("*alacritty*", Profile::Manual))
        .with_rule(AppRule::new("*kitty*", Profile::Manual))
        .with_rule(AppRule::new("*keepass*", Profile::Off))
}

fn doctor() -> Result<(), Box<dyn Error>> {
    let mut failures = 0;

    println!("LayoutPilot doctor");
    println!("DISPLAY: {:?}", env::var("DISPLAY").ok());

    match X11Injector::new() {
        Ok(_) => println!("XTEST: ok"),
        Err(error) => {
            failures += 1;
            println!("XTEST: FAIL ({error})");
        }
    }

    match X11LayoutState::new(vec![LayoutId::Us, LayoutId::Ru]) {
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

    match X11Focus::new() {
        Ok(focus) => match focus.snapshot() {
            Ok(info) => println!("focus: window={:#x}, app={:?}", info.window, info.app_id),
            Err(error) => println!("focus: unavailable ({error})"),
        },
        Err(error) => println!("focus: FAIL ({error})"),
    }

    match XRecordCapture::new() {
        Ok(mut capture) => {
            println!("RECORD: ok");
            if let Err(error) = capture.stop() {
                println!("RECORD stop: FAIL ({error})");
            }
        }
        Err(error) => {
            failures += 1;
            println!("RECORD: FAIL ({error})");
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
         \x20 layoutpilot doctor          check X11 backends and dictionaries\n\
         \x20 layoutpilot run             run the daemon (default)\n\
         \x20 layoutpilot run --dry-run   print plans without injecting\n\n\
         Set DISPLAY to the target X11 session."
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.as_slice() {
        [] => Daemon::new(false).and_then(Daemon::run),
        [command] if command == "doctor" => doctor(),
        [command] if command == "run" => Daemon::new(false).and_then(Daemon::run),
        [command, flag] if command == "run" && flag == "--dry-run" => {
            Daemon::new(true).and_then(Daemon::run)
        }
        [command] if command == "-h" || command == "--help" => {
            print_help();
            Ok(())
        }
        _ => {
            print_help();
            return ExitCode::from(2);
        }
    };

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
        let resolver = default_resolver();
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
}
