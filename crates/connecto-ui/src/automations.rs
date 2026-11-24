//! The Automations sheet (Terminal menu): the Jev API key, the automations
//! and their settings, the confirmations waiting for the user, and the history
//! of the engine's calls. A banner in the main window raises a confirmation
//! when the sheet is closed.

use connecto_core::{
    AutomationItem, AutomationOutcome, AutomationsControl, AutomationsView, EngineState,
    HistoryRecord, KeyStatus, LastCheck, PendingConfirmation, ToolKind,
};
use egui::{Align2, Color32, Id, Key, Margin, Rect, RichText, Sense, Stroke, TextEdit, Ui, vec2};

use crate::forms::{heading, muted, separator};
use crate::history::when;
use crate::theme::{self, FieldStyle as _};
use crate::utility::ago;

const THRESHOLD_STEP: i32 = 5;
const THRESHOLD_MIN: i32 = 55;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Automations,
    History,
    /// The Jev API key, Connecto Home and the logins: set once, then left alone.
    Setup,
}

/// Which agent's automations the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Only(ToolKind),
}

/// The tabs of the filter strip: All, then every kind that can have automations
/// (Claude and Codex: there are none for Pi's screens, and a plain shell
/// cannot have any).
fn filter_tabs() -> Vec<Filter> {
    std::iter::once(Filter::All)
        .chain(
            [ToolKind::Claude, ToolKind::Codex]
                .into_iter()
                .map(Filter::Only),
        )
        .collect()
}

thread_local! {
    /// The tab last chosen, kept for the next time the sheet opens (0 is All).
    /// The window runs on one thread; a test gets a fresh one.
    static REMEMBERED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn remembered() -> Filter {
    filter_tabs()
        .get(REMEMBERED.with(std::cell::Cell::get))
        .copied()
        .unwrap_or(Filter::All)
}

fn remember(filter: Filter) {
    if let Some(at) = filter_tabs().iter().position(|f| *f == filter) {
        REMEMBERED.with(|cell| cell.set(at));
    }
}

/// Marks of a row that opens. egui's built-in fonts have no small triangles,
/// which drew as empty boxes.
const CLOSED: &str = "+";
const OPEN: &str = "−";

pub(crate) struct AutomationsSheet {
    tab: Tab,
    /// Whether the New automation form is shown beside a list. `None` until the
    /// user says, which reads as hidden. With no automation it always shows.
    new_open: Option<bool>,
    /// The automations whose row is open, by slug.
    expanded: std::collections::HashSet<String>,
    /// The groups that are opened to show their automations, by id.
    open_groups: std::collections::HashSet<String>,
    /// The slugs seen so far. A slug that appears after the first look is new,
    /// and its row opens.
    known: Option<std::collections::HashSet<String>>,
    pub(crate) key_input: String,
    /// Why the key could not be stored.
    pub(crate) key_error: Option<String>,
    /// Which agent's automations are listed, and the New form's agent.
    filter: Filter,
    pub(crate) name: String,
    /// "When the terminal shows…"
    pub(crate) screen: String,
    /// "Do this…"
    pub(crate) action: String,
    agent: ToolKind,
    /// Why the automation could not be created.
    pub(crate) create_error: Option<String>,
    /// Why a session could not be opened.
    pub(crate) error: Option<String>,
    /// Why the folder could not be set, or a login terminal not opened.
    pub(crate) root_error: Option<String>,
    /// The automation whose Delete awaits its confirmation.
    confirm_delete: Option<String>,
    /// The automation whose Clear audit awaits its confirmation.
    confirm_clear: Option<String>,
    /// Why an automation's audit could not be cleared.
    pub(crate) audit_error: Option<(String, String)>,
    /// The audit panels that are open, with the calls they list.
    audit: std::collections::HashMap<String, Audit>,
    /// Calls whose request and response are expanded, by (slug, entry id).
    open_calls: std::collections::HashSet<(String, u64)>,
    /// The title being typed for a new group; `None` while the entry is closed.
    pub(crate) new_group: Option<String>,
    /// Why a group could not be made, renamed, deleted or filled.
    pub(crate) group_error: Option<String>,
    /// The custom group being renamed, with the title typed so far.
    pub(crate) renaming_group: Option<(String, String)>,
    /// The custom group whose Delete awaits its confirmation.
    confirm_delete_group: Option<String>,
    /// The next group title field drawn takes the keyboard.
    focus_group_field: bool,
}

/// What a drag of an automation carries: who, and the custom group it leaves
/// (`None` for a loose one).
#[derive(Debug, Clone)]
struct Dragged {
    slug: String,
    from: Option<String>,
}

/// One automation's calls, read when its Audit panel opens and again when the
/// view changes.
struct Audit {
    revision: u64,
    calls: Vec<Call>,
}

struct Call {
    record: HistoryRecord,
    /// Pretty-printed when the text parses as JSON.
    request: Option<String>,
    response: Option<String>,
}

impl Call {
    fn new(record: HistoryRecord) -> Self {
        let request = record.request.as_deref().map(pretty);
        let response = record.response.as_deref().map(pretty);
        Self {
            record,
            request,
            response,
        }
    }
}

fn pretty(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| text.to_owned())
}

impl Default for AutomationsSheet {
    fn default() -> Self {
        Self {
            tab: Tab::default(),
            filter: remembered(),
            new_open: None,
            expanded: std::collections::HashSet::new(),
            open_groups: std::collections::HashSet::new(),
            known: None,
            key_input: String::new(),
            key_error: None,
            name: String::new(),
            screen: String::new(),
            action: String::new(),
            agent: match remembered() {
                Filter::Only(kind) => kind,
                Filter::All => ToolKind::Claude,
            },
            create_error: None,
            error: None,
            root_error: None,
            confirm_delete: None,
            confirm_clear: None,
            audit_error: None,
            audit: std::collections::HashMap::new(),
            open_calls: std::collections::HashSet::new(),
            new_group: None,
            group_error: None,
            renaming_group: None,
            confirm_delete_group: None,
            focus_group_field: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AutomationsAction {
    Close,
    /// Open the sheet (from the banner).
    OpenSheet,
    SaveKey(String),
    ClearKey,
    /// Open Settings, where Connecto Home (the automations folder) is chosen.
    OpenSettings,
    /// Open a terminal at the folder to log the tool in.
    OpenLogin(ToolKind),
    Create(String, ToolKind),
    /// Described in text: when the terminal shows `screen`, do `action`.
    CreateDescribed {
        name: String,
        agent: ToolKind,
        screen: String,
        action: String,
    },
    OpenSession(String),
    SetEnabled(String, bool),
    /// Switch several automations (a group's members) at once.
    SetManyEnabled(Vec<String>, bool),
    Delete(String),
    SetThreshold(String, f64),
    Approve(u64),
    Dismiss(u64),
    ClearHistory,
    /// Remove one automation's history.
    ClearAudit(String),
    /// Make a group of the user's own, with this title.
    CreateGroup(String),
    /// Retitle a custom group, by id.
    RenameGroup {
        id: String,
        title: String,
    },
    /// Delete a custom group, by id; its automations are kept.
    DeleteGroup(String),
    /// Put an automation in a custom group, or with `None` out of its group.
    SetGroup(String, Option<String>),
}

pub(crate) struct AutomationsInput<'a> {
    /// `None` when the engine did not start.
    pub(crate) view: Option<&'a AutomationsView>,
    /// For the audit lists, which are read from the engine when opened.
    pub(crate) control: Option<&'a dyn AutomationsControl>,
    pub(crate) now_ms: u64,
    pub(crate) utc_offset_s: i32,
}

#[derive(Default)]
pub(crate) struct AutomationsOut {
    pub(crate) actions: Vec<AutomationsAction>,
    #[cfg(test)]
    pub(crate) hits: AutomationsHits,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct AutomationsHits {
    /// Each control drawn, by name.
    pub(crate) rects: std::collections::HashMap<String, Rect>,
    /// Every line of text drawn.
    pub(crate) texts: Vec<String>,
    /// The label of each button drawn, by name.
    pub(crate) labels: std::collections::HashMap<String, String>,
    /// The full text shown on hover, for lines that are cut short.
    pub(crate) hovers: Vec<String>,
}

impl AutomationsOut {
    fn button(&mut self, ui: &mut Ui, key: &str, text: &str, enabled: bool) -> bool {
        self.button_with(ui, key, text, enabled, |button| button)
    }

    /// A button restyled by `style`.
    fn button_with(
        &mut self,
        ui: &mut Ui,
        key: &str,
        text: &str,
        enabled: bool,
        style: impl FnOnce(egui::Button<'_>) -> egui::Button<'_>,
    ) -> bool {
        let response = ui.add_enabled(enabled, style(egui::Button::new(text)));
        #[cfg(test)]
        {
            self.hits.rects.insert(key.to_owned(), response.rect);
            self.hits.labels.insert(key.to_owned(), text.to_owned());
        }
        let _ = key;
        response.clicked()
    }

    fn say(&mut self, ui: &mut Ui, text: RichText) {
        #[cfg(test)]
        self.hits.texts.push(text.text().to_owned());
        ui.label(text);
    }

    /// A line that shows `full` on hover.
    fn say_hover(&mut self, ui: &mut Ui, text: RichText, full: &str) {
        #[cfg(test)]
        {
            self.hits.texts.push(text.text().to_owned());
            self.hits.hovers.push(full.to_owned());
        }
        ui.label(text).on_hover_text(full);
    }

    #[cfg(test)]
    fn mark(&mut self, key: &str, rect: Rect) {
        self.hits.rects.insert(key.to_owned(), rect);
    }

    #[cfg(not(test))]
    fn mark(&mut self, _key: &str, _rect: Rect) {}
}

/// The API key field's id, fixed so its edit state can be wiped.
pub(crate) fn key_field_id() -> Id {
    static ID: std::sync::OnceLock<Id> = std::sync::OnceLock::new();
    *ID.get_or_init(|| Id::unique("automations-key-field"))
}

/// Fixed ids for the New automation fields, so their focus and cursor survive
/// the layout shifting around them.
fn form_ids() -> [Id; 3] {
    static IDS: std::sync::OnceLock<[Id; 3]> = std::sync::OnceLock::new();
    *IDS.get_or_init(|| {
        [
            Id::unique("automation-name"),
            Id::unique("automation-screen"),
            Id::unique("automation-action"),
        ]
    })
}

impl AutomationsSheet {
    /// Empty the New automation form after a successful create.
    pub(crate) fn clear_form(&mut self) {
        self.name.clear();
        self.screen.clear();
        self.action.clear();
        self.create_error = None;
        self.new_open = None;
    }
}

/// Forget what egui keeps of the key field besides its text: the undo history
/// holds every earlier text, the secret included.
pub(crate) fn wipe_key_state(ctx: &egui::Context) {
    let id = key_field_id();
    if let Some(mut state) = TextEdit::load_state(ctx, id) {
        state.clear_undoer();
        TextEdit::store_state(ctx, id, state);
    }
}

fn percent(threshold: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    {
        (threshold * 100.0).round() as i32
    }
}

fn faint(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .size(10.5)
        .monospace()
        .color(theme::FAINT)
}

const TRIGGER_SHOWN: usize = 40;

fn clip(phrase: &str) -> String {
    if phrase.chars().count() > TRIGGER_SHOWN {
        let head: String = phrase.chars().take(TRIGGER_SHOWN - 1).collect();
        format!("{head}…")
    } else {
        phrase.to_owned()
    }
}

/// `Trigger: "one" + "two"`, each phrase cut short; the full text is the hover.
fn trigger_line(trigger: &[String]) -> (String, String) {
    if trigger.is_empty() {
        return ("Trigger: none".to_owned(), String::new());
    }
    let join = |f: &dyn Fn(&str) -> String| {
        trigger
            .iter()
            .map(|phrase| format!("\"{}\"", f(phrase)))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    (
        format!("Trigger: {}", join(&clip)),
        format!("Trigger: {}", join(&str::to_owned)),
    )
}

fn keys_summary(keys: &[String]) -> String {
    if keys.is_empty() {
        "none".to_owned()
    } else {
        keys.join(" ")
    }
}

fn terminal_label(name: Option<&String>, id: &str) -> String {
    name.map_or_else(|| id.to_owned(), Clone::clone)
}

pub(crate) fn sheet(
    ctx: &egui::Context,
    area: Rect,
    state: &mut AutomationsSheet,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let closed = crate::sheet::show(
        ctx,
        area,
        "automations-panel",
        "Automations",
        |ui, content| {
            body(ui, content, state, input, out);
        },
    );
    if closed {
        out.actions.push(AutomationsAction::Close);
    }
}

fn body(
    ui: &mut Ui,
    rect: Rect,
    state: &mut AutomationsSheet,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let escape = ui.input(|i| i.key_pressed(Key::Escape));
    // Esc first cancels a confirmation, and only then closes the sheet.
    let cancelled = escape
        && (state.confirm_delete.take().is_some()
            | state.confirm_clear.take().is_some()
            | state.confirm_delete_group.take().is_some()
            | state.renaming_group.take().is_some()
            | state.new_group.take().is_some());
    if escape && !cancelled {
        out.actions.push(AutomationsAction::Close);
    }
    egui::Frame::NONE
        .inner_margin(Margin::symmetric(crate::sheet::PAD as i8, 10))
        .show(ui, |ui| {
            ui.set_width(rect.width() - 2.0 * crate::sheet::PAD);
            let Some(view) = input.view else {
                ui.add_space(8.0);
                out.say(
                    ui,
                    muted("Automations are not available in this run of Connecto."),
                );
                return;
            };
            let pending = view.pending.len();
            let history = view.history.len();
            ui.horizontal(|ui| {
                let label = if pending > 0 {
                    format!("Automations ({pending} waiting)")
                } else {
                    "Automations".to_owned()
                };
                let setup = if setup_problems(view).is_empty() {
                    "Setup".to_owned()
                } else {
                    "Setup (incomplete)".to_owned()
                };
                for (tab, name, text) in [
                    (Tab::Automations, "automations", label),
                    (Tab::History, "history", format!("History ({history})")),
                    (Tab::Setup, "setup", setup),
                ] {
                    let response = ui.selectable_label(state.tab == tab, &text);
                    out.mark(&format!("tab:{text}"), response.rect);
                    out.mark(&format!("tab:{name}"), response.rect);
                    if response.clicked() {
                        state.tab = tab;
                    }
                }
                // Top right, so a new automation never takes a scroll.
                if state.tab == Tab::Automations && !view.automations.is_empty() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let open = new_form_open(state, view);
                        let text = if open { "Cancel" } else { "New automation" };
                        if out.button(ui, "new", text, true) {
                            state.new_open = Some(!open);
                        }
                    });
                }
            });
            // Outside the scroll area, so a refused move or rename is seen
            // wherever the list is scrolled to.
            if state.tab == Tab::Automations
                && let Some(error) = &state.group_error
            {
                out.say(ui, RichText::new(error).size(11.5).color(theme::ERROR));
            }
            ui.add_space(4.0);
            // One scroll position per tab.
            egui::ScrollArea::vertical()
                .id_salt(("automations-scroll", state.tab as u8))
                .auto_shrink([false, false])
                .show(ui, |ui| match state.tab {
                    Tab::Automations => automations_tab(ui, state, view, input, out),
                    Tab::History => history_tab(ui, view, input, out),
                    Tab::Setup => setup_tab(ui, state, view, out),
                });
        });
}

/// Whether the New automation form is shown. With no automation it always is.
fn new_form_open(state: &AutomationsSheet, view: &AutomationsView) -> bool {
    view.automations.is_empty() || state.new_open.unwrap_or(false)
}

/// Opens the row of each automation that appears after the first look.
fn open_new_rows(state: &mut AutomationsSheet, view: &AutomationsView) {
    let slugs = view.automations.iter().map(|item| item.slug.clone());
    match &mut state.known {
        None => state.known = Some(slugs.collect()),
        Some(known) => {
            for slug in slugs {
                if known.insert(slug.clone()) {
                    state.expanded.insert(slug);
                }
            }
        }
    }
}

fn automations_tab(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    open_new_rows(state, view);
    filter_strip(ui, state, view, out);
    if new_form_open(state, view) {
        egui::Frame::NONE
            .fill(theme::PANEL_BG)
            .stroke(Stroke::new(1.0, theme::TEAL))
            .corner_radius(6.0)
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                new_section(ui, state, view.root.is_some(), out);
            });
        ui.add_space(6.0);
    }
    status_line(ui, view, out);
    setup_callout(ui, state, view, out);
    if !view.pending.is_empty() {
        ui.add_space(6.0);
        out.say(ui, heading("WAITING FOR YOU"));
        for pending in &view.pending {
            pending_card(ui, pending, out);
        }
    }
    if let Some(error) = &state.error {
        out.say(ui, RichText::new(error).size(11.5).color(theme::ERROR));
    }
    for (kind, name) in [(ToolKind::Claude, "Claude"), (ToolKind::Codex, "Codex")] {
        if let Filter::Only(only) = state.filter
            && only != kind
        {
            continue;
        }
        ui.add_space(8.0);
        // Under a tab of its own, the heading would only repeat the tab.
        if state.filter == Filter::All {
            out.say(ui, heading(name));
            ui.add_space(2.0);
        }
        let mut any = false;
        for group in view
            .groups
            .iter()
            .filter(|group| !group.custom && group.agent == kind)
        {
            any = true;
            ui.push_id(&group.id, |ui| {
                group_card(ui, state, group, view, input, out);
            });
        }
        // While a grouped automation is dragged, the loose list is where it
        // drops to leave its group.
        let leaving = egui::DragAndDrop::payload::<Dragged>(ui.ctx())
            .is_some_and(|dragged| dragged.from.is_some());
        let mut loose = |ui: &mut Ui| {
            let mut shown = false;
            for item in view
                .automations
                .iter()
                .filter(|item| item.agent == kind && group_of(view, &item.slug).is_none())
            {
                shown = true;
                // Ids by slug, so a press on a row that goes never lands on its neighbour.
                ui.push_id(&item.slug, |ui| {
                    automation_card(ui, state, item, view, input, out)
                });
            }
            shown
        };
        if leaving {
            let (zone, dropped) =
                ui.dnd_drop_zone::<Dragged, bool>(egui::Frame::NONE.corner_radius(6.0), |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(ROW_HEIGHT);
                    let shown = loose(ui);
                    if !shown {
                        ui.label(muted("Drop here to take it out of its group"));
                    }
                    shown
                });
            any = true;
            out.mark(&format!("loose:{name}"), zone.response.rect);
            if let Some(dragged) = dropped {
                out.actions
                    .push(AutomationsAction::SetGroup(dragged.slug.clone(), None));
            }
        } else {
            any |= loose(ui);
        }
        if !any {
            out.say(ui, muted(format!("No {name} automations yet")));
            ui.add_space(6.0);
        }
    }
    // The shipped groups are above; the user's own come last.
    groups_section(ui, state, view, input, out);
}

/// The group an automation belongs to, if any.
fn group_of<'a>(
    view: &'a AutomationsView,
    slug: &str,
) -> Option<&'a connecto_core::AutomationGroup> {
    view.groups
        .iter()
        .find(|group| group.members.iter().any(|member| member == slug))
}

/// The user's own groups: the New group entry, then a card for each.
fn groups_section(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let customs: Vec<_> = view.groups.iter().filter(|group| group.custom).collect();
    if customs.is_empty() && state.new_group.is_none() {
        return;
    }
    ui.add_space(8.0);
    out.say(ui, heading("GROUPS"));
    ui.add_space(2.0);
    if state.new_group.is_some() {
        new_group_row(ui, state, out);
    }
    for group in customs {
        ui.push_id(&group.id, |ui| {
            group_card(ui, state, group, view, input, out);
        });
    }
}

/// The title entry of a new group: Enter or Create makes it, Esc or Cancel
/// drops it.
fn new_group_row(ui: &mut Ui, state: &mut AutomationsSheet, out: &mut AutomationsOut) {
    let focus = &mut state.focus_group_field;
    let Some(title) = &mut state.new_group else {
        return;
    };
    let mut create = false;
    let mut cancel = false;
    ui.horizontal(|ui| {
        let field = ui.add(
            TextEdit::singleline(title)
                .padded()
                .hint_text("Group name")
                .char_limit(80)
                .desired_width((ui.available_width() - 150.0).max(80.0)),
        );
        if std::mem::take(focus) {
            field.request_focus();
        }
        out.mark("field:new-group", field.rect);
        let ready = !title.trim().is_empty();
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if (out.button(ui, "create-group", "Create", ready) || entered) && ready {
            create = true;
        }
        if out.button(ui, "cancel-group", "Cancel", true) {
            cancel = true;
        }
    });
    if create {
        let title = title.trim().to_owned();
        state.group_error = None;
        out.actions.push(AutomationsAction::CreateGroup(title));
    } else if cancel {
        state.new_group = None;
        state.group_error = None;
    }
}

/// Rename and Delete of a custom group, each in place. Delete asks first and
/// says the automations stay.
fn custom_group_controls(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    group: &connecto_core::AutomationGroup,
    out: &mut AutomationsOut,
) {
    let id = &group.id;
    if let Some((_, text)) = state.renaming_group.as_mut().filter(|(rid, _)| rid == id) {
        let mut save = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            let field = ui.add(
                TextEdit::singleline(text)
                    .padded()
                    .char_limit(80)
                    .desired_width((ui.available_width() - 150.0).max(80.0)),
            );
            if std::mem::take(&mut state.focus_group_field) {
                field.request_focus();
            }
            out.mark(&format!("field:rename-group:{id}"), field.rect);
            let ready = !text.trim().is_empty();
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if (out.button(ui, &format!("save-rename:{id}"), "Save", ready) || entered) && ready {
                save = true;
            }
            if out.button(ui, &format!("cancel-rename:{id}"), "Cancel", true) {
                cancel = true;
            }
        });
        if save {
            let title = text.trim().to_owned();
            state.group_error = None;
            out.actions.push(AutomationsAction::RenameGroup {
                id: id.clone(),
                title,
            });
        } else if cancel {
            state.renaming_group = None;
            state.group_error = None;
        }
    } else if state.confirm_delete_group.as_deref() == Some(id.as_str()) {
        out.say(
            ui,
            RichText::new(format!(
                "Delete the group {}? Its automations are kept.",
                group.title
            ))
            .color(theme::ERROR),
        );
        ui.horizontal_wrapped(|ui| {
            if out.button(
                ui,
                &format!("confirm-delete-group:{id}"),
                "Delete group",
                true,
            ) {
                out.actions.push(AutomationsAction::DeleteGroup(id.clone()));
                state.confirm_delete_group = None;
            }
            if out.button(ui, &format!("keep-group:{id}"), "Keep", true) {
                state.confirm_delete_group = None;
            }
        });
    } else {
        ui.horizontal(|ui| {
            if out.button(ui, &format!("rename-group:{id}"), "Rename", true) {
                state.renaming_group = Some((id.clone(), group.title.clone()));
                state.group_error = None;
                state.focus_group_field = true;
            }
            if out.button_with(
                ui,
                &format!("delete-group:{id}"),
                "Delete group",
                true,
                |b| b.frame(false).fill(Color32::TRANSPARENT),
            ) {
                state.confirm_delete_group = Some(id.clone());
            }
        });
    }
}

/// A group: one switch for all of it, opened to show its automations, each
/// with its own switch. The whole header line opens and closes it. The user's
/// own groups can also be renamed and deleted, and take automations dropped
/// on them.
fn group_card(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    group: &connecto_core::AutomationGroup,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let members: Vec<&AutomationItem> = group
        .members
        .iter()
        .filter_map(|slug| view.automations.iter().find(|a| &a.slug == slug))
        .collect();
    let usable: Vec<String> = members
        .iter()
        .filter(|a| a.enabled || (!a.unfinished && a.errors.is_empty()))
        .map(|a| a.slug.clone())
        .collect();
    let on_count = members.iter().filter(|a| a.enabled).count();
    let all_on = !members.is_empty() && on_count == members.len();
    // An empty group of the user's stays open: it shows where to drop.
    let open = state.open_groups.contains(&group.id) || (group.custom && members.is_empty());
    let id = &group.id;
    let frame = egui::Frame::NONE
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(6.0)
        .inner_margin(Margin::same(10));
    let contents = |ui: &mut Ui| {
        ui.set_width(ui.available_width());
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
        out.mark(&format!("group-row:{id}"), rect);
        if response.clicked() {
            if open {
                state.open_groups.remove(id);
            } else {
                state.open_groups.insert(id.clone());
            }
        }
        let mut row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        let switch_response = switch(&mut row, all_on, !usable.is_empty(), "Enabled");
        out.mark(&format!("group-enabled:{id}"), switch_response.rect);
        if !usable.is_empty() && switch_response.clicked() {
            out.actions
                .push(AutomationsAction::SetManyEnabled(usable.clone(), !all_on));
        }
        let count = format!("{on_count} of {} on", members.len());
        row.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            #[cfg(test)]
            {
                out.hits
                    .texts
                    .push(if open { OPEN } else { CLOSED }.to_owned());
                out.hits.texts.push(group.title.clone());
                out.hits.texts.push(count.clone());
            }
            ui.add(egui::Label::new(muted(if open { OPEN } else { CLOSED })).selectable(false));
            ui.add(
                egui::Label::new(RichText::new(&group.title).color(Color32::WHITE))
                    .truncate()
                    .selectable(false),
            );
            ui.add(
                egui::Label::new(faint(count.clone()))
                    .truncate()
                    .selectable(false),
            );
        });
        if !open {
            return;
        }
        ui.add_space(4.0);
        if group.custom {
            custom_group_controls(ui, state, group, out);
            if members.is_empty() {
                out.say(ui, muted("Drag automations here"));
            }
        } else {
            out.say(ui, muted(group.about.clone()));
        }
        ui.add_space(4.0);
        ui.indent(("group-members", id), |ui| {
            for item in &members {
                ui.push_id(&item.slug, |ui| {
                    automation_card(ui, state, item, view, input, out);
                });
            }
        });
    };
    if group.custom {
        let (_, dropped) = ui.dnd_drop_zone::<Dragged, ()>(frame, contents);
        if let Some(dragged) = dropped
            && dragged.from.as_deref() != Some(id.as_str())
        {
            out.actions.push(AutomationsAction::SetGroup(
                dragged.slug.clone(),
                Some(id.clone()),
            ));
        }
    } else {
        frame.show(ui, contents);
    }
    ui.add_space(6.0);
}

/// The strip under the sheet's tabs: All, Claude, Codex, with how many each
/// holds. Choosing one filters the list and points New at that agent.
fn filter_strip(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    view: &AutomationsView,
    out: &mut AutomationsOut,
) {
    ui.horizontal(|ui| {
        for filter in filter_tabs() {
            let (name, count) = match filter {
                Filter::All => ("All", view.automations.len()),
                Filter::Only(kind) => (
                    kind.label(),
                    view.automations.iter().filter(|a| a.agent == kind).count(),
                ),
            };
            let text = format!("{name} ({count})");
            #[cfg(test)]
            out.hits.texts.push(text.clone());
            let response = ui.selectable_label(state.filter == filter, &text);
            out.mark(&format!("filter:{name}"), response.rect);
            if response.clicked() && state.filter != filter {
                state.filter = filter;
                remember(filter);
                if let Filter::Only(kind) = filter {
                    state.agent = kind;
                }
            }
        }
        // Top right of the list, as New automation is of the sheet.
        if state.new_group.is_none() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = ui.button("New group").on_hover_text(
                    "Make a group to switch several automations on or off together, then drag automations into it",
                );
                out.mark("new-group", button.rect);
                if button.clicked() {
                    state.new_group = Some(String::new());
                    state.group_error = None;
                    state.focus_group_field = true;
                }
            });
        }
    });
    ui.add_space(2.0);
}

fn setup_tab(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    view: &AutomationsView,
    out: &mut AutomationsOut,
) {
    key_section(ui, state, view, out);
    separator(ui);
    root_section(ui, state, view, out);
}

/// What still stops automations from running, in the user's words.
fn setup_problems(view: &AutomationsView) -> Vec<&'static str> {
    let mut problems = Vec::new();
    match view.key {
        KeyStatus::Missing => problems.push("The Jev API key is not set."),
        KeyStatus::Unavailable(_) => problems.push(crate::platform::KEY_STORE_UNAVAILABLE),
        KeyStatus::Unknown | KeyStatus::Present => {}
    }
    if view.root.is_none() {
        problems.push(ROOT_FIRST);
    }
    problems
}

/// One line on the Automations tab while Setup is not complete.
fn setup_callout(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    view: &AutomationsView,
    out: &mut AutomationsOut,
) {
    let problems = setup_problems(view);
    if problems.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        out.say(
            ui,
            RichText::new(problems.join(" "))
                .size(11.5)
                .color(theme::AMBER),
        );
        if out.button(ui, "open-setup", "Open Setup", true) {
            state.tab = Tab::Setup;
        }
    });
}

fn key_section(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    view: &AutomationsView,
    out: &mut AutomationsOut,
) {
    out.say(ui, heading("JEV API KEY"));
    let (status, color) = match &view.key {
        KeyStatus::Unknown => ("checking…".to_owned(), theme::FAINT),
        KeyStatus::Missing => ("not set".to_owned(), theme::AMBER),
        KeyStatus::Present => ("set".to_owned(), theme::GREEN),
        KeyStatus::Unavailable(why) => (
            format!("{} unavailable: {why}", crate::platform::KEY_STORE),
            theme::ERROR,
        ),
    };
    ui.horizontal_wrapped(|ui| {
        out.say(ui, RichText::new(format!("Key: {status}")).color(color));
        ui.hyperlink_to(
            RichText::new("Get a key at typesafe.ai").size(11.5),
            "https://typesafe.ai",
        );
    });
    ui.horizontal(|ui| {
        let field = ui.add(
            TextEdit::singleline(&mut state.key_input)
                .padded()
                .id(key_field_id())
                .password(true)
                .hint_text("Paste the Jev API key")
                .desired_width(ui.available_width() - 130.0),
        );
        out.mark("field:key", field.rect);
        let ready = !state.key_input.trim().is_empty();
        if out.button(ui, "save-key", "Save", ready) {
            out.actions.push(AutomationsAction::SaveKey(
                state.key_input.trim().to_owned(),
            ));
            state.key_input.clear();
            wipe_key_state(ui.ctx());
            state.key_error = None;
        }
        let present = view.key == KeyStatus::Present;
        if out.button(ui, "clear-key", "Clear", present) {
            out.actions.push(AutomationsAction::ClearKey);
            state.key_input.clear();
            wipe_key_state(ui.ctx());
        }
    });
    if let Some(error) = &state.key_error {
        out.say(ui, RichText::new(error).size(11.5).color(theme::ERROR));
    }
    out.say(
        ui,
        muted("Jev is only asked when an automation's trigger text appears on a terminal screen."),
    );
}

const ROOT_FIRST: &str = "Set Connecto Home in Settings first.";

fn root_section(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    view: &AutomationsView,
    out: &mut AutomationsOut,
) {
    out.say(ui, heading("AUTOMATIONS FOLDER (CONNECTO HOME)"));
    match &view.root {
        Some(path) => out.say(ui, RichText::new(format!("Folder: {}", path.display()))),
        None => out.say(ui, RichText::new("Folder: not set").color(theme::AMBER)),
    }
    out.say(
        ui,
        muted("Every automation lives in automations/ here and shares one project and one login. The folder is Connecto Home, chosen in Settings."),
    );
    ui.horizontal_wrapped(|ui| {
        if out.button(ui, "open-settings", "Open Settings…", true) {
            state.root_error = None;
            out.actions.push(AutomationsAction::OpenSettings);
        }
        let set = view.root.is_some();
        for (tool, text, key) in [
            (ToolKind::Claude, "Log in Claude", "login-claude"),
            (ToolKind::Codex, "Log in Codex", "login-codex"),
        ] {
            if out.button(ui, key, text, set) {
                state.root_error = None;
                out.actions.push(AutomationsAction::OpenLogin(tool));
            }
        }
    });
    if let Some(error) = &state.root_error {
        out.say(ui, RichText::new(error).size(11.5).color(theme::ERROR));
    }
}

fn status_line(ui: &mut Ui, view: &AutomationsView, out: &mut AutomationsOut) {
    let state = match view.status.state {
        EngineState::Starting => "starting",
        EngineState::NoKey => "waiting for an API key",
        EngineState::NoAutomations => "no enabled automation",
        EngineState::Paused => "paused",
        EngineState::Watching => "watching your terminals",
    };
    let mut line = format!("Engine: {state} · {} calls", view.status.calls);
    if view.status.in_flight > 0 {
        line.push_str(&format!(" · {} running", view.status.in_flight));
    }
    out.say(ui, faint(line));
    if let Some(error) = &view.status.last_error {
        out.say(
            ui,
            RichText::new(format!("Last error: {error}"))
                .size(11.5)
                .color(theme::ERROR),
        );
    }
}

fn pending_card(ui: &mut Ui, pending: &PendingConfirmation, out: &mut AutomationsOut) {
    egui::Frame::NONE
        .fill(theme::RAISED_BG)
        .corner_radius(6.0)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let terminal = terminal_label(pending.terminal_name.as_ref(), &pending.terminal_id);
            out.say(
                ui,
                RichText::new(format!("{} in {terminal}", pending.automation))
                    .color(Color32::WHITE),
            );
            out.say(
                ui,
                muted(format!(
                    "{} · {}% sure (needs {}%) · type: {}",
                    pending.project,
                    percent(pending.confidence),
                    percent(pending.threshold),
                    keys_summary(&pending.keys)
                )),
            );
            // Ids by confirmation, so a press that began on one never lands on
            // the one that takes its place.
            ui.push_id(pending.id, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if out.button(ui, &format!("approve:{}", pending.id), "Approve", true) {
                        out.actions.push(AutomationsAction::Approve(pending.id));
                    }
                    if out.button(ui, &format!("dismiss:{}", pending.id), "Dismiss", true) {
                        out.actions.push(AutomationsAction::Dismiss(pending.id));
                    }
                });
            });
        });
}

fn automation_card(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    item: &AutomationItem,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let has_root = view.root.is_some();
    let slug = &item.slug;
    let open = state.expanded.contains(slug);
    egui::Frame::NONE
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(6.0)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let drag = group_of(view, slug)
                .is_none_or(|group| group.custom)
                .then(|| Dragged {
                    slug: slug.clone(),
                    from: group_of(view, slug).map(|group| group.id.clone()),
                });
            summary_row(ui, state, item, open, drag, out);
            if !open {
                return;
            }
            ui.add_space(4.0);
            let agent = item.agent.label();
            let (last, last_color) = last_line(item.last.as_ref(), input);
            header_line(ui, item, agent, (&last, last_color), out);
            if item.unfinished {
                out.say(
                    ui,
                    RichText::new("Unfinished: open the session to complete it.")
                        .size(11.5)
                        .color(theme::AMBER),
                );
            }
            for error in &item.errors {
                out.say(ui, RichText::new(error).size(11.5).color(theme::ERROR));
            }
            out.say(
                ui,
                muted(format!(
                    "Matches: {} · Types: {}",
                    item.matches,
                    keys_summary(&item.keys)
                )),
            );
            let (shown, full) = trigger_line(&item.trigger);
            out.say_hover(ui, muted(shown), &full);
            if item.needs_trigger {
                ui.horizontal_wrapped(|ui| {
                    out.say(
                        ui,
                        RichText::new("Needs trigger text — not running. Open session to add one.")
                            .size(11.5)
                            .color(theme::AMBER),
                    );
                    if out.button(ui, &format!("open:{slug}"), "Open session", has_root) {
                        out.actions
                            .push(AutomationsAction::OpenSession(slug.clone()));
                    }
                });
            }
            controls_row(ui, state, item, view, input, out);
            if state.confirm_delete.as_deref() == Some(slug.as_str()) {
                // The prompt wraps above its buttons, so neither leaves the sheet.
                out.say(
                    ui,
                    RichText::new(format!("Delete {} and its folder?", item.name))
                        .color(theme::ERROR),
                );
                ui.horizontal_wrapped(|ui| {
                    if out.button(ui, &format!("confirm-delete:{slug}"), "Delete", true) {
                        out.actions.push(AutomationsAction::Delete(slug.clone()));
                        state.confirm_delete = None;
                    }
                    if out.button(ui, &format!("cancel-delete:{slug}"), "Keep", true) {
                        state.confirm_delete = None;
                    }
                });
            }
            audit_panel(ui, state, item, view, input, out);
        });
    ui.add_space(6.0);
}

/// The name of the automation being dragged, following the pointer.
fn drag_ghost(ctx: &egui::Context, name: &str) {
    static GHOST: std::sync::OnceLock<Id> = std::sync::OnceLock::new();
    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    let Some(pos) = ctx.pointer_interact_pos() else {
        return;
    };
    egui::Area::new(*GHOST.get_or_init(|| Id::unique("automation-drag-ghost")))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + vec2(12.0, 12.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(RichText::new(name).color(Color32::WHITE));
            });
        });
}

/// The trigger in a few words, for the collapsed row.
fn trigger_summary(trigger: &[String]) -> String {
    match trigger {
        [] => "No trigger".to_owned(),
        [one] => format!("\"{}\"", clip(one)),
        [first, rest @ ..] => format!("\"{}\" +{}", clip(first), rest.len()),
    }
}

const ROW_HEIGHT: f32 = 22.0;

/// The row that is always shown: name, trigger and the Enabled switch. A click
/// anywhere but the switch opens or closes the details.
fn summary_row(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    item: &AutomationItem,
    open: bool,
    drag: Option<Dragged>,
    out: &mut AutomationsOut,
) {
    let slug = &item.slug;
    // The row's own click area goes down first, so the switch above it wins.
    let sense = if drag.is_some() {
        Sense::click_and_drag()
    } else {
        Sense::click()
    };
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), sense);
    if let Some(dragged) = drag {
        response.dnd_set_drag_payload(dragged);
        if response.dragged() {
            drag_ghost(ui.ctx(), &item.name);
        }
    }
    out.mark(&format!("row:{slug}"), rect);
    if response.clicked() {
        if open {
            state.expanded.remove(slug);
        } else {
            state.expanded.insert(slug.clone());
        }
    }
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    let usable = item.enabled || (!item.unfinished && item.errors.is_empty());
    let switch_response = switch(&mut row, item.enabled, usable, "Enabled");
    out.mark(&format!("enabled:{slug}"), switch_response.rect);
    if usable && switch_response.clicked() {
        out.actions
            .push(AutomationsAction::SetEnabled(slug.clone(), !item.enabled));
    }
    row.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        // Not selectable, so none of these takes the click: the whole line is
        // the row's own click target (the switch is the only other one).
        #[cfg(test)]
        out.hits
            .texts
            .push(if open { OPEN } else { CLOSED }.to_owned());
        ui.add(egui::Label::new(muted(if open { OPEN } else { CLOSED })).selectable(false));
        #[cfg(test)]
        out.hits.texts.push(item.name.clone());
        ui.add(
            egui::Label::new(RichText::new(&item.name).color(Color32::WHITE))
                .truncate()
                .selectable(false),
        );
        let summary = trigger_summary(&item.trigger);
        #[cfg(test)]
        out.hits.texts.push(summary.clone());
        ui.add(
            egui::Label::new(faint(summary))
                .truncate()
                .selectable(false),
        );
    });
}

/// The state the pill shows: what the engine does with the automation.
fn status_pill(item: &AutomationItem) -> (&'static str, Color32) {
    if item.needs_trigger {
        ("Needs trigger", theme::AMBER)
    } else if item.enabled {
        ("Active", theme::GREEN)
    } else {
        ("Off", theme::FAINT)
    }
}

/// Below this width the latest result gets a line of its own.
const LAST_BESIDE_MIN: f32 = 520.0;

/// Name and badge on the left; the latest result and the status pill on the
/// right.
fn header_line(
    ui: &mut Ui,
    item: &AutomationItem,
    agent: &str,
    (last, last_color): (&str, Color32),
    out: &mut AutomationsOut,
) {
    let beside = ui.available_width() >= LAST_BESIDE_MIN;
    let last = RichText::new(last).size(11.0).color(last_color);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (word, color) = status_pill(item);
            let pill = egui::Frame::NONE
                .fill(color.gamma_multiply(0.18))
                .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
                .corner_radius(9.0)
                .inner_margin(Margin::symmetric(8, 1))
                .show(ui, |ui| {
                    out.say(ui, RichText::new(word).size(11.0).color(color));
                });
            out.mark(&format!("status:{}", item.slug), pill.response.rect);
            if beside {
                out.say(ui, last.clone());
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                #[cfg(test)]
                out.hits.texts.push(item.name.clone());
                ui.add(
                    egui::Label::new(RichText::new(&item.name).color(Color32::WHITE)).truncate(),
                );
                out.say(ui, faint(format!("{agent} · {}", item.question_type)));
            });
        });
    });
    if !beside {
        out.say(ui, last);
    }
}

fn text_width(ui: &Ui, text: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            egui::TextStyle::Button.resolve(ui.style()),
            Color32::WHITE,
        )
        .size()
        .x
}

fn button_width(ui: &Ui, text: &str) -> f32 {
    text_width(ui, text) + 2.0 * ui.spacing().button_padding.x
}

const SWITCH: egui::Vec2 = vec2(28.0, 16.0);

/// A switch with its label. The whole of it is the click target.
fn switch(ui: &mut Ui, on: bool, enabled: bool, label: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::TextStyle::Body.resolve(ui.style()),
        theme::TEXT_ROW,
    );
    let size = vec2(
        SWITCH.x + 6.0 + galley.size().x,
        SWITCH.y.max(galley.size().y),
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let dim = if enabled { 1.0 } else { 0.45 };
    let track = Rect::from_min_size(
        egui::pos2(rect.min.x, rect.center().y - SWITCH.y / 2.0),
        SWITCH,
    );
    let painter = ui.painter();
    let fill = if on { theme::TEAL } else { theme::RAISED_BG };
    painter.rect_filled(track, 99.0, fill.gamma_multiply(dim));
    painter.rect_stroke(
        track,
        99.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Inside,
    );
    let knob_x = if on {
        track.max.x - SWITCH.y / 2.0
    } else {
        track.min.x + SWITCH.y / 2.0
    };
    painter.circle_filled(
        egui::pos2(knob_x, track.center().y),
        SWITCH.y / 2.0 - 3.0,
        if on { theme::CYAN } else { theme::TEXT_MUTED }.gamma_multiply(dim),
    );
    painter.galley(
        egui::pos2(track.max.x + 6.0, rect.center().y - galley.size().y / 2.0),
        galley,
        theme::TEXT_ROW.gamma_multiply(dim),
    );
    let label = label.to_owned();
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::accesskit::Role::Switch, enabled, on, label.clone())
    });
    response
}

/// The threshold on the left; Open session, Audit and Delete on
/// the right. When they do not fit on one row, each group is a row of its own.
fn controls_row(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    item: &AutomationItem,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let confirming = state.confirm_delete.as_deref() == Some(item.slug.as_str());
    let spacing = ui.spacing().item_spacing.x;
    let left_w = button_width(ui, "−")
        + text_width(ui, "Fire at ≥ 100%")
        + button_width(ui, "+")
        + spacing * 3.0
        + 12.0;
    let mut right_w = button_width(ui, "Open session") + button_width(ui, "Audit") + spacing;
    if movable(view, &item.slug) {
        right_w += button_width(ui, "Group") + spacing;
    }
    if !confirming {
        right_w += button_width(ui, "Delete") + spacing;
    }
    let one_row = ui.available_width() >= left_w + right_w + 16.0;
    if one_row {
        ui.horizontal(|ui| {
            left_group(ui, item, out);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                right_group(ui, state, item, view, input, confirming, out);
            });
        });
    } else {
        ui.horizontal(|ui| left_group(ui, item, out));
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                right_group(ui, state, item, view, input, confirming, out);
            });
        });
    }
}

fn left_group(ui: &mut Ui, item: &AutomationItem, out: &mut AutomationsOut) {
    let slug = &item.slug;
    let pct = percent(item.threshold);
    if out.button(ui, &format!("lower:{slug}"), "−", pct > THRESHOLD_MIN) {
        out.actions.push(AutomationsAction::SetThreshold(
            slug.clone(),
            f64::from((pct - THRESHOLD_STEP).max(THRESHOLD_MIN)) / 100.0,
        ));
    }
    out.say(
        ui,
        RichText::new(format!("Fire at ≥ {pct}%"))
            .monospace()
            .size(12.0),
    );
    if out.button(ui, &format!("raise:{slug}"), "+", pct < 100) {
        out.actions.push(AutomationsAction::SetThreshold(
            slug.clone(),
            f64::from((pct + THRESHOLD_STEP).min(100)) / 100.0,
        ));
    }
}

/// Added right to left, so it reads Open session, Audit, Delete.
fn right_group(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    item: &AutomationItem,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    confirming: bool,
    out: &mut AutomationsOut,
) {
    let slug = &item.slug;
    if !confirming
        && out.button_with(ui, &format!("delete:{slug}"), "Delete", true, |button| {
            button.frame(false).fill(Color32::TRANSPARENT)
        })
    {
        state.confirm_delete = Some(slug.clone());
    }
    let open = state.audit.contains_key(slug);
    if out.button_with(ui, &format!("audit:{slug}"), "Audit", true, |button| {
        button.selected(open)
    }) {
        toggle_audit(state, slug, view, input);
    }
    if movable(view, slug) {
        move_menu(ui, item, view, out);
    }
    // The one Open session is beside the warning when a trigger is missing.
    if !item.needs_trigger
        && out.button_with(
            ui,
            &format!("open:{slug}"),
            "Open session",
            view.root.is_some(),
            |button| button.fill(theme::TEAL_DEEP),
        )
    {
        out.actions
            .push(AutomationsAction::OpenSession(slug.clone()));
    }
}

/// Whether the automation can change groups: not when a shipped group has it.
fn movable(view: &AutomationsView, slug: &str) -> bool {
    group_of(view, slug).is_none_or(|group| group.custom)
}

/// The same move as a drag, for the keyboard: No group, then each of the
/// user's groups, with the current one marked.
fn move_menu(ui: &mut Ui, item: &AutomationItem, view: &AutomationsView, out: &mut AutomationsOut) {
    let slug = &item.slug;
    let current = group_of(view, slug).map(|group| group.id.as_str());
    let menu = ui.menu_button("Group", |ui| {
        let mut choose = |ui: &mut Ui, key: String, label: &str, target: Option<&str>| {
            let here = current == target;
            let response = ui.selectable_label(here, label);
            out.mark(&key, response.rect);
            if response.clicked() {
                ui.close();
                if !here {
                    out.actions.push(AutomationsAction::SetGroup(
                        slug.clone(),
                        target.map(str::to_owned),
                    ));
                }
            }
        };
        choose(ui, format!("move-none:{slug}"), "No group", None);
        for group in view.groups.iter().filter(|group| group.custom) {
            choose(
                ui,
                format!("move-to:{slug}:{}", group.id),
                &group.title,
                Some(&group.id),
            );
        }
    });
    out.mark(&format!("move:{slug}"), menu.response.rect);
    menu.response
        .on_hover_text("Move to a group, or out of one");
}

fn clock_ms(at: std::time::SystemTime) -> u64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The one-line result of an automation's latest check.
fn last_line(last: Option<&LastCheck>, input: &AutomationsInput<'_>) -> (String, Color32) {
    let Some(last) = last else {
        return ("Not checked yet".to_owned(), theme::FAINT);
    };
    let time = when(clock_ms(last.at), input.now_ms, input.utc_offset_s);
    let percent = last.confidence.map(|c| format!("{}%", (c * 100.0).round()));
    let (head, detail, color) = match last.outcome {
        AutomationOutcome::NoMatch => (
            format!("Last checked {time}"),
            vec![last.terminal.clone(), "no match".to_owned()],
            theme::FAINT,
        ),
        AutomationOutcome::Error => (
            format!("Error {time}"),
            vec![last.answer.clone().unwrap_or_else(|| last.terminal.clone())],
            theme::ERROR,
        ),
        other => {
            let word = other.word();
            let head = format!("{}{} {time}", word[..1].to_uppercase(), &word[1..]);
            (
                head,
                vec![percent.unwrap_or_else(|| last.terminal.clone())],
                theme::FAINT,
            )
        }
    };
    let mut line = std::iter::once(head)
        .chain(detail)
        .collect::<Vec<_>>()
        .join(" · ");
    if last.repeat > 1 {
        line.push_str(&format!(" ×{}", last.repeat));
    }
    (line, color)
}

fn toggle_audit(
    state: &mut AutomationsSheet,
    slug: &String,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
) {
    if state.audit.remove(slug).is_some() {
        state.open_calls.retain(|(s, _)| s != slug);
    } else {
        // Read now; kept fresh by the panel while it stays open.
        state.audit.insert(
            slug.clone(),
            Audit {
                revision: view.revision,
                calls: read_calls(input, slug),
            },
        );
    }
}

fn audit_panel(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    item: &AutomationItem,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let slug = &item.slug;
    let Some(audit) = state.audit.get_mut(slug) else {
        return;
    };
    if audit.revision != view.revision {
        audit.revision = view.revision;
        audit.calls = read_calls(input, slug);
    }
    let count = audit.calls.len();
    ui.push_id("audit-header", |ui| {
        audit_header(ui, state, item, count, out);
    });
    let Some(audit) = state.audit.get(slug) else {
        return;
    };
    if audit.calls.is_empty() {
        out.say(ui, muted("No calls yet."));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(("audit-list", slug))
        .max_height(260.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for call in &audit.calls {
                ui.push_id(call.record.id, |ui| {
                    call_row(ui, slug, call, &mut state.open_calls, input, out);
                });
            }
        });
}

/// "Audit · N calls" and a subdued Clear audit, which asks first.
fn audit_header(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    item: &AutomationItem,
    count: usize,
    out: &mut AutomationsOut,
) {
    let slug = &item.slug;
    if state.confirm_clear.as_deref() == Some(slug.as_str()) {
        out.say(
            ui,
            RichText::new(format!("Clear {count} calls for {}?", item.name)).color(theme::ERROR),
        );
        ui.horizontal_wrapped(|ui| {
            if out.button(ui, &format!("confirm-clear:{slug}"), "Clear", true) {
                out.actions
                    .push(AutomationsAction::ClearAudit(slug.clone()));
                state.confirm_clear = None;
            }
            if out.button(ui, &format!("keep-clear:{slug}"), "Keep", true) {
                state.confirm_clear = None;
            }
        });
    } else {
        ui.horizontal(|ui| {
            out.say(ui, faint(format!("Audit · {count} calls")));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if out.button_with(
                    ui,
                    &format!("clear-audit:{slug}"),
                    "Clear audit",
                    count > 0,
                    |b| b.frame(false).fill(Color32::TRANSPARENT),
                ) {
                    state.confirm_clear = Some(slug.clone());
                }
            });
        });
    }
    if let Some((failed, error)) = &state.audit_error
        && failed == slug
    {
        out.say(ui, RichText::new(error).size(11.5).color(theme::ERROR));
    }
}

impl AutomationsSheet {
    /// Forget the cached calls of `slug`, so its open panel reads them again.
    pub(crate) fn refetch_audit(&mut self, slug: &str) {
        self.audit_error = None;
        self.open_calls.retain(|(s, _)| s != slug);
        if let Some(audit) = self.audit.get_mut(slug) {
            audit.revision = u64::MAX;
        }
    }
}

fn read_calls(input: &AutomationsInput<'_>, slug: &str) -> Vec<Call> {
    input
        .control
        .map(|control| control.history_for(slug))
        .unwrap_or_default()
        .into_iter()
        .map(Call::new)
        .collect()
}

fn call_row(
    ui: &mut Ui,
    slug: &str,
    call: &Call,
    open_calls: &mut std::collections::HashSet<(String, u64)>,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let record = &call.record;
    let id = record.id;
    let mut parts = vec![
        when(record.time_ms, input.now_ms, input.utc_offset_s),
        terminal_label(record.terminal_name.as_ref(), &record.terminal_id),
    ];
    if let Some(answer) = &record.answer {
        parts.push(format!("answer {answer}"));
    }
    if let Some(confidence) = record.confidence {
        parts.push(format!("{}%", percent(confidence)));
    }
    parts.push(record.outcome.word().to_owned());
    parts.push(format!("{} ms", record.latency_ms));
    let key = (slug.to_owned(), id);
    let expanded = open_calls.contains(&key);
    let line = format!(
        "{} {}",
        if expanded { OPEN } else { CLOSED },
        parts.join(" · ")
    );
    #[cfg(test)]
    out.hits.texts.push(line.clone());
    let response = ui.selectable_label(expanded, &line);
    out.mark(&format!("call:{slug}:{id}"), response.rect);
    if response.clicked() {
        if expanded {
            open_calls.remove(&key);
        } else {
            open_calls.insert(key);
        }
    }
    if !expanded {
        return;
    }
    if let Some(message) = &record.message {
        out.say(ui, muted(message));
    }
    for (label, name, body) in [
        ("Request JSON", "request", &call.request),
        ("Response JSON", "response", &call.response),
    ] {
        ui.horizontal(|ui| {
            out.say(ui, heading(label));
            if let Some(body) = body
                && out.button(ui, &format!("copy-{name}:{slug}:{id}"), "Copy", true)
            {
                ui.ctx().copy_text(body.clone());
            }
        });
        match body {
            Some(body) => {
                #[cfg(test)]
                out.hits.texts.push(body.clone());
                egui::ScrollArea::vertical()
                    .id_salt(name)
                    .max_height(140.0)
                    .show(ui, |ui| {
                        ui.add(
                            TextEdit::multiline(&mut body.as_str())
                                .padded()
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY),
                        );
                    });
            }
            None => out.say(ui, muted("(not recorded)")),
        }
    }
}

fn new_section(
    ui: &mut Ui,
    state: &mut AutomationsSheet,
    has_root: bool,
    out: &mut AutomationsOut,
) {
    out.say(ui, heading("NEW AUTOMATION"));
    out.say(
        ui,
        muted("A Claude or Codex terminal opens to write it. Describe it below, or leave both fields empty and paste a screenshot there."),
    );
    let [name_id, screen_id, action_id] = form_ids();
    let field = ui.add(
        TextEdit::singleline(&mut state.name)
            .padded()
            .id(name_id)
            .hint_text("Name, e.g. Accept edits")
            .desired_width(f32::INFINITY),
    );
    out.mark("field:name", field.rect);
    out.say(ui, muted("When the terminal shows… (optional)"));
    let field = ui.add(
        TextEdit::multiline(&mut state.screen).padded()
            .id(screen_id)
            .hint_text(
                "e.g. Claude asks 'Do you want to proceed?' with options 1. Yes 2. Yes, and don't ask again 3. No",
            )
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    out.mark("field:screen", field.rect);
    out.say(ui, muted("Do this… (optional)"));
    let field = ui.add(
        TextEdit::multiline(&mut state.action)
            .padded()
            .id(action_id)
            .hint_text("e.g. choose option 2")
            .desired_rows(2)
            .desired_width(f32::INFINITY),
    );
    out.mark("field:action", field.rect);
    let screen = state.screen.trim().to_owned();
    let action = state.action.trim().to_owned();
    let half = screen.is_empty() != action.is_empty();
    if half {
        out.say(
            ui,
            RichText::new("Fill in both fields to describe it here, or leave both empty and paste a screenshot in the session.")
                .size(11.5)
                .color(theme::AMBER),
        );
    }
    ui.horizontal_wrapped(|ui| {
        for (agent, text) in [(ToolKind::Claude, "Claude"), (ToolKind::Codex, "Codex")] {
            let response = ui.selectable_label(state.agent == agent, text);
            out.mark(&format!("agent:{text}"), response.rect);
            if response.clicked() {
                state.agent = agent;
            }
        }
        let ready = !state.name.trim().is_empty() && !half;
        if out.button(ui, "create", "Create", ready && has_root) {
            state.create_error = None;
            let name = state.name.trim().to_owned();
            out.actions.push(if screen.is_empty() {
                AutomationsAction::Create(name, state.agent)
            } else {
                AutomationsAction::CreateDescribed {
                    name,
                    agent: state.agent,
                    screen,
                    action,
                }
            });
        }
    });
    if !has_root {
        out.say(ui, muted(ROOT_FIRST));
    }
    if let Some(error) = &state.create_error {
        out.say(ui, RichText::new(error).size(11.5).color(theme::ERROR));
    }
}

fn history_tab(
    ui: &mut Ui,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    ui.horizontal(|ui| {
        out.say(ui, heading("RECENT CALLS"));
        if out.button(
            ui,
            "clear-history",
            "Clear history",
            !view.history.is_empty(),
        ) {
            out.actions.push(AutomationsAction::ClearHistory);
        }
    });
    if view.history.is_empty() {
        out.say(ui, muted("Nothing yet. Every call to Jev is listed here."));
    }
    for record in view.history.iter() {
        history_row(ui, record, view, input, out);
    }
}

fn outcome_color(outcome: AutomationOutcome) -> Color32 {
    match outcome {
        AutomationOutcome::Fired | AutomationOutcome::Approved => theme::GREEN,
        AutomationOutcome::Asked => theme::AMBER,
        AutomationOutcome::Error => theme::ERROR,
        AutomationOutcome::Dismissed | AutomationOutcome::Expired | AutomationOutcome::NoMatch => {
            theme::FAINT
        }
    }
}

fn history_row(
    ui: &mut Ui,
    record: &HistoryRecord,
    view: &AutomationsView,
    input: &AutomationsInput<'_>,
    out: &mut AutomationsOut,
) {
    let automation = if record.automation.is_empty() {
        "-".to_owned()
    } else {
        view.automations
            .iter()
            .find(|item| item.slug == record.automation)
            .map_or_else(|| record.automation.clone(), |item| item.name.clone())
    };
    let terminal = terminal_label(record.terminal_name.as_ref(), &record.terminal_id);
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        out.say(
            ui,
            RichText::new(record.outcome.word())
                .monospace()
                .color(outcome_color(record.outcome)),
        );
        out.say(ui, RichText::new(format!("{automation} · {terminal}")));
    });
    let mut line = vec![ago(record.time_ms, input.now_ms, input.utc_offset_s)];
    if let Some(answer) = &record.answer {
        line.push(format!("answer {answer}"));
    }
    if let Some(confidence) = record.confidence {
        line.push(format!("{}%", percent(confidence)));
    }
    line.push(format!("{} ms", record.latency_ms));
    out.say(ui, faint(line.join(" · ")));
    if let Some(message) = &record.message {
        out.say(ui, muted(message));
    }
}

/// The strip in the main window that raises the oldest confirmation while the
/// sheet is closed.
pub(crate) fn banner(
    ctx: &egui::Context,
    area: Rect,
    view: &AutomationsView,
    out: &mut AutomationsOut,
) {
    let Some(first) = view.pending.first() else {
        return;
    };
    let more = view.pending.len() - 1;
    egui::Area::new(Id::unique("automations-banner"))
        .order(egui::Order::Foreground)
        .anchor(Align2::RIGHT_BOTTOM, vec2(-12.0, -12.0))
        .constrain_to(area)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(theme::POPOVER_BG)
                .stroke(Stroke::new(1.0, theme::AMBER))
                .corner_radius(8.0)
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_max_width((area.width() - 48.0).clamp(160.0, 420.0));
                    let terminal = terminal_label(first.terminal_name.as_ref(), &first.terminal_id);
                    let extra = if more > 0 {
                        format!(" (+{more} more)")
                    } else {
                        String::new()
                    };
                    out.say(
                        ui,
                        RichText::new(format!("Automation needs your OK{extra}"))
                            .color(theme::AMBER),
                    );
                    out.say(
                        ui,
                        muted(format!(
                            "{} in {terminal}: type {} ({}% sure)",
                            first.automation,
                            keys_summary(&first.keys),
                            percent(first.confidence)
                        )),
                    );
                    ui.push_id(first.id, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if out.button(ui, "banner-approve", "Approve", true) {
                                out.actions.push(AutomationsAction::Approve(first.id));
                            }
                            if out.button(ui, "banner-dismiss", "Dismiss", true) {
                                out.actions.push(AutomationsAction::Dismiss(first.id));
                            }
                            if out.button(ui, "banner-review", "Review", true) {
                                out.actions.push(AutomationsAction::OpenSheet);
                            }
                        });
                    });
                });
        });
}
