//! The menu bar inside the main window, where there is no system menu bar
//! (Windows). It shows [`super::in_window_menus`]: the macOS menu's commands,
//! in its order, each sent the way a macOS menu click is (`App::queued`).
//!
//! egui's menu bar does the opening, the switching between menus on hover,
//! Open Recent's submenu and the closing (on a click, a click outside or
//! Escape). The rows are drawn here, in the sidebar's colours. While a menu
//! is open the terminal gets none of the frame's events
//! ([`InWindowBar::blocks_input`]), so neither the keys that steer the menu
//! nor the click that closes it reach a program.

use connecto_core::RecentProject;
use egui::containers::menu::{MenuBar, MenuButton, MenuConfig, SubMenu, menu_style};
use egui::{
    Button, CornerRadius, FontId, Margin, Popup, Rect, Response, RichText, Sense, Shape, Stroke,
    Style, Ui, UiBuilder, pos2, vec2,
};

use super::{
    BarOut, CLEAR_RECENT, Entry, Menu, NO_RECENTS, RecentEntry, in_window_menus,
    in_window_shortcut, recent_entries,
};
use crate::commands::Command;
use crate::icons;
use crate::theme;

/// The bar's height.
pub(crate) const BAR_HEIGHT: f32 = 26.0;
const TITLE_HEIGHT: f32 = 20.0;
const ROW_HEIGHT: f32 = 24.0;
const ROW_PAD: f32 = 10.0;
/// Room left of the labels for a check mark.
const CHECK_GUTTER: f32 = 24.0;
/// Least room between a label and its shortcut.
const SHORTCUT_GAP: f32 = 32.0;
const MIN_WIDTH: f32 = 180.0;
const SEPARATOR_HEIGHT: f32 = 9.0;
const TITLE_SIZE: f32 = 12.5;
const ITEM_SIZE: f32 = 12.5;
const SHORTCUT_SIZE: f32 = 11.5;

/// A control the bar drew, for tests.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BarHit {
    Title(&'static str),
    Row {
        label: String,
        shortcut: Option<String>,
        enabled: bool,
        checked: bool,
    },
    Separator,
}

pub(crate) struct InWindowBar {
    menus: Vec<Menu>,
    /// A menu was open when the last pass ended.
    open: bool,
    /// A menu was open when this pass began.
    open_at_start: bool,
    #[cfg(test)]
    pub(crate) hits: Vec<(BarHit, Rect)>,
}

impl Default for InWindowBar {
    fn default() -> Self {
        Self {
            menus: in_window_menus(),
            open: false,
            open_at_start: false,
            #[cfg(test)]
            hits: Vec::new(),
        }
    }
}

/// The titles: quiet text that lights up under the pointer.
fn title_style(style: &mut Style) {
    menu_style(style);
    style.spacing.button_padding = vec2(8.0, 2.0);
    style.spacing.item_spacing = vec2(2.0, 0.0);
    style.spacing.interact_size.y = TITLE_HEIGHT;
    let widgets = &mut style.visuals.widgets;
    widgets.inactive.fg_stroke = Stroke::new(1.0, theme::TEXT_ROW);
    widgets.hovered.weak_bg_fill = theme::RAISED_BG;
    widgets.hovered.fg_stroke = Stroke::new(1.0, theme::TEXT);
    widgets.active.weak_bg_fill = theme::TERM_ON;
    widgets.active.fg_stroke = Stroke::new(1.0, theme::TEXT);
    for state in [
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        state.corner_radius = CornerRadius::same(5);
    }
}

/// The open menus: the app's popover surface.
fn popup_style(style: &mut Style) {
    menu_style(style);
    style.visuals.window_fill = theme::POPOVER_BG;
    style.visuals.window_stroke = Stroke::new(1.0, theme::POPOVER_BORDER);
    style.visuals.menu_corner_radius = CornerRadius::same(theme::RADIUS);
    style.spacing.menu_margin = Margin::same(4);
    style.spacing.item_spacing = vec2(0.0, 0.0);
}

impl InWindowBar {
    /// Draw the bar along the top of `area`. Clicked items come back as
    /// commands, to run as a macOS menu click is.
    pub(crate) fn draw(
        &mut self,
        ui: &mut Ui,
        area: Rect,
        recents: &[RecentProject],
        paused: bool,
    ) -> BarOut {
        #[cfg(test)]
        self.hits.clear();
        self.open_at_start = self.open;
        let (bar, rest) = area.split_top_bottom_at_y(area.min.y + BAR_HEIGHT);
        let painter = ui.painter_at(bar);
        painter.rect_filled(bar, 0.0, theme::SIDEBAR_BG);
        painter.hline(
            bar.x_range(),
            bar.max.y - 0.5,
            Stroke::new(1.0, theme::SIDEBAR_BORDER),
        );

        let mut commands = Vec::new();
        let mut popups = Vec::new();
        let strip = Rect::from_min_max(
            pos2(bar.min.x + 6.0, bar.center().y - TITLE_HEIGHT / 2.0),
            pos2(bar.max.x - 6.0, bar.center().y + TITLE_HEIGHT / 2.0),
        );
        let menus = &self.menus;
        #[cfg(test)]
        let hits = &mut self.hits;
        ui.scope_builder(UiBuilder::new().max_rect(strip), |ui| {
            MenuBar::new()
                .style(title_style)
                .config(MenuConfig::new().style(popup_style))
                .ui(ui, |ui| {
                    for menu in menus {
                        // The title of the open menu stays lit while the
                        // pointer is in the menu.
                        let lit = ui.painter().add(Shape::Noop);
                        let button = Button::new(RichText::new(menu.title).size(TITLE_SIZE));
                        let (response, _) = MenuButton::from_button(button).ui(ui, |ui| {
                            let mut rows = Rows {
                                commands: &mut commands,
                                #[cfg(test)]
                                hits: &mut *hits,
                            };
                            rows.menu(ui, menu, recents, paused);
                        });
                        let popup = Popup::default_response_id(&response);
                        if Popup::is_id_open(ui.ctx(), popup) && !response.hovered() {
                            ui.painter().set(
                                lit,
                                Shape::rect_filled(
                                    response.rect,
                                    CornerRadius::same(5),
                                    theme::TERM_ON,
                                ),
                            );
                        }
                        #[cfg(test)]
                        hits.push((BarHit::Title(menu.title), response.rect));
                        popups.push(popup);
                    }
                });
        });

        let open = popups
            .into_iter()
            .any(|popup| Popup::is_id_open(ui.ctx(), popup));
        let closed = self.open && !open;
        self.open = open;
        if !commands.is_empty() || closed {
            ui.ctx().request_repaint();
        }
        BarOut {
            rest: rest.intersect(area),
            commands,
            closed,
        }
    }

    /// Whether the terminal must not get this pass's events: a menu was open
    /// when it began, or is open now.
    pub(crate) fn blocks_input(&self) -> bool {
        self.open_at_start || self.open
    }
}

/// One open menu's rows.
struct Rows<'a> {
    commands: &'a mut Vec<Command>,
    #[cfg(test)]
    hits: &'a mut Vec<(BarHit, Rect)>,
}

/// One row's look and state.
struct Row<'a> {
    label: &'a str,
    shortcut: Option<String>,
    enabled: bool,
    checked: bool,
    submenu: bool,
}

impl Rows<'_> {
    fn menu(&mut self, ui: &mut Ui, menu: &Menu, recents: &[RecentProject], paused: bool) {
        let rows: Vec<(&str, Option<String>)> = menu
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Item { label, chord, .. } | Entry::Pause { label, chord } => {
                    Some((*label, chord.map(in_window_shortcut)))
                }
                Entry::Recent { label } => Some((*label, None)),
                Entry::Separator | Entry::System(_) => None,
            })
            .collect();
        let width = row_width(ui, &rows);
        for entry in &menu.entries {
            match entry {
                Entry::Item {
                    label,
                    command,
                    chord,
                } => {
                    let row = Row {
                        label,
                        shortcut: chord.map(in_window_shortcut),
                        enabled: true,
                        checked: false,
                        submenu: false,
                    };
                    if self.row(ui, width, row).clicked() {
                        self.commands.push(command.clone());
                        ui.close();
                    }
                }
                Entry::Pause { label, chord } => {
                    let row = Row {
                        label,
                        shortcut: chord.map(in_window_shortcut),
                        enabled: true,
                        checked: paused,
                        submenu: false,
                    };
                    if self.row(ui, width, row).clicked() {
                        self.commands.push(Command::TogglePause);
                        ui.close();
                    }
                }
                Entry::Recent { label } => {
                    let row = Row {
                        label,
                        shortcut: None,
                        enabled: true,
                        checked: false,
                        submenu: true,
                    };
                    let response = self.row(ui, width, row);
                    SubMenu::new().show(ui, &response, |ui| self.recent(ui, recents));
                }
                Entry::Separator => self.separator(ui, width),
                Entry::System(_) => {}
            }
        }
    }

    fn recent(&mut self, ui: &mut Ui, recents: &[RecentProject]) {
        let entries = recent_entries(recents);
        let labels: Vec<(&str, Option<String>)> = entries
            .iter()
            .filter_map(|entry| match entry {
                RecentEntry::Empty => Some((NO_RECENTS, None)),
                RecentEntry::Project { name, .. } => Some((*name, None)),
                RecentEntry::Clear => Some((CLEAR_RECENT, None)),
                RecentEntry::Separator => None,
            })
            .collect();
        let width = row_width(ui, &labels).min(420.0);
        for entry in &entries {
            let label = match entry {
                RecentEntry::Empty => NO_RECENTS,
                RecentEntry::Project { name, .. } => name,
                RecentEntry::Clear => CLEAR_RECENT,
                RecentEntry::Separator => {
                    self.separator(ui, width);
                    continue;
                }
            };
            let command = entry.command();
            let row = Row {
                label,
                shortcut: None,
                enabled: command.is_some(),
                checked: false,
                submenu: false,
            };
            if self.row(ui, width, row).clicked()
                && let Some(command) = command
            {
                self.commands.push(command);
                ui.close();
            }
        }
    }

    fn row(&mut self, ui: &mut Ui, width: f32, row: Row<'_>) -> Response {
        let sense = if row.enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(vec2(width, ROW_HEIGHT), sense);
        let lit = row.enabled && (response.hovered() || response.has_focus());
        let painter = ui.painter_at(rect.expand(1.0));
        if lit {
            painter.rect_filled(rect, 5.0, theme::TERM_ON);
        }
        let text = if !row.enabled {
            theme::FAINT
        } else if lit {
            theme::TEXT
        } else {
            theme::TEXT_ROW
        };
        if row.checked {
            icons::check(
                &painter,
                pos2(rect.min.x + CHECK_GUTTER / 2.0 + 1.0, rect.center().y),
                theme::CYAN,
            );
        }
        let right = rect.max.x - ROW_PAD;
        let mut label_room = right - (rect.min.x + CHECK_GUTTER);
        if let Some(shortcut) = &row.shortcut {
            let galley = painter.layout_no_wrap(
                shortcut.clone(),
                FontId::proportional(SHORTCUT_SIZE),
                theme::FAINT,
            );
            label_room -= galley.size().x + SHORTCUT_GAP / 2.0;
            let color = if lit { theme::TEXT_MUTED } else { theme::FAINT };
            painter.galley(
                pos2(
                    right - galley.size().x,
                    rect.center().y - galley.size().y / 2.0,
                ),
                galley,
                color,
            );
        }
        if row.submenu {
            icons::twisty(&painter, pos2(right - 3.0, rect.center().y), false, text);
            label_room -= 12.0;
        }
        let galley = crate::tree::fit(
            &painter,
            row.label,
            FontId::proportional(ITEM_SIZE),
            text,
            label_room,
        );
        painter.galley(
            pos2(
                rect.min.x + CHECK_GUTTER,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            text,
        );
        let (label, enabled, checked) = (row.label.to_owned(), row.enabled, row.checked);
        response.widget_info(|| {
            if checked {
                egui::WidgetInfo::selected(
                    egui::accesskit::Role::MenuItemCheckBox,
                    enabled,
                    true,
                    label.clone(),
                )
            } else {
                egui::WidgetInfo::labeled(egui::accesskit::Role::MenuItem, enabled, label.clone())
            }
        });
        #[cfg(test)]
        self.hits.push((
            BarHit::Row {
                label: row.label.to_owned(),
                shortcut: row.shortcut,
                enabled: row.enabled,
                checked: row.checked,
            },
            rect,
        ));
        response
    }

    fn separator(&mut self, ui: &mut Ui, width: f32) {
        let (rect, _) = ui.allocate_exact_size(vec2(width, SEPARATOR_HEIGHT), Sense::hover());
        ui.painter().hline(
            (rect.min.x + 6.0)..=(rect.max.x - 6.0),
            rect.center().y,
            Stroke::new(1.0, theme::POPOVER_BORDER),
        );
        #[cfg(test)]
        self.hits.push((BarHit::Separator, rect));
    }
}

/// A menu wide enough for its longest label and its longest shortcut.
fn row_width(ui: &Ui, rows: &[(&str, Option<String>)]) -> f32 {
    let measure = |text: &str, size: f32| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), FontId::proportional(size), theme::TEXT)
            .size()
            .x
    };
    let label = rows
        .iter()
        .map(|(label, _)| measure(label, ITEM_SIZE))
        .fold(0.0_f32, f32::max);
    let shortcut = rows
        .iter()
        .filter_map(|(_, shortcut)| shortcut.as_deref())
        .map(|shortcut| measure(shortcut, SHORTCUT_SIZE))
        .fold(0.0_f32, f32::max);
    let gap = if shortcut > 0.0 { SHORTCUT_GAP } else { 12.0 };
    (CHECK_GUTTER + label + gap + shortcut + ROW_PAD).max(MIN_WIDTH)
}
