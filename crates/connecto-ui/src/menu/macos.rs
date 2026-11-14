//! The native macOS menu bar, built from [`super::menus`].
//!
//! `muda` builds an `NSMenu` and hands us its clicks through a channel. The
//! menu is installed from the eframe app creator, which runs after winit has
//! finished launching and made its own default menu, so ours replaces it. Menu
//! events are read on the UI thread in `App::logic`.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, channel};

use connecto_core::RecentProject;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{
    AboutMetadata, CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};

use super::{CLEAR_RECENT, Chord, Entry, NO_RECENTS, RecentEntry, Role, System};
use crate::commands::Command;

pub(crate) struct NativeMenu {
    /// Kept alive: dropping the menu removes it.
    _menu: Menu,
    commands: HashMap<MenuId, Command>,
    events: Receiver<MenuId>,
    recent_menu: Submenu,
    recent_ids: Vec<MenuId>,
    clear_id: Option<MenuId>,
    recents: Option<Vec<RecentProject>>,
    pause: CheckMenuItem,
    paused: Option<bool>,
}

/// The key a chord's key is on.
fn code(key: egui::Key) -> Option<Code> {
    use egui::Key;
    Some(match key {
        Key::A => Code::KeyA,
        Key::B => Code::KeyB,
        Key::C => Code::KeyC,
        Key::D => Code::KeyD,
        Key::E => Code::KeyE,
        Key::F => Code::KeyF,
        Key::G => Code::KeyG,
        Key::H => Code::KeyH,
        Key::I => Code::KeyI,
        Key::J => Code::KeyJ,
        Key::K => Code::KeyK,
        Key::L => Code::KeyL,
        Key::M => Code::KeyM,
        Key::N => Code::KeyN,
        Key::O => Code::KeyO,
        Key::P => Code::KeyP,
        Key::Q => Code::KeyQ,
        Key::R => Code::KeyR,
        Key::S => Code::KeyS,
        Key::T => Code::KeyT,
        Key::U => Code::KeyU,
        Key::V => Code::KeyV,
        Key::W => Code::KeyW,
        Key::X => Code::KeyX,
        Key::Y => Code::KeyY,
        Key::Z => Code::KeyZ,
        Key::Num0 => Code::Digit0,
        Key::Num1 => Code::Digit1,
        Key::Num2 => Code::Digit2,
        Key::Num3 => Code::Digit3,
        Key::Num4 => Code::Digit4,
        Key::Num5 => Code::Digit5,
        Key::Num6 => Code::Digit6,
        Key::Num7 => Code::Digit7,
        Key::Num8 => Code::Digit8,
        Key::Num9 => Code::Digit9,
        Key::Comma => Code::Comma,
        Key::Period => Code::Period,
        Key::Backslash => Code::Backslash,
        Key::Tab => Code::Tab,
        _ => return None,
    })
}

/// A chord as the menu's key equivalent. Cmd is `META` (Super).
fn accel(chord: Chord) -> Option<Accelerator> {
    let mods = match chord {
        Chord::Cmd(_) => Modifiers::META,
        Chord::CmdShift(_) => Modifiers::META | Modifiers::SHIFT,
        Chord::CmdAlt(_) => Modifiers::META | Modifiers::ALT,
        Chord::Ctrl(_) => Modifiers::CONTROL,
        Chord::CtrlShift(_) => Modifiers::CONTROL | Modifiers::SHIFT,
    };
    Some(Accelerator::new(mods, code(chord.key())?))
}

fn system(item: System) -> PredefinedMenuItem {
    match item {
        System::About => PredefinedMenuItem::about(
            None,
            Some(AboutMetadata {
                copyright: Some("Copyright © 2026 Marco Manzi. AGPL-3.0-only.".to_owned()),
                credits: Some(
                    "Free software, community-funded.\nSource: https://github.com/marcomanzi/connecto-community\nhttps://connecto.studio".to_owned(),
                ),
                ..AboutMetadata::default()
            }),
        ),
        System::Hide => PredefinedMenuItem::hide(None),
        System::HideOthers => PredefinedMenuItem::hide_others(None),
        System::ShowAll => PredefinedMenuItem::show_all(None),
        System::Quit => PredefinedMenuItem::quit(None),
        System::Minimize => PredefinedMenuItem::minimize(None),
        System::Maximize => PredefinedMenuItem::maximize(None),
    }
}

impl NativeMenu {
    /// Build the menu and make it the application's. Call on the main
    /// thread, after the window exists.
    pub(crate) fn install(ctx: &egui::Context) -> Option<Self> {
        let (tx, events) = channel();
        let repaint = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = tx.send(event.id);
            repaint.request_repaint();
        }));

        let mut commands = HashMap::new();
        let mut recent_menu = None;
        let mut pause = None;
        let mut window = None;
        let menu = Menu::new();
        for spec in super::menus() {
            let submenu = Submenu::new(spec.title, true);
            for entry in spec.entries {
                match entry {
                    Entry::Item {
                        label,
                        command,
                        chord,
                    } => {
                        let item = MenuItem::new(label, true, chord.and_then(accel));
                        commands.insert(item.id().clone(), command);
                        submenu.append(&item).ok()?;
                    }
                    Entry::Pause { label, chord } => {
                        let item = CheckMenuItem::new(label, true, false, chord.and_then(accel));
                        commands.insert(item.id().clone(), Command::TogglePause);
                        submenu.append(&item).ok()?;
                        pause = Some(item);
                    }
                    Entry::Recent { label } => {
                        let recent = Submenu::new(label, true);
                        submenu.append(&recent).ok()?;
                        recent_menu = Some(recent);
                    }
                    Entry::Separator => {
                        submenu.append(&PredefinedMenuItem::separator()).ok()?;
                    }
                    Entry::System(item) => submenu.append(&system(item)).ok()?,
                }
            }
            menu.append(&submenu).ok()?;
            if spec.role == Role::Window {
                window = Some(submenu);
            }
        }
        menu.init_for_nsapp();
        if let Some(window) = &window {
            window.set_as_windows_menu_for_nsapp();
        }
        Some(Self {
            _menu: menu,
            commands,
            events,
            recent_menu: recent_menu?,
            recent_ids: Vec::new(),
            clear_id: None,
            recents: None,
            pause: pause?,
            paused: None,
        })
    }

    /// Clicks since the last call.
    pub(crate) fn poll(&mut self) -> Vec<Command> {
        self.events
            .try_iter()
            .filter_map(|id| {
                if let Some(index) = self.recent_ids.iter().position(|recent| *recent == id) {
                    return Some(Command::OpenRecent(index));
                }
                self.commands.get(&id).cloned()
            })
            .collect()
    }

    /// Bring the Open Recent list and the Pause check mark up to date.
    pub(crate) fn sync(&mut self, recents: &[RecentProject], paused: bool) {
        if self.paused != Some(paused) {
            self.pause.set_checked(paused);
            self.paused = Some(paused);
        }
        if self.recents.as_deref() == Some(recents) {
            return;
        }
        while self.recent_menu.remove_at(0).is_some() {}
        self.recent_ids.clear();
        if let Some(old) = self.clear_id.take() {
            self.commands.remove(&old);
        }
        for entry in super::recent_entries(recents) {
            match entry {
                RecentEntry::Empty => {
                    let none = MenuItem::new(NO_RECENTS, false, None);
                    let _ = self.recent_menu.append(&none);
                }
                RecentEntry::Project { name, .. } => {
                    let item = MenuItem::new(name, true, None);
                    self.recent_ids.push(item.id().clone());
                    let _ = self.recent_menu.append(&item);
                }
                RecentEntry::Separator => {
                    let _ = self.recent_menu.append(&PredefinedMenuItem::separator());
                }
                RecentEntry::Clear => {
                    let clear = MenuItem::new(CLEAR_RECENT, true, None);
                    self.commands
                        .insert(clear.id().clone(), Command::ClearRecents);
                    self.clear_id = Some(clear.id().clone());
                    let _ = self.recent_menu.append(&clear);
                }
            }
        }
        self.recents = Some(recents.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shortcut in the menu has a key equivalent.
    #[test]
    fn every_chord_has_a_key_equivalent() {
        for menu in super::super::menus() {
            for entry in menu.entries {
                if let Entry::Item {
                    chord: Some(chord), ..
                }
                | Entry::Pause {
                    chord: Some(chord), ..
                } = entry
                {
                    assert!(accel(chord).is_some(), "{chord:?}");
                }
            }
        }
    }
}
