//! The host switcher: `localhost` and each saved host, above the projects in
//! the sidebar, and the sheet that adds or renames a host.
//!
//! Drawing only reads the views and returns `Command`s. A click on a host
//! changes which terminal list the window shows and nothing else; it never
//! types into a terminal.

use connecto_core::{HostLink, HostRow, LOCALHOST, PendingHost, RemoteView};
use egui::{Align2, Color32, FontId, Id, Rect, RichText, Sense, Stroke, TextEdit, Ui, pos2, vec2};

use crate::commands::Command;
use crate::forms::{self, Out, heading, muted, problem, warning};
use crate::icons;
use crate::theme;

const ROW_HEIGHT: f32 = 24.0;
const DOT_RADIUS: f32 = 4.0;

/// The words beside a host.
pub(crate) fn link_word(link: &HostLink) -> String {
    match link {
        HostLink::Disconnected => "disconnected".to_owned(),
        HostLink::Connecting => "connecting".to_owned(),
        HostLink::Direct => "direct".to_owned(),
        HostLink::Relayed => "relayed".to_owned(),
        HostLink::Connected => "connected".to_owned(),
        HostLink::Reconnecting { retry_in_secs } => format!("retry in {retry_in_secs}s"),
        HostLink::Refused(_) => "refused".to_owned(),
    }
}

/// The dot's colour, or `None` for the hollow one.
fn link_color(link: &HostLink) -> Option<Color32> {
    match link {
        HostLink::Direct => Some(theme::GREEN),
        HostLink::Relayed | HostLink::Connected => Some(theme::CYAN),
        HostLink::Connecting | HostLink::Reconnecting { .. } => Some(theme::AMBER),
        HostLink::Refused(_) => Some(theme::ERROR),
        HostLink::Disconnected => None,
    }
}

/// The most host rows shown at once.
const MAX_ROWS: usize = 6;
const HEAD_HEIGHT: f32 = 22.0;

/// How tall the section is for `hosts` saved hosts (and `localhost`).
pub(crate) fn section_height(hosts: usize) -> f32 {
    HEAD_HEIGHT + ROW_HEIGHT * (1 + hosts.min(MAX_ROWS)) as f32 + 8.0
}

/// Where each row of the section was drawn, for tests.
#[cfg(test)]
pub(crate) type RowRects = Vec<(String, Rect)>;

/// Draw `HOSTS`, `localhost` and the saved hosts. `selected` is the remote host
/// shown, or `None` for this machine.
pub(crate) fn section(
    ui: &mut Ui,
    rows: &[HostRow],
    selected: Option<&str>,
    commands: &mut Vec<Command>,
    #[cfg(test)] rects: &mut RowRects,
) {
    // The heading, with the add button.
    let (head, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEAD_HEIGHT), Sense::hover());
    ui.painter().text(
        pos2(head.min.x + 8.0, head.center().y),
        Align2::LEFT_CENTER,
        "HOSTS",
        FontId::monospace(10.5),
        theme::FAINT,
    );
    let plus = Rect::from_center_size(pos2(head.max.x - 14.0, head.center().y), vec2(20.0, 20.0));
    let add = ui.interact(plus, Id::unique("host-add"), Sense::click());
    if add.hovered() {
        ui.painter().rect_filled(plus, 6.0, theme::RAISED_BG);
    }
    icons::plus(
        ui.painter(),
        plus.center(),
        4.5,
        if add.hovered() {
            theme::TEXT
        } else {
            theme::TEXT_MUTED
        },
    );
    add.widget_info(|| egui::WidgetInfo::labeled(egui::accesskit::Role::Button, true, "Add host"));
    #[cfg(feature = "ui-test-driver")]
    crate::driver::note(ui.ctx(), "host-add", plus);
    #[cfg(test)]
    rects.push(("add".to_owned(), plus));
    if add.on_hover_text("Add host").clicked() {
        commands.push(Command::AddHost);
    }

    one_row(
        ui,
        LOCALHOST,
        "this machine",
        Some(theme::GREEN),
        selected.is_none(),
        None,
        commands,
        #[cfg(test)]
        rects,
    );
    for host in rows.iter().take(MAX_ROWS) {
        let hint = match &host.link {
            HostLink::Refused(why) => why.clone(),
            other => format!("{}  ·  {}", link_word(other), host.fingerprint),
        };
        one_row(
            ui,
            &host.name,
            &link_word(&host.link),
            link_color(&host.link),
            selected.is_some_and(|name| name.eq_ignore_ascii_case(&host.name)),
            Some((host, hint)),
            commands,
            #[cfg(test)]
            rects,
        );
    }
    ui.add_space(6.0);
}

#[allow(clippy::too_many_arguments)]
fn one_row(
    ui: &mut Ui,
    name: &str,
    word: &str,
    color: Option<Color32>,
    selected: bool,
    host: Option<(&HostRow, String)>,
    commands: &mut Vec<Command>,
    #[cfg(test)] rects: &mut RowRects,
) {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    #[cfg(test)]
    rects.push((name.to_owned(), rect));
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 6.0, theme::ROW_ACTIVE);
    } else if response.hovered() {
        painter.rect_filled(rect, 6.0, theme::ROW_HOVER);
    }
    let center = pos2(rect.min.x + 16.0, rect.center().y);
    match color {
        Some(color) => {
            painter.circle_filled(center, DOT_RADIUS, color);
        }
        None => {
            painter.circle_stroke(center, DOT_RADIUS - 0.5, Stroke::new(1.2, theme::FAINT));
        }
    }
    let text_color = if selected {
        Color32::WHITE
    } else {
        theme::TEXT_ROW
    };
    painter.text(
        pos2(rect.min.x + 28.0, rect.center().y),
        Align2::LEFT_CENTER,
        name,
        FontId::proportional(13.0),
        text_color,
    );
    painter.text(
        pos2(rect.max.x - 8.0, rect.center().y),
        Align2::RIGHT_CENTER,
        word,
        FontId::proportional(10.5),
        theme::FAINT,
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::accesskit::Role::Button,
            true,
            selected,
            format!("{name}, {word}"),
        )
    });
    let response = match &host {
        Some((_, hint)) => response.on_hover_text(hint.as_str()),
        None => response,
    };
    if response.clicked() {
        commands.push(Command::SelectHost(host.as_ref().map(|_| name.to_owned())));
    }
    if let Some((row, _)) = host {
        response.context_menu(|ui| {
            let connected = !matches!(row.link, HostLink::Disconnected);
            let items: [(&str, Command); 4] = [
                (
                    if connected { "Reconnect" } else { "Connect" },
                    Command::ConnectHost(row.name.clone()),
                ),
                ("Disconnect", Command::DisconnectHost(row.name.clone())),
                ("Rename host…", Command::RenameHostAt(row.name.clone())),
                ("Forget host…", Command::AskForgetHost(row.name.clone())),
            ];
            for (label, command) in items {
                if ui.button(label).clicked() {
                    commands.push(command);
                    ui.close();
                }
            }
        });
    }
}

// ---------- the sheet ----------

/// What the sheet is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    Add,
    Rename { from: String },
}

pub(crate) struct HostSheet {
    pub(crate) mode: Mode,
    pub(crate) name: String,
    pub(crate) address: String,
    pub(crate) error: Option<String>,
}

impl HostSheet {
    /// Add a machine by its address and SSH key: the advanced way. The usual
    /// way is the pairing dialog.
    pub(crate) fn add() -> Self {
        Self {
            mode: Mode::Add,
            name: String::new(),
            address: String::new(),
            error: None,
        }
    }

    pub(crate) fn rename(from: &str) -> Self {
        Self {
            mode: Mode::Rename {
                from: from.to_owned(),
            },
            name: from.to_owned(),
            address: String::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostAction {
    Close,
    /// Name and address given: show the fingerprint to confirm.
    Add {
        name: String,
        address: String,
    },
    /// The fingerprint matches: save and connect.
    Confirm,
    /// Not this machine.
    CancelPending,
    Rename {
        from: String,
        to: String,
    },
}

pub(crate) fn sheet(
    ctx: &egui::Context,
    area: Rect,
    state: &mut HostSheet,
    view: &RemoteView,
    out: &mut Out<HostAction>,
) {
    let title = match state.mode {
        Mode::Add => "Add host",
        Mode::Rename { .. } => "Rename host",
    };
    let closed = crate::sheet::show(ctx, area, "host-panel", title, |ui, content| {
        body(ui, content, state, view, out);
    });
    if closed {
        out.actions.push(HostAction::Close);
    }
}

fn body(
    ui: &mut Ui,
    rect: Rect,
    state: &mut HostSheet,
    view: &RemoteView,
    out: &mut Out<HostAction>,
) {
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        if view.pending.is_some() {
            out.actions.push(HostAction::CancelPending);
        }
        out.actions.push(HostAction::Close);
    }
    forms::column(ui, rect, |ui| match state.mode.clone() {
        Mode::Rename { from } => rename_body(ui, state, &from, out),
        Mode::Add => match &view.pending {
            Some(pending) => confirm_body(ui, pending, out),
            None => add_body(ui, state, out),
        },
    });
    if let Some(error) = state.error.as_deref().or(view.last_error.as_deref()) {
        forms::column(ui, rect, |ui| out.say(ui, problem(error)));
    }
}

fn field(ui: &mut Ui, id: &'static str, text: &mut String, hint: &str) -> egui::Response {
    ui.add(
        TextEdit::singleline(text)
            .id(Id::unique(id))
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    )
}

fn add_body(ui: &mut Ui, state: &mut HostSheet, out: &mut Out<HostAction>) {
    out.say(ui, heading("ADD BY ADDRESS"));
    out.say(
        ui,
        muted("The advanced way: the other machine must also allow one of this machine's SSH keys. Most people pair a device instead."),
    );
    out.say(ui, heading("NAME"));
    out.say(
        ui,
        muted("Something you will recognise. It must be different from every other host, and it cannot be \"localhost\"."),
    );
    let name = field(ui, "host-name", &mut state.name, "build box");
    out.mark("host-name", name.rect);
    ui.add_space(8.0);
    out.say(ui, heading("ADDRESS"));
    out.say(
        ui,
        muted("The address the other machine shows in Settings, under Remote access. It starts with connecto:"),
    );
    let address = field(ui, "host-address", &mut state.address, "connecto:…");
    out.mark("host-address", address.rect);
    ui.add_space(10.0);
    let ready = !state.name.trim().is_empty() && !state.address.trim().is_empty();
    let enter = address.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if out.button_hint(
        ui,
        "host-continue",
        "Continue",
        ready,
        "Give it a name and an address.",
    ) || (enter && ready)
    {
        state.error = None;
        out.actions.push(HostAction::Add {
            name: state.name.trim().to_owned(),
            address: state.address.trim().to_owned(),
        });
    }
}

fn confirm_body(ui: &mut Ui, pending: &PendingHost, out: &mut Out<HostAction>) {
    out.say(ui, heading("CONFIRM THIS MACHINE"));
    out.say(
        ui,
        RichText::new(format!("Name: {}", pending.name)).color(theme::TEXT),
    );
    ui.add_space(4.0);
    out.say(ui, muted("Its fingerprint:"));
    out.say(
        ui,
        RichText::new(&pending.fingerprint)
            .monospace()
            .size(16.0)
            .color(theme::CYAN),
    );
    ui.add_space(4.0);
    out.say(
        ui,
        warning("Compare it with the fingerprint shown on that machine, in Settings under Remote access. Connect only if they are the same: from now on this name means that machine and no other."),
    );
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if out.button(ui, "host-confirm", "They match: connect", true) {
            out.actions.push(HostAction::Confirm);
            out.actions.push(HostAction::Close);
        }
        if out.button(ui, "host-cancel", "Not the same", true) {
            out.actions.push(HostAction::CancelPending);
        }
    });
}

fn rename_body(ui: &mut Ui, state: &mut HostSheet, from: &str, out: &mut Out<HostAction>) {
    out.say(ui, heading("NEW NAME"));
    let name = field(ui, "host-rename", &mut state.name, from);
    out.mark("host-rename", name.rect);
    ui.add_space(10.0);
    let ready = !state.name.trim().is_empty();
    let enter = name.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if out.button(ui, "host-save", "Save", ready) || (enter && ready) {
        out.actions.push(HostAction::Rename {
            from: from.to_owned(),
            to: state.name.trim().to_owned(),
        });
    }
}
