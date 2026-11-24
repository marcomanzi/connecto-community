//! The memory itself (task T11): rooms with their counts, a room's index,
//! search, and one memory whole.
//!
//! Every read is a request; its answer is drawn from the project's `reads`,
//! pending or failed until it arrives. Nothing here blocks or touches disk.
//!
//! Each memory shows three things apart: its standing (what it claims), its
//! status (how reliable) and its freshness (how recently confirmed). Its open
//! notes sit beside it, and a contradiction comes right under the memory it
//! contradicts, wherever that memory is shown. Nothing hides it.

use connecto_core::memory::{
    Annotation, AnnotationKind, BootstrapState, EvidenceKind, Lookup, MemoryCard, MemoryDetail,
    MemoryId, MemoryStatus, ProjectMemoryView, ReadAnswer, ReadRequest, ReadResult, ReadState,
    Room, Scope, SearchHit, StatusCounts, UserStatementKind,
};
use egui::{Margin, RichText, Stroke, Ui};

use super::fetch;
use super::{Cx, MemoryAction, MemoryPanel, Slot, Tab, field, field_hinted, row, statement, text};
use crate::forms::{self, Out, card, heading, muted, problem, warning};
use crate::theme;

/// What the memory tab shows.
#[derive(Debug, Clone, Default)]
pub(crate) struct Browse {
    /// The search text.
    pub(crate) query: String,
    /// Search in one room only.
    pub(crate) room: Option<Room>,
    /// Search in one scope only, as typed (`branch:main` ...).
    pub(crate) scope: String,
    /// Search the archive too.
    pub(crate) all: bool,
    /// The list on show: a room's index or search results.
    pub(crate) list: Option<Shown>,
    /// The memory open over the list.
    pub(crate) detail: Option<Shown>,
    /// What the page shows of the palace as a whole, kept per slot.
    pub(crate) cache: super::fetch::Cache,
    /// The filters under the search box are open.
    pub(crate) filters_open: bool,
    /// The entrance's text is folded away.
    pub(crate) entrance_closed: bool,
}

/// Every input the entrance's text can depend on: the memory version, the
/// open notes and contradictions, freshness and backlog wording, and the
/// build's state. The text is asked for again when any of them changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BriefKey {
    version: u64,
    open_annotations: u32,
    open_contradictions: u32,
    considered_through_ms: Option<u64>,
    queued: u32,
    bootstrap: BootstrapState,
}

impl BriefKey {
    pub(crate) fn of(p: &ProjectMemoryView) -> Self {
        Self {
            version: p.version,
            open_annotations: p.open_annotations,
            open_contradictions: p.open_contradictions,
            considered_through_ms: p.considered_through_ms,
            queued: p.backlog.queued,
            bootstrap: p.bootstrap.state.clone(),
        }
    }
}

/// How often the entrance and the rooms are asked for again while shown.
pub(crate) const BRIEF_EVERY_MS: u64 = super::fetch::REFRESH_EVERY_MS;

/// A read on show. Its life, frame by frame (see [`show`] and [`due`]):
///
/// - Every request for it, by the user or automatic, records the memory's
///   data as the frame that sent it saw it (`asked`). The engine answers from
///   data at least that new, so an answer is never taken as fresher than it
///   is.
/// - When the data differs from `asked`, it is due to be asked for again.
///   When the latest read vanished from the view's bounded `reads`, it is due
///   once (`lost`); after that the user is offered "Ask again".
/// - A due request is sent, and only then recorded (`asked` moves, or `lost`
///   is set, even if the engine refuses it, so nothing is repeated until the
///   data changes again), only in a frame where the user asked for nothing:
///   the user's action always wins. A frame sends at most one read: the
///   user's last, or else the due one. A due request set aside is not
///   recorded, so it is sent in a later frame if it still applies.
/// - Navigation is settled before anything is due: after Back, what is due is
///   the list's refresh, never the closed memory's.
/// - The last answer seen ready (`fallback`) stays on show while newer ones
///   are pending, however many refreshes are in flight.
/// - Only the latest request counts. An answer to a request that a later one
///   replaced is never shown, even when it is ready first: it may predate
///   what is on show, and the latest answer is on its way. Until it arrives
///   the fallback stays.
#[derive(Debug, Clone)]
pub(crate) struct Shown {
    pub(crate) request: ReadRequest,
    /// The latest accepted read.
    pub(crate) id: u64,
    /// The memory's data when it was last asked for.
    pub(crate) asked: Stamp,
    /// The last read of this request seen ready.
    pub(crate) fallback: Option<u64>,
    /// The latest read vanished and was already asked for again once.
    pub(crate) lost: bool,
}

/// What changes when memories or notes change: a different stamp means an
/// answer on show may be out of date. Not the view's revision, which also
/// moves when a read is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Stamp {
    version: u64,
    totals: StatusCounts,
    open_annotations: u32,
    open_contradictions: u32,
}

impl Stamp {
    pub(crate) const fn of(p: &ProjectMemoryView) -> Self {
        Self {
            version: p.version,
            totals: p.totals,
            open_annotations: p.open_annotations,
            open_contradictions: p.open_contradictions,
        }
    }
}

impl Browse {
    /// Back to the palace's rooms: no list and no memory on show. The search
    /// text stays.
    pub(super) fn close(&mut self) {
        self.list = None;
        self.detail = None;
    }

    /// Forget the search (its text and filters) and close what it showed.
    pub(super) fn clear_search(&mut self) {
        self.query.clear();
        self.room = None;
        self.scope.clear();
        self.all = false;
        self.close();
    }

    /// A read was accepted with this id; `asked` is the memory's data as the
    /// frame that sent it saw it. Asking again for the read on show keeps
    /// its last ready answer, and keeps the memory open over a refreshed
    /// list.
    pub(super) fn landed(&mut self, slot: Slot, request: ReadRequest, id: u64, asked: Stamp) {
        let place = match slot {
            Slot::List => &mut self.list,
            Slot::Detail => &mut self.detail,
        };
        let same = place.take().filter(|s| s.request == request);
        let new_list = slot == Slot::List && same.is_none();
        *place = Some(Shown {
            request,
            id,
            asked,
            fallback: same.as_ref().and_then(|s| s.fallback),
            lost: same.is_some_and(|s| s.lost),
        });
        if new_list {
            self.detail = None;
        }
    }
}

/// Why a read on show is due to be asked for again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Due {
    /// The memory's data is now this.
    Changed(Stamp),
    /// The latest read vanished from `reads`.
    Lost,
}

/// Note a ready answer, then say whether the read is due again: for changed
/// data first, else for a read that vanished. Records nothing else: see
/// [`Shown`].
fn due(shown: &mut Shown, p: &ProjectMemoryView) -> Option<Due> {
    let latest = p.read(shown.id);
    if latest.is_some_and(|r| matches!(r.state, ReadState::Ready(_))) {
        shown.fallback = Some(shown.id);
        shown.lost = false;
    }
    let now = Stamp::of(p);
    if now != shown.asked {
        Some(Due::Changed(now))
    } else if latest.is_none() && !shown.lost {
        Some(Due::Lost)
    } else {
        None
    }
}

const fn is_read(action: &MemoryAction) -> bool {
    matches!(action, MemoryAction::Read(..))
}
pub(super) fn show(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let p = cx.project;
    panel.browse.cache.settle(p, cx.now_ms);
    let on_show = on_show(p, panel.browse.detail.as_ref());
    statement::settle(&mut panel.composer, on_show);
    said(ui, panel, out);
    if panel.browse.detail.is_none() {
        super::ask::show(ui, panel, cx, out);
        search(ui, panel, out);
        if panel.browse.list.is_none() {
            schedule(ui, panel, cx, out);
        }
        // The entrance is for a palace with memories, when nothing is chosen.
        let idle = panel.browse.list.is_none() && panel.browse.query.trim().is_empty();
        if idle && p.totals.total() > 0 {
            entrance(ui, panel, cx, out);
        }
    } else {
        // Back is settled first: what is due after it is the list's refresh.
        let mut back = false;
        ui.horizontal_wrapped(|ui| {
            back = out.button(ui, "mem-back", "‹ Back", true);
        });
        if back {
            panel.browse.detail = None;
        }
    }
    let pending = if panel.browse.detail.is_some() {
        detail_page(ui, panel, cx, out)
    } else {
        columns(ui, panel, cx, out)
    };
    settle_reads(panel, pending, out);
}

/// A memory on show, whole: its refresh, then the page.
fn detail_page(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    cx: &Cx<'_>,
    out: &mut Out<MemoryAction>,
) -> Option<(Slot, Due)> {
    let mut refresh = false;
    ui.horizontal_wrapped(|ui| {
        refresh = out.button(ui, "mem-refresh", "Refresh", true);
    });
    let shown = panel.browse.detail.as_mut()?;
    if refresh {
        out.actions
            .push(MemoryAction::Read(shown.request.clone(), Slot::Detail));
    }
    let pending = due(shown, cx.project).map(|d| (Slot::Detail, d));
    answer(ui, cx, shown, Slot::Detail, &mut panel.composer, out);
    // The note about this memory, and the statement notices, are on the page.
    let on = on_show(cx.project, panel.browse.detail.as_ref());
    say(ui, panel, on, out);
    pending
}

/// The rooms beside the list. Stacked when the window is narrow.
fn columns(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    cx: &Cx<'_>,
    out: &mut Out<MemoryAction>,
) -> Option<(Slot, Due)> {
    // An empty palace has no rooms to choose: only what to do next.
    if cx.project.totals.total() == 0 && panel.browse.list.is_none() {
        return list_column(ui, panel, cx, out);
    }
    let mut pending = None;
    if ui.available_width() >= WIDE_LAYOUT {
        ui.columns(2, |cols| {
            if let [left, right] = cols {
                room_column(left, &mut panel.browse, cx.project, out);
                pending = list_column(right, panel, cx, out);
            }
        });
    } else {
        room_column(ui, &mut panel.browse, cx.project, out);
        ui.add_space(8.0);
        pending = list_column(ui, panel, cx, out);
    }
    pending
}

/// The window is at least this wide: rooms and list sit side by side.
const WIDE_LAYOUT: f32 = 620.0;

/// Plain room names with their counts, "All" on top. Choosing one lists its
/// memories; "All" closes the list.
fn room_column(
    ui: &mut Ui,
    browse: &mut Browse,
    p: &ProjectMemoryView,
    out: &mut Out<MemoryAction>,
) {
    let all_on = browse.list.is_none();
    if out.choice(ui, "mem-rooms", all_on, "All") {
        browse.close();
    }
    for room in Room::ALL.iter().filter(|r| r.holds_memories()) {
        let n = p
            .rooms
            .iter()
            .find(|c| c.room == *room)
            .map_or(0, |c| c.counts.total());
        let on =
            matches!(&browse.list, Some(Shown { request: ReadRequest::Room(r), .. }) if r == room);
        let key = format!("mem-room:{}", room.word());
        let label = format!("{}  {n}", room.label());
        if out.choice(ui, &key, on, &label) && !on {
            out.actions
                .push(MemoryAction::Read(ReadRequest::Room(*room), Slot::List));
        }
    }
}

/// The list on show (a room's memories or search results), or what to do
/// when none is on show.
fn list_column(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    cx: &Cx<'_>,
    out: &mut Out<MemoryAction>,
) -> Option<(Slot, Due)> {
    let p = cx.project;
    let Some(shown) = panel.browse.list.as_mut() else {
        if p.totals.total() == 0 {
            overview(ui, panel, cx, out);
        } else {
            all_rooms(ui, panel, cx, out);
        }
        say(ui, panel, None, out);
        return None;
    };
    let mut refresh = false;
    ui.horizontal_wrapped(|ui| {
        refresh = out.button(ui, "mem-refresh", "Refresh", true);
    });
    if refresh {
        out.actions
            .push(MemoryAction::Read(shown.request.clone(), Slot::List));
    }
    let pending = due(shown, p).map(|d| (Slot::List, d));
    answer(ui, cx, shown, Slot::List, &mut panel.composer, out);
    say(ui, panel, None, out);
    pending
}

/// At most one read this frame: the user's last, or else the due one, and
/// that only when the user asked for nothing else.
fn settle_reads(
    panel: &mut MemoryPanel,
    pending: Option<(Slot, Due)>,
    out: &mut Out<MemoryAction>,
) {
    if let Some(last) = out.actions.iter().rposition(is_read) {
        let mut i = 0;
        out.actions.retain(|a| {
            let keep = !is_read(a) || i == last;
            i += 1;
            keep
        });
        return;
    }
    let Some((slot, due)) = pending else {
        return;
    };
    if !out.actions.is_empty() {
        return;
    }
    let place = match slot {
        Slot::List => &mut panel.browse.list,
        Slot::Detail => &mut panel.browse.detail,
    };
    if let Some(shown) = place {
        match due {
            Due::Changed(now) => shown.asked = now,
            Due::Lost => shown.lost = true,
        }
        out.actions
            .push(MemoryAction::Read(shown.request.clone(), slot));
    }
}

/// "Add a note…" for the room on show, and its composer.
fn say(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    on_show: Option<&MemoryDetail>,
    out: &mut Out<MemoryAction>,
) {
    // A note about the memory on show can be renewed against it as it is
    // now, keeping its text: the explicit way past a change under it.
    let renew = on_show.filter(|d| {
        !statement::locked(&panel.composer)
            && panel.composer.as_ref().is_some_and(|c| {
                c.kind == UserStatementKind::Note
                    && !c.is_active()
                    && c.bound.as_ref().is_some_and(|b| b.shown == d.memory.id)
            })
    });
    ui.horizontal_wrapped(|ui| {
        let free = statement::may_start(&panel.composer, None) || renew.is_some();
        let why = statement::why_not(&panel.composer);
        if out.button_hint(ui, "mem-say-note", "Add a note…", free, why) && free {
            panel.said = None;
            let kept = match (renew, panel.composer.take()) {
                (Some(_), Some(c)) => c.text,
                _ => String::new(),
            };
            let mut c = statement::Composer::new(
                UserStatementKind::Note,
                on_show.map(|d| statement::Bound::memory(&d.memory)),
                "Add a note",
            );
            c.text = kept;
            panel.composer = Some(c);
        }
        out.say(ui, muted(statement::PROVENANCE));
    });
    if panel
        .composer
        .as_ref()
        .is_some_and(|c| c.kind == UserStatementKind::Note)
    {
        statement::compose(ui, &mut panel.composer, on_show, out);
    }
    statement::suspended(ui, &mut panel.composer, out);
    forms::separator(ui);
}

/// The word that the last statement was recorded: drawn on every page.
fn said(ui: &mut Ui, panel: &MemoryPanel, out: &mut Out<MemoryAction>) {
    if let Some(said) = &panel.said {
        out.say(
            ui,
            RichText::new(format!(
                "{said} What is on show updates when the memory does."
            ))
            .size(11.5)
            .color(theme::GREEN),
        );
    }
}

/// The read whose memory is drawn for `shown`, as [`answer`] draws it: the
/// latest when it is ready, else the last ready one (kept on show while a
/// refresh is pending, gone, or failed), else the latest.
fn displayed<'a>(p: &'a ProjectMemoryView, shown: &Shown) -> Option<&'a ReadResult> {
    let fallback = shown
        .fallback
        .and_then(|id| p.read(id))
        .filter(|r| matches!(r.state, ReadState::Ready(_)));
    match (p.read(shown.id), fallback) {
        (Some(latest), _) if matches!(latest.state, ReadState::Ready(_)) => Some(latest),
        (_, Some(old)) => Some(old),
        (latest, None) => latest,
    }
}

/// The memory whose page is on show, if one is.
fn on_show<'a>(p: &'a ProjectMemoryView, detail: Option<&Shown>) -> Option<&'a MemoryDetail> {
    match &displayed(p, detail?)?.state {
        ReadState::Ready(ReadAnswer::Memory(d)) => Some(d),
        _ => None,
    }
}

/// The brief's text, as a plain paragraph.
fn brief_text(ui: &mut Ui, out: &mut Out<MemoryAction>, text: &str) {
    if text.trim().is_empty() {
        out.say(ui, muted("The brief is empty: nothing is known yet."));
    } else {
        out.say(
            ui,
            RichText::new(text.trim_end())
                .size(12.5)
                .color(theme::TEXT_ROW),
        );
    }
}
/// No list on show and an empty palace: the way to start is named.
fn overview(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let p = cx.project;
    if p.totals.total() == 0 {
        out.say(
            ui,
            RichText::new("The palace has no memories yet.")
                .strong()
                .color(theme::TEXT),
        );
        let mut go = "mem-go-build";
        if let Some(refused) = super::refusal(cx) {
            // The latest run kept nothing: say so, and say where the reasons are.
            out.say(
                ui,
                warning(format!(
                    "The latest run kept nothing from {} proposed. {}",
                    refused.proposed, refused.reason
                )),
            );
            if super::report_for(cx, refused.run).is_some() {
                go = "mem-go-report";
                out.say(ui, muted("The reasons are in Activity, under that run."));
            } else if p.last_report.is_some() {
                out.say(
                    ui,
                    muted("The report kept belongs to another run, so the reasons for this one are not shown."),
                );
            } else {
                out.say(
                    ui,
                    muted("The reasons were shown when the run finished and are not kept after the app restarts."),
                );
            }
        } else {
            out.say(
                ui,
                muted(
                    "Build it from the project in Activity, or start empty: it fills as you work.",
                ),
            );
        }
        if out.button(ui, go, "Open Activity", true) {
            panel.tab = Tab::Activity;
        }
    }
}

/// The search box, with its filters folded under a toggle. Empty, it does
/// nothing: the rooms and the entrance are shown instead.
fn search(ui: &mut Ui, panel: &mut MemoryPanel, out: &mut Out<MemoryAction>) {
    let query = panel.browse.query.clone();
    let mut typed = None;
    let mut go = false;
    let searching = !query.is_empty()
        || matches!(
            panel.browse.list,
            Some(Shown {
                request: ReadRequest::Search { .. },
                ..
            })
        );
    ui.horizontal_wrapped(|ui| {
        typed = field_hinted(
            ui,
            panel,
            out,
            "mem-search-field",
            &query,
            280.0,
            "Search memory text, e.g. release",
        );
        go = ui.input(|i| i.key_pressed(egui::Key::Enter)) && typed.is_some();
        if out.button(ui, "mem-search", "Search", true) {
            go = true;
        }
        if out.button(ui, "mem-search-clear", "Clear", searching) {
            out.actions.push(MemoryAction::ClearSearch);
        }
        let folded = if panel.browse.filters_open {
            "Hide filters"
        } else {
            "Filters"
        };
        if out.button(ui, "mem-filters", folded, true) {
            panel.browse.filters_open = !panel.browse.filters_open;
        }
    });
    if let Some(typed) = typed {
        panel.browse.query = typed;
    }
    if panel.browse.filters_open {
        row(ui, out, "Room", |ui, out| {
            if out.choice(
                ui,
                "mem-search-room:any",
                panel.browse.room.is_none(),
                "Any",
            ) {
                panel.browse.room = None;
            }
            for room in Room::ALL.iter().filter(|r| r.holds_memories()) {
                let key = format!("mem-search-room:{}", room.word());
                if out.choice(ui, &key, panel.browse.room == Some(*room), room.label()) {
                    panel.browse.room = Some(*room);
                }
            }
        });
        let scope = panel.browse.scope.clone();
        let mut scope_typed = None;
        row(ui, out, "Scope", |ui, out| {
            scope_typed = field(ui, panel, out, "mem-search-scope", &scope, 200.0);
            out.say(
                ui,
                muted("empty for any; project, component:<path>, branch:<name> or worktree:<path>"),
            );
        });
        if let Some(typed) = scope_typed {
            panel.browse.scope = typed.trim().to_owned();
        }
        let mut all = panel.browse.all;
        let response = ui.checkbox(&mut all, "Include the archive");
        out.mark("mem-search-all", response.rect);
        panel.browse.all = all;
    }
    if !go {
        return;
    }
    let query = panel.browse.query.trim().to_owned();
    if query.is_empty() {
        panel.error = Some("Type something to search for.".to_owned());
        return;
    }
    let scope = if panel.browse.scope.is_empty() {
        None
    } else {
        match Scope::parse(&panel.browse.scope) {
            Ok(scope) => Some(scope),
            Err(why) => {
                panel.error = Some(format!("Scope: {why}."));
                return;
            }
        }
    };
    out.actions.push(MemoryAction::Read(
        ReadRequest::Search {
            query,
            room: panel.browse.room,
            scope,
            all: panel.browse.all,
        },
        Slot::List,
    ));
}

/// Draw the answer to a read, or where it stands.
fn answer(
    ui: &mut Ui,
    cx: &Cx<'_>,
    shown: &Shown,
    slot: Slot,
    composer: &mut Option<statement::Composer>,
    out: &mut Out<MemoryAction>,
) {
    let request = &shown.request;
    let again = |ui: &mut Ui, out: &mut Out<MemoryAction>, label: &str| {
        if out.button(ui, "mem-read-again", label, true) {
            out.actions.push(MemoryAction::Read(request.clone(), slot));
        }
    };
    let fallback = shown
        .fallback
        .and_then(|id| cx.project.read(id))
        .filter(|r| matches!(r.state, ReadState::Ready(_)));
    let read = match (cx.project.read(shown.id), fallback) {
        (Some(latest), _) if !matches!(latest.state, ReadState::Pending) => latest,
        // Newer answers are on their way: the last one stays on show.
        (_, Some(old)) => old,
        (Some(latest), None) => latest,
        (None, None) if shown.lost => {
            out.say(ui, muted("This answer is no longer kept."));
            again(ui, out, "Ask again");
            return;
        }
        (None, None) => {
            out.say(ui, muted("Loading…"));
            return;
        }
    };
    match &read.state {
        ReadState::Pending => out.say(ui, muted("Loading…")),
        ReadState::Failed(why) => {
            out.say(ui, problem(format!("Could not read the memory: {why}")));
            again(ui, out, "Try again");
            if let Some(old) = fallback.filter(|r| r.id != read.id) {
                out.say(ui, muted("The last answer, from before:"));
                let older = Shown {
                    id: old.id,
                    fallback: None,
                    ..shown.clone()
                };
                answer(ui, cx, &older, slot, composer, out);
            }
        }
        ReadState::Ready(ReadAnswer::NotFound) => out.say(ui, muted("Not found.")),
        ReadState::Ready(ReadAnswer::Room(cards)) => {
            let title = match request {
                ReadRequest::Room(room) => room.label(),
                _ => "Room",
            };
            out.say(
                ui,
                RichText::new(format!("{title} · {}", text::memories(cards.len() as u32)))
                    .strong()
                    .color(theme::TEXT),
            );
            if let ReadRequest::Room(room) = request {
                out.say(ui, muted(text::room_about(*room)));
            }
            if cards.is_empty() {
                out.say(ui, muted("No memories in this room yet."));
            }
            for c in cards {
                ui.push_id(c.id.as_str(), |ui| memory_row(ui, c, cx, out));
            }
        }
        ReadState::Ready(ReadAnswer::Search(hits)) => {
            let query = match request {
                ReadRequest::Search { query, .. } => query.as_str(),
                _ => "",
            };
            if hits.is_empty() {
                out.say(
                    ui,
                    RichText::new(format!("No memories match “{query}”."))
                        .strong()
                        .color(theme::TEXT),
                );
                out.say(
                    ui,
                    muted("Try other words, or drop the room, scope or archive filter. Search looks in the memory text, so a word from a room's description may find nothing."),
                );
                if out.button(ui, "mem-clear-search-empty", "Clear search", true) {
                    out.actions.push(MemoryAction::ClearSearch);
                }
                return;
            }
            out.say(
                ui,
                RichText::new(format!("{} results for “{query}”", hits.len()))
                    .strong()
                    .color(theme::TEXT),
            );
            for (i, hit) in hits.iter().enumerate() {
                ui.push_id(("hit", i), |ui| match hit {
                    SearchHit::Memory(c) => memory_row(ui, c, cx, out),
                    SearchHit::Annotation(a) => card(ui, |ui| {
                        annotation(ui, a, cx, true, None, out);
                    }),
                });
            }
        }
        ReadState::Ready(ReadAnswer::Memory(detail)) => {
            memory_detail(ui, detail, cx, composer, out);
        }
        // The entrance's overview is not built yet: its text is shown as it is.
        ReadState::Ready(ReadAnswer::Brief(text)) => {
            for line in text.lines() {
                out.say(ui, RichText::new(line).size(12.0).color(theme::TEXT_ROW));
            }
        }
        ReadState::Ready(ReadAnswer::Annotation(a)) => card(ui, |ui| {
            annotation(ui, a, cx, true, None, out);
        }),
    }
}

/// Open notes in the order they are shown: contradictions first.
fn ordered(open: &[Annotation]) -> Vec<&Annotation> {
    let mut list: Vec<&Annotation> = open.iter().filter(|a| a.is_open()).collect();
    list.sort_by_key(|a| a.kind != AnnotationKind::Contradiction);
    list
}

/// A memory in a list: its first line, then one muted line with its kind, its
/// standing and status in plain words, and how fresh it is. An open
/// contradiction is drawn right under it, so no list hides it.
fn memory_row(ui: &mut Ui, c: &MemoryCard, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let title = text::title(&c.summary);
    let response =
        ui.add(egui::Button::new(RichText::new(title).size(13.5).color(theme::TEXT)).frame(false));
    out.mark(&format!("mem-open:{}", c.id), response.rect);
    out.text_mark(title);
    if response.clicked() {
        out.actions.push(open(&c.id));
    }
    let fresh: Vec<String> = text::freshness(&c.freshness, cx.now_ms, cx.offset_s)
        .into_iter()
        .map(|(part, _)| part)
        .collect();
    let mut line = format!(
        "{} · {} · {}",
        c.kind.label(),
        c.standing.label(),
        c.status.label()
    );
    if !fresh.is_empty() {
        line.push_str(&format!(" · {}", fresh.join(" · ")));
    }
    out.say(ui, muted(line));
    // An open contradiction is never hidden: its text sits under the memory.
    for a in ordered(&c.open)
        .into_iter()
        .filter(|a| a.kind == AnnotationKind::Contradiction)
    {
        out.say(
            ui,
            RichText::new(format!("Contradicted: {}", a.text))
                .size(12.0)
                .color(theme::ERROR),
        );
    }
    ui.add_space(10.0);
}

fn open(id: &MemoryId) -> MemoryAction {
    MemoryAction::Read(ReadRequest::Open(Lookup::Memory(id.clone())), Slot::Detail)
}
/// One note. A contradiction is drawn in the error colour, with its source.
fn annotation(
    ui: &mut Ui,
    a: &Annotation,
    cx: &Cx<'_>,
    with_target: bool,
    acting: Option<(&mut Option<statement::Composer>, &MemoryDetail)>,
    out: &mut Out<MemoryAction>,
) {
    let contradiction = a.kind == AnnotationKind::Contradiction;
    let color = if contradiction {
        theme::ERROR
    } else if a.is_open() {
        theme::AMBER
    } else {
        theme::FAINT
    };
    egui::Frame::NONE
        .fill(color.gamma_multiply(0.08))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.5)))
        .corner_radius(6.0)
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let word = if contradiction {
                    "CONTRADICTION".to_owned()
                } else {
                    a.kind.label().to_uppercase()
                };
                out.say(ui, RichText::new(word).size(10.0).strong().color(color));
                out.say(ui, RichText::new(&a.text).size(12.0).color(theme::TEXT));
            });
            let mut about = if statement::from_window(a) {
                format!(
                    "{} · {} · {}",
                    a.id,
                    statement::FROM_WINDOW,
                    crate::utility::ago(a.created_ms, cx.now_ms, cx.offset_s)
                )
            } else {
                format!(
                    "{} · from {} · {} · {}",
                    a.id,
                    text::origin(&a.origin),
                    a.verification.label(),
                    crate::utility::ago(a.created_ms, cx.now_ms, cx.offset_s)
                )
            };
            if let Some(standing) = a.standing {
                about.push_str(&format!(" · {}", standing.label()));
            }
            if let Some(version) = a.absorbed_in {
                about.push_str(&format!(" · absorbed in version {version}"));
            }
            out.say(ui, muted(about));
            out.say(
                ui,
                muted(format!("source: {}", text::annotation_sources(a))),
            );
            if let Some((composer, d)) = acting
                && a.is_open()
            {
                let bound = statement::Bound::annotation(&d.memory, a);
                let wording = statement::for_annotation(a);
                statement::actions(ui, composer, &bound, &wording, true, out);
                if composer.as_ref().is_some_and(|c| c.under(&bound.subject)) {
                    statement::compose(ui, composer, Some(d), out);
                }
            }
            if with_target {
                ui.horizontal_wrapped(|ui| {
                    out.say(ui, muted(text::target(a)));
                    if let Some(id) = a.target_memory() {
                        let key = format!("mem-open:{id}");
                        if out.button(ui, &key, "Open", true) {
                            out.actions.push(open(id));
                        }
                    }
                });
            }
        });
    ui.add_space(3.0);
}

/// One memory, whole.
fn memory_detail(
    ui: &mut Ui,
    d: &MemoryDetail,
    cx: &Cx<'_>,
    composer: &mut Option<statement::Composer>,
    out: &mut Out<MemoryAction>,
) {
    let m = &d.memory;
    if let Some(from) = &d.redirected_from {
        out.say(ui, muted(format!("{from} moved here.")));
    }
    // The whole text first, then what it says about itself.
    out.say(
        ui,
        RichText::new(&m.summary)
            .size(15.0)
            .strong()
            .color(theme::TEXT),
    );
    let notes = ordered(&d.annotations);
    for a in notes
        .iter()
        .filter(|a| a.kind == AnnotationKind::Contradiction)
    {
        annotation(ui, a, cx, false, Some((&mut *composer, d)), out);
    }
    if !m.details.is_empty() {
        out.say(ui, RichText::new(&m.details).size(13.0).color(theme::TEXT));
    }
    out.say(
        ui,
        muted(format!(
            "{} · {} · {} · {}",
            m.kind.label(),
            m.standing.label(),
            m.status.label(),
            text::freshness(&m.freshness, cx.now_ms, cx.offset_s)
                .into_iter()
                .map(|(part, _)| part)
                .collect::<Vec<_>>()
                .join(" · ")
        )),
    );
    out.say(
        ui,
        muted(format!(
            "{} · {} · version {} · updated {}",
            m.address,
            m.id,
            m.version,
            crate::utility::ago(m.updated_ms, cx.now_ms, cx.offset_s)
        )),
    );
    let bound = statement::Bound::memory(m);
    // The engine accepts no verdict on a memory that was replaced or
    // archived: only a correction or a note, and the way to its successor.
    let retired = if m.status == MemoryStatus::Superseded {
        Some("superseded")
    } else if m.room() == Room::Archive {
        Some("archived")
    } else {
        None
    };
    if let Some(word) = retired {
        out.say(
            ui,
            warning(format!(
                "This memory is {word}: act on the one that replaced it."
            )),
        );
        // The live memories that replaced it, as the engine resolves them.
        ui.horizontal_wrapped(|ui| {
            for id in &d.superseded_by {
                let key = format!("mem-successor:{id}");
                if out.button(ui, &key, &format!("Open {id}"), true) {
                    out.actions.push(open(id));
                }
            }
        });
    }
    forms::separator(ui);
    out.say(ui, heading("SOURCES"));
    for (kind, n) in text::count_sources(&m.sources) {
        out.say(
            ui,
            RichText::new(format!("{} ({n})", kind.label()))
                .strong()
                .size(12.0)
                .color(theme::TEXT),
        );
        for s in m.sources.iter().filter(|s| s.kind == kind) {
            let evidence = d.evidence.iter().find(|e| e.id == s.evidence);
            let mut line = s.evidence.to_string();
            if let Some(e) = evidence {
                line.push_str(&format!(
                    " · {}",
                    crate::utility::ago(e.created_ms, cx.now_ms, cx.offset_s)
                ));
                match (&e.terminal, kind) {
                    // A user statement with no terminal was made in this
                    // window: certain, unlike a prompt credited to the user.
                    (None, EvidenceKind::UserStatement) => {
                        line.push_str(&format!(" · {}", statement::WINDOW_SOURCE));
                    }
                    (Some(terminal), EvidenceKind::UserStatement) => {
                        line.push_str(&format!(" · prompt typed in {terminal}"));
                    }
                    (Some(terminal), _) => line.push_str(&format!(" · {terminal}")),
                    (None, _) => {}
                }
                if let Some(branch) = &e.branch {
                    line.push_str(&format!(" · branch {branch}"));
                }
            }
            out.say(ui, muted(line));
            if let Some(e) = evidence
                && !e.excerpt.get().is_empty()
            {
                out.say(
                    ui,
                    RichText::new(e.excerpt.get())
                        .size(11.5)
                        .color(theme::TEXT_ROW),
                );
            }
        }
    }
    forms::separator(ui);
    out.say(ui, heading("SCOPE"));
    for s in &m.scopes {
        let line = text::scope(&s.scope, s.gone);
        if s.gone {
            out.say(ui, warning(line));
        } else {
            out.say(ui, RichText::new(line).size(12.0).color(theme::TEXT_ROW));
        }
    }
    if !m.anchors.is_empty() {
        out.say(ui, heading("ANCHORS"));
        if m.freshness.anchors_changed {
            out.say(
                ui,
                warning("An anchored file changed since it was observed. That does not make the memory false."),
            );
        }
        for anchor in &m.anchors {
            let short = anchor.blob.get(..10).unwrap_or(&anchor.blob);
            out.say(ui, muted(format!("{} @ {short}", anchor.path)));
        }
    }
    if !m.relations.is_empty() {
        out.say(ui, heading("RELATIONS"));
        for (i, r) in m.relations.iter().enumerate() {
            ui.push_id(("relation", i), |ui| {
                ui.horizontal_wrapped(|ui| {
                    out.say(
                        ui,
                        RichText::new(format!("{} {}", r.kind.label(), r.target))
                            .size(12.0)
                            .color(theme::TEXT_ROW),
                    );
                    let key = format!("mem-open:{}", r.target);
                    if out.button(ui, &key, "Open", true) {
                        out.actions.push(open(&r.target));
                    }
                });
            });
        }
    }
    forms::separator(ui);
    for a in notes
        .iter()
        .filter(|a| a.kind != AnnotationKind::Contradiction)
    {
        annotation(ui, a, cx, false, Some((&mut *composer, d)), out);
    }
    statement::actions(
        ui,
        composer,
        &bound,
        &statement::for_memory(m.kind, m.standing),
        retired.is_none(),
        out,
    );
    out.say(ui, muted(statement::PROVENANCE));
    if composer.as_ref().is_some_and(|c| c.under(&bound.subject)) {
        statement::compose(ui, composer, Some(d), out);
    }
    let absorbed: Vec<&Annotation> = d.annotations.iter().filter(|a| !a.is_open()).collect();
    if !absorbed.is_empty() {
        forms::separator(ui);
        out.say(ui, heading("ABSORBED NOTES"));
        for a in absorbed {
            annotation(ui, a, cx, false, None, out);
        }
    }
}

/// Memories in a room, by the palace's counts.
fn room_count(p: &ProjectMemoryView, room: Room) -> u32 {
    p.rooms
        .iter()
        .find(|c| c.room == room)
        .map_or(0, |c| c.counts.total())
}

/// The entrance: the project's brief, as `connecto memory brief` prints it.
/// Drawn from the cache: the last answer kept, marked as refreshing while a
/// newer one is on its way.
fn entrance(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    ui.horizontal(|ui| {
        out.say(ui, muted("Brief for agents, from this memory"));
        let label = if panel.browse.entrance_closed {
            "Show"
        } else {
            "Hide"
        };
        if out.button(ui, "mem-entrance-toggle", label, true) {
            panel.browse.entrance_closed = !panel.browse.entrance_closed;
        }
    });
    if panel.browse.entrance_closed {
        ui.add_space(6.0);
        return;
    }
    let key = BriefKey::of(cx.project);
    let slot = &panel.browse.cache.entrance;
    let mut again = false;
    ui.vertical(|ui| {
        match slot.value() {
            Some(text) => {
                brief_text(ui, out, text);
                if slot.refreshing(&key) {
                    out.say(ui, muted("Refreshing…"));
                }
            }
            None if slot.failed_for(&key).is_none() => out.say(ui, muted("Loading…")),
            None => {}
        }
        if let Some(why) = slot.failed_for(&key) {
            out.say(ui, problem(format!("Could not load the brief: {why}")));
            again = out.button(ui, "mem-brief-again", "Try again", true);
        }
    });
    if again {
        out.actions.push(MemoryAction::ReadEntrance {
            key,
            at_ms: cx.now_ms,
        });
    }
    ui.add_space(8.0);
}

/// Every memory of the palace, room by room in room order. Each room is drawn
/// from its own slot: ready, refreshing, loading or failed, on its own.
fn all_rooms(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let p = cx.project;
    let key = BriefKey::of(p);
    for room in populated_rooms(p) {
        ui.push_id(room.word(), |ui| {
            room_block(
                ui,
                room,
                panel.browse.cache.room(room),
                room_count(p, room),
                cx,
                out,
                &key,
            );
        });
    }
}

/// One room in the all-rooms view: its heading, then its memories, or what its
/// slot says about them.
fn room_block(
    ui: &mut Ui,
    room: Room,
    slot: Option<&fetch::Slot<Vec<MemoryCard>>>,
    count: u32,
    cx: &Cx<'_>,
    out: &mut Out<MemoryAction>,
    key: &BriefKey,
) {
    out.say(
        ui,
        RichText::new(format!("{} · {}", room.label(), text::memories(count)))
            .strong()
            .color(theme::TEXT),
    );
    let mut again = false;
    ui.vertical(|ui| {
        match slot.and_then(|s| s.value()) {
            Some(cards) => {
                for c in cards {
                    ui.push_id(c.id.as_str(), |ui| memory_row(ui, c, cx, out));
                }
                let shown = cards.len() as u32;
                if count > shown {
                    ui.horizontal_wrapped(|ui| {
                        out.say(ui, muted(format!("and {} more — ", count - shown)));
                        let key_text = format!("mem-all-more:{}", room.word());
                        if out.button(ui, &key_text, &format!("open {}", room.label()), true) {
                            out.actions
                                .push(MemoryAction::Read(ReadRequest::Room(room), Slot::List));
                        }
                    });
                }
                if slot.is_some_and(|s| s.refreshing(key)) {
                    out.say(ui, muted("Refreshing…"));
                }
            }
            None if slot.and_then(|s| s.failed_for(key)).is_none() => {
                out.say(ui, muted("Loading…"));
            }
            None => {}
        }
        if let Some(why) = slot.and_then(|s| s.failed_for(key)) {
            out.say(ui, problem(format!("Could not read this room: {why}")));
            let key_text = format!("mem-all-retry:{}", room.word());
            again = out.button(ui, &key_text, "Try again", true);
        }
    });
    if again {
        out.actions.push(MemoryAction::ReadRoomSlot {
            room,
            key: key.clone(),
            at_ms: cx.now_ms,
        });
    }
    ui.add_space(6.0);
}

/// The rooms that hold memories now, in room order.
fn populated_rooms(p: &ProjectMemoryView) -> Vec<Room> {
    Room::ALL
        .iter()
        .copied()
        .filter(|r| r.holds_memories() && room_count(p, *r) > 0)
        .collect()
}

/// Ask the engine for each slot whose answer is missing or for another key,
/// one batch at a time: at most one batch every REFRESH_SPACING_MS, and only
/// when no request of ours is in flight, so a batch never evicts an answer
/// still on its way. Only on the rooms overview: a batch could evict the
/// answers of a memory or a search being read.
fn schedule(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let p = cx.project;
    let now = cx.now_ms;
    let key = BriefKey::of(p);
    let cache = &mut panel.browse.cache;
    // A batch waits while the user asks for something else this frame.
    let user_asks = out.actions.iter().any(|a| {
        !matches!(
            a,
            MemoryAction::ReadRoomSlot { .. } | MemoryAction::ReadEntrance { .. }
        )
    });
    let since_batch = cache.last_batch_ms.map(|t| now.saturating_sub(t));
    let spaced = since_batch.is_none_or(|t| t >= fetch::REFRESH_SPACING_MS);
    if !user_asks && spaced && !cache.busy() && p.totals.total() > 0 {
        let mut batch = false;
        if cache.entrance.wanted(&key, now) {
            out.actions.push(MemoryAction::ReadEntrance {
                key: key.clone(),
                at_ms: now,
            });
            batch = true;
        }
        for room in populated_rooms(p) {
            if cache.room(room).is_none_or(|s| s.wanted(&key, now)) {
                out.actions.push(MemoryAction::ReadRoomSlot {
                    room,
                    key: key.clone(),
                    at_ms: now,
                });
                batch = true;
            }
        }
        if batch {
            cache.last_batch_ms = Some(now);
        }
    }
    // Wake the window for what is due next: an answer or a timeout, the
    // spacing, or the minute.
    if cache.busy() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    } else if let Some(t) = since_batch.filter(|t| *t < fetch::REFRESH_SPACING_MS) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(
                fetch::REFRESH_SPACING_MS - t,
            ));
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(BRIEF_EVERY_MS));
}
