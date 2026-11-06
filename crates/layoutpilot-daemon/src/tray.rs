//! System tray icon (StatusNotifierItem): pause/resume and a visible signal
//! that a correction happened.
//!
//! The tray runs on ksni's own thread. Commands go to the daemon through a
//! channel that the main loop polls; the daemon pushes state back with
//! [`TrayHandle::update`].

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip};

/// How long the icon stays highlighted after a correction.
pub(crate) const FLASH: Duration = Duration::from_millis(1500);

/// A request from the tray to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayCommand {
    TogglePause,
    ToggleTerminals,
    Quit,
}

/// What the tray shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrayState {
    pub paused: bool,
    /// Highlight the icon: a correction was just applied.
    pub flash: bool,
    pub corrections: u64,
    /// `typed → converted` of the last correction, shown in the menu and tooltip.
    pub last: Option<String>,
    pub pause_hotkey: String,
    /// Terminals (the `manual` profile) are corrected automatically.
    pub terminals_auto: bool,
}

struct LayoutPilotTray {
    state: TrayState,
    commands: Sender<TrayCommand>,
}

impl LayoutPilotTray {
    fn status_line(&self) -> String {
        if self.state.paused {
            "LayoutPilot — на паузе".to_owned()
        } else {
            "LayoutPilot — активен".to_owned()
        }
    }

    fn send(&self, command: TrayCommand) {
        let _ = self.commands.send(command);
    }
}

impl ksni::Tray for LayoutPilotTray {
    fn id(&self) -> String {
        "layoutpilot".to_owned()
    }

    fn title(&self) -> String {
        "LayoutPilot".to_owned()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let style = if self.state.paused {
            IconStyle::Paused
        } else if self.state.flash {
            IconStyle::Flash
        } else {
            IconStyle::Active
        };
        [22, 32, 48, 64]
            .into_iter()
            .map(|size| draw_icon(size, style))
            .collect()
    }

    fn tool_tip(&self) -> ToolTip {
        let mut description = format!("Исправлений: {}", self.state.corrections);
        if let Some(last) = &self.state.last {
            description.push_str(&format!("\nПоследнее: {last}"));
        }
        description.push_str("\nЩелчок — пауза/продолжить");
        ToolTip {
            title: self.status_line(),
            description,
            ..Default::default()
        }
    }

    /// Left click toggles pause.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::TogglePause);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: self.status_line(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Исправлений: {}", self.state.corrections),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ];
        if let Some(last) = &self.state.last {
            items.push(
                StandardItem {
                    label: format!("Последнее: {last}"),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        let pause_label = if self.state.pause_hotkey.is_empty() {
            "Пауза".to_owned()
        } else {
            format!("Пауза ({})", self.state.pause_hotkey)
        };
        items.extend([
            MenuItem::Separator,
            CheckmarkItem {
                label: pause_label,
                checked: self.state.paused,
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::TogglePause)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Автоисправление в терминалах".to_owned(),
                checked: self.state.terminals_auto,
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::ToggleTerminals)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Выход".to_owned(),
                icon_name: "application-exit".to_owned(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }

    /// The panel may start after the daemon (or restart): keep waiting for it.
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        true
    }
}

/// The daemon's side of the tray.
pub(crate) struct TrayHandle {
    handle: ksni::blocking::Handle<LayoutPilotTray>,
    commands: Receiver<TrayCommand>,
    shown: TrayState,
    flash_until: Option<Instant>,
}

impl TrayHandle {
    /// Register the tray icon.
    pub(crate) fn spawn(state: TrayState) -> Result<Self, ksni::Error> {
        let (sender, commands) = channel();
        let tray = LayoutPilotTray {
            state: state.clone(),
            commands: sender,
        };
        let handle = tray.assume_sni_available(true).spawn()?;
        Ok(Self {
            handle,
            commands,
            shown: state,
            flash_until: None,
        })
    }

    /// Next pending command, if any. Never blocks.
    pub(crate) fn try_command(&self) -> Option<TrayCommand> {
        self.commands.try_recv().ok()
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        self.update(|state| state.paused = paused);
    }

    pub(crate) fn set_terminals_auto(&mut self, enabled: bool) {
        self.update(|state| state.terminals_auto = enabled);
    }

    /// Count a correction and highlight the icon for [`FLASH`].
    pub(crate) fn correction(&mut self, typed: &str, converted: &str) {
        self.flash_until = Some(Instant::now() + FLASH);
        let last = format!("{typed} → {converted}");
        self.update(|state| {
            state.corrections += 1;
            state.flash = true;
            state.last = Some(last);
        });
    }

    /// Clear the highlight once it has expired. Call from the main loop.
    pub(crate) fn tick(&mut self) {
        if self
            .flash_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.flash_until = None;
            self.update(|state| state.flash = false);
        }
    }

    fn update(&mut self, change: impl FnOnce(&mut TrayState)) {
        let mut next = self.shown.clone();
        change(&mut next);
        if next == self.shown {
            return;
        }
        self.shown = next.clone();
        // ksni diffs the properties and emits the D-Bus signals.
        self.handle.update(move |tray| tray.state = next);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconStyle {
    Active,
    Paused,
    Flash,
}

/// A keyboard glyph: a rounded tile with two rows of keys and a space bar.
fn draw_icon(size: i32, style: IconStyle) -> Icon {
    let (background, keys): ([u8; 3], [u8; 3]) = match style {
        IconStyle::Active => ([0x2d, 0x7d, 0xd2], [0xff, 0xff, 0xff]),
        IconStyle::Paused => ([0x80, 0x80, 0x80], [0xd0, 0xd0, 0xd0]),
        IconStyle::Flash => ([0x2e, 0xa0, 0x43], [0xff, 0xff, 0xff]),
    };
    let s = f64::from(size);
    let unit = s / 16.0;
    let tile = Rect::new(0.5 * unit, 2.5 * unit, 15.0 * unit, 11.0 * unit, 2.0 * unit);
    let mut key_rects = Vec::new();
    for (row, count) in [(0.0, 5), (1.0, 5)] {
        for column in 0..count {
            let x = 2.0 + f64::from(column) * 2.5;
            let y = 4.5 + row * 2.6;
            key_rects.push(Rect::new(
                x * unit,
                y * unit,
                1.8 * unit,
                1.8 * unit,
                0.3 * unit,
            ));
        }
    }
    key_rects.push(Rect::new(
        4.5 * unit,
        9.8 * unit,
        7.0 * unit,
        1.8 * unit,
        0.3 * unit,
    ));

    let mut data = Vec::with_capacity(usize::try_from(size * size * 4).unwrap_or(0));
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
            let pixel = if key_rects.iter().any(|key| key.contains(px, py)) {
                Some(keys)
            } else if tile.contains(px, py) {
                Some(background)
            } else {
                None
            };
            match pixel {
                // ARGB32, network byte order.
                Some([r, g, b]) => data.extend([0xff, r, g, b]),
                None => data.extend([0, 0, 0, 0]),
            }
        }
    }
    Icon {
        width: size,
        height: size,
        data,
    }
}

struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
}

impl Rect {
    const fn new(x: f64, y: f64, width: f64, height: f64, radius: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
            radius,
        }
    }

    fn contains(&self, px: f64, py: f64) -> bool {
        if px < self.x || py < self.y || px > self.x + self.width || py > self.y + self.height {
            return false;
        }
        // Distance to the nearest corner centre for rounded corners.
        let cx = px.clamp(self.x + self.radius, self.x + self.width - self.radius);
        let cy = py.clamp(self.y + self.radius, self.y + self.height - self.radius);
        (px - cx).powi(2) + (py - cy).powi(2) <= self.radius.powi(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_have_argb_data_for_every_pixel() {
        for style in [IconStyle::Active, IconStyle::Paused, IconStyle::Flash] {
            let icon = draw_icon(32, style);
            assert_eq!(icon.data.len(), 32 * 32 * 4);
            // Corners are transparent, the centre of the tile is opaque.
            assert_eq!(icon.data[0], 0);
            assert!(icon.data.chunks(4).any(|pixel| pixel[0] == 0xff));
        }
    }

    #[test]
    fn styles_differ() {
        assert_ne!(
            draw_icon(22, IconStyle::Active).data,
            draw_icon(22, IconStyle::Paused).data
        );
        assert_ne!(
            draw_icon(22, IconStyle::Active).data,
            draw_icon(22, IconStyle::Flash).data
        );
    }
}
