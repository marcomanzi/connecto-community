//! `connecto memory <verb>` (spec §6): what one command does and prints. The
//! engine's `MemoryBridge` calls [`execute`] on a bridge handler thread with
//! the project's writer handle, a reader of its own and the current view.
//!
//! - `brief`, `search`, `read`, `room` read inside one snapshot, render under
//!   the budget ([`crate::render`]) and record in `served` exactly the
//!   subjects the page showed, before the page is returned: if that record
//!   cannot be written, the page is not given out (the rule against
//!   self-confirmation depends on it).
//! - `status` reads the view and the counts; it shows no memory content.
//! - `propose` and `flag` refuse an empty text or one over 8 KiB (never cut),
//!   redact it with the host's known secrets and the pattern rules, then in
//!   one writer transaction: queue a high-priority `proposta_agente` event,
//!   store the text as evidence of that event, and create an open annotation
//!   sourced on it with origin `agente:<terminal>` and verification
//!   `non_verificata`. The same note sent again by the same session is
//!   recognised by the event's key and stored once. An agent's note never
//!   claims more than the weakest standing of its kind: only the user can make
//!   a proposal a decision.
//! - With memory off, every verb answers [`NOT_ENABLED`] alone.
//! - Every output has the known secrets replaced once more before it is
//!   returned, for a value added to the vault after its text was stored.
//!
//! Errors are fixed sentences plus ids: nothing the agent typed is echoed.

use std::collections::BTreeSet;
use std::fmt::Write;

use connecto_core::memory::{
    Annotation, AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget, BootstrapState,
    Cycle, EvidenceKind, Lookup, MemoryCaller, MemoryCard, MemoryCommand, MemoryId, MemoryKind,
    MemoryStatus, NOT_ENABLED, ProjectMemoryView, ReadAnswer, Room, RunOutcome, Scope, Standing,
    Subject, Verb, Verification,
};

use rusqlite::{OptionalExtension, params};

use crate::codec::to_i64;
use crate::error::StoreError;
use crate::queue::{EventState, NewEvent};
use crate::read::{Read, Reader, SearchQuery};
use crate::render::{self, Page};
use crate::schema;
use crate::store::{NewAnnotation, NewEvidence, Store, create_annotation_in, insert_evidence_in};
use crate::writer::WriterHandle;

/// What a command printed, and whether it stored a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub text: String,
    /// A `propose` or `flag` stored its note.
    pub proposed: bool,
}

impl CommandOutput {
    fn text(text: String) -> Self {
        Self {
            text,
            proposed: false,
        }
    }
}

/// The kind of the event a `propose` or `flag` queues.
pub const PROPOSAL_EVENT: &str = "proposta_agente";

/// Most search results considered before the budget.
const SEARCH_LIMIT: usize = 50;
/// Most notes not yet in a memory a brief lists.
const BRIEF_NOTES: usize = 20;

/// Run one command for `caller`. `now_ms` is Unix milliseconds.
#[must_use]
#[allow(clippy::too_many_arguments)] // the contract with the engine names each input
pub fn execute(
    writer: &WriterHandle,
    reader: &Reader,
    view: &ProjectMemoryView,
    caller: &MemoryCaller,
    command: &MemoryCommand,
    text: Option<&str>,
    known_secrets: &[String],
    now_ms: u64,
) -> CommandOutput {
    let mut out = run(
        writer,
        reader,
        view,
        caller,
        command,
        text,
        known_secrets,
        now_ms,
    );
    out.text = hide_known(&out.text, known_secrets);
    out
}

/// Every known secret value replaced, in what is about to be printed or
/// sent to a model: a value added to the vault after a text was stored never
/// leaves again. Every host-assembled prompt goes through it last.
pub(crate) fn hide_known(text: &str, known_secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in known_secrets.iter().filter(|s| !s.is_empty()) {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), REDACTED);
        }
    }
    out
}

/// What a removed secret is replaced by (the redactor's mark).
const REDACTED: &str = "[REDACTED]";

#[allow(clippy::too_many_arguments)]
fn run(
    writer: &WriterHandle,
    reader: &Reader,
    view: &ProjectMemoryView,
    caller: &MemoryCaller,
    command: &MemoryCommand,
    text: Option<&str>,
    known_secrets: &[String],
    now_ms: u64,
) -> CommandOutput {
    if !view.enabled {
        return CommandOutput::text(NOT_ENABLED.to_owned());
    }
    if let Err(error) = command.check_text(text) {
        let tail = if command.verb().takes_text() {
            " Nothing was stored."
        } else {
            ""
        };
        return CommandOutput::text(format!("{error}.{tail}\n"));
    }
    match command {
        MemoryCommand::Status { budget } => CommandOutput::text(status(
            reader,
            view,
            budget_of(*budget, Verb::Status),
            now_ms,
        )),
        MemoryCommand::Propose {
            kind,
            about,
            room,
            scope,
        } => propose(
            writer,
            caller,
            Proposal {
                verb: Verb::Propose,
                kind: *kind,
                about: about.clone(),
                room: *room,
                scope: scope.clone(),
            },
            text.unwrap_or_default(),
            known_secrets,
            now_ms,
        ),
        MemoryCommand::Flag { target, kind } => propose(
            writer,
            caller,
            Proposal {
                verb: Verb::Flag,
                kind: *kind,
                about: Some(target.clone()),
                room: None,
                scope: None,
            },
            text.unwrap_or_default(),
            known_secrets,
            now_ms,
        ),
        MemoryCommand::Brief { .. }
        | MemoryCommand::Search { .. }
        | MemoryCommand::Read { .. }
        | MemoryCommand::Room { .. } => {
            let page = match reader.snapshot() {
                Ok(snapshot) => page(&snapshot.read(), view, caller, command, now_ms),
                Err(error) => Err(error),
            };
            match page {
                Ok((text, shown)) => serve(writer, caller, command.verb(), text, shown, now_ms),
                Err(error) => CommandOutput::text(read_failed(&error)),
            }
        }
    }
}

fn budget_of(budget: Option<u32>, verb: Verb) -> u32 {
    budget.or_else(|| verb.default_budget()).unwrap_or(600)
}

fn read_failed(error: &StoreError) -> String {
    format!(
        "project memory could not be read: {}\n",
        render::clean(&error.to_string())
    )
}

/// Record what the page showed, then give the page out.
fn serve(
    writer: &WriterHandle,
    caller: &MemoryCaller,
    verb: Verb,
    text: String,
    shown: Vec<Subject>,
    now_ms: u64,
) -> CommandOutput {
    if shown.is_empty() {
        return CommandOutput::text(text);
    }
    let session = caller.tool_session.clone();
    let terminal = caller.terminal.clone();
    let recorded = writer.call(move |store| {
        store.record_served(session.as_deref(), Some(&terminal), verb, &shown, now_ms)
    });
    match recorded {
        Ok(Ok(())) => CommandOutput::text(text),
        Ok(Err(error)) | Err(error) => CommandOutput::text(format!(
            "project memory could not record what it would show you ({}), so it shows nothing. Try again.\n",
            render::clean(&error.to_string())
        )),
    }
}

/// Whether a memory's scopes apply to a caller on `branch`: any scope that
/// is not another branch does.
fn in_scope(card: &MemoryCard, branch: Option<&str>) -> bool {
    let Some(branch) = branch else { return true };
    card.scopes.iter().any(|entry| match &entry.scope {
        Scope::Branch(name) => name == branch,
        Scope::Project | Scope::Component(_) | Scope::Worktree(_) => true,
    })
}

fn note_in_scope(scopes: &[Scope], branch: Option<&str>) -> bool {
    let Some(branch) = branch else { return true };
    scopes.is_empty()
        || scopes.iter().any(|scope| match scope {
            Scope::Branch(name) => name == branch,
            _ => true,
        })
}

const fn status_rank(status: MemoryStatus) -> u8 {
    match status {
        MemoryStatus::Conflict => 0,
        MemoryStatus::Verified => 1,
        MemoryStatus::ToVerify => 2,
        MemoryStatus::Candidate => 3,
        MemoryStatus::Superseded => 4,
    }
}

fn meta_line(view: &ProjectMemoryView, now_ms: u64) -> String {
    let freshness = view.considered_through_ms.map_or_else(
        || "no event considered yet".to_owned(),
        |ms| format!("events considered through {}", render::when(ms, now_ms)),
    );
    let backlog = match view.backlog.oldest_queued_ms {
        Some(oldest) if view.backlog.queued > 0 => format!(
            "{} events queued, oldest {}",
            view.backlog.queued,
            render::ago(oldest, now_ms)
        ),
        _ => format!("{} events queued", view.backlog.queued),
    };
    format!(
        "version {} | freshness: {freshness} | backlog: {backlog}",
        view.version
    )
}

pub(crate) fn page(
    read: &Read<'_>,
    view: &ProjectMemoryView,
    caller: &MemoryCaller,
    command: &MemoryCommand,
    now_ms: u64,
) -> Result<(String, Vec<Subject>), StoreError> {
    let budget = budget_of(command.budget(), command.verb());
    match command {
        MemoryCommand::Brief { .. }
        | MemoryCommand::Room {
            room: Room::Entrance,
            ..
        } => brief(read, view, caller, budget, now_ms),
        MemoryCommand::Room { room, .. } => {
            let mut page = Page::new(budget);
            let cards = read.room(*room)?;
            for card in &cards {
                page.add(render::card_entry(card, now_ms, None));
            }
            if cards.is_empty() {
                page.note("This room holds no memories yet.");
            }
            let meta = vec![format!(
                "room {}: {} memories | {}",
                room.word(),
                cards.len(),
                meta_line(view, now_ms)
            )];
            Ok(page.finish(&meta))
        }
        MemoryCommand::Search {
            query,
            room,
            scope,
            all,
            ..
        } => {
            let hits = read.search(&SearchQuery {
                text: query.clone(),
                room: *room,
                scope: scope.clone(),
                all: *all,
                limit: SEARCH_LIMIT,
            })?;
            let carried: BTreeSet<AnnotationId> = hits
                .iter()
                .filter_map(|hit| match hit {
                    connecto_core::memory::SearchHit::Memory(card) => Some(card),
                    connecto_core::memory::SearchHit::Annotation(_) => None,
                })
                .flat_map(|card| card.open.iter().map(|a| a.id.clone()))
                .collect();
            let mut page = Page::new(budget);
            let mut found = 0;
            for hit in &hits {
                match hit {
                    connecto_core::memory::SearchHit::Memory(card) => {
                        found += 1;
                        page.add(render::card_entry(card, now_ms, None));
                    }
                    connecto_core::memory::SearchHit::Annotation(a) => {
                        if !carried.contains(&a.id) {
                            found += 1;
                            page.add(render::annotation_entry(a, now_ms, None));
                        }
                    }
                }
            }
            if found == 0 {
                page.note("Nothing matched.");
            }
            let archive = if *all {
                "archive included"
            } else {
                "archive left out (add --all)"
            };
            let meta = vec![format!(
                "search: {found} results, best first, {archive} | {}",
                meta_line(view, now_ms)
            )];
            Ok(page.finish(&meta))
        }
        MemoryCommand::Read { target, .. } => {
            let mut page = Page::new(budget);
            match read.detail(target)? {
                ReadAnswer::Memory(detail) => {
                    for entry in render::detail_entries(&detail, now_ms) {
                        page.add(entry);
                    }
                }
                ReadAnswer::Annotation(a) => {
                    page.add(render::annotation_entry(&a, now_ms, None));
                    if let Some(id) = a.target_memory()
                        && let Some(memory) = read.memory(id)?
                    {
                        let card = read.card(&memory)?;
                        page.add(render::card_entry(
                            &card,
                            now_ms,
                            Some("the memory it is about"),
                        ));
                    }
                }
                ReadAnswer::NotFound
                | ReadAnswer::Search(_)
                | ReadAnswer::Room(_)
                | ReadAnswer::Brief(_) => {
                    page.note(&format!(
                        "No memory or note {} in this project.",
                        lookup_words(target)
                    ));
                }
            }
            Ok(page.finish(&[meta_line(view, now_ms)]))
        }
        MemoryCommand::Propose { .. }
        | MemoryCommand::Flag { .. }
        | MemoryCommand::Status { .. } => Ok((String::new(), Vec::new())),
    }
}

/// The lookup as asked, which is an id or address the parser accepted.
fn lookup_words(lookup: &Lookup) -> String {
    render::clean(&lookup.to_string())
}

/// The brief the Connecto window shows as the palace entrance: the
/// command's page for the whole project (no branch), with the command's
/// default budget and cleaning. The window is the user, not an agent
/// session, so nothing is recorded as served (like the window's other
/// reads).
pub(crate) fn window_brief(
    read: &Read<'_>,
    view: &ProjectMemoryView,
    now_ms: u64,
) -> Result<String, StoreError> {
    let caller = MemoryCaller {
        terminal: String::new(),
        tool_session: None,
        branch: None,
    };
    let budget = budget_of(None, Verb::Brief);
    brief(read, view, &caller, budget, now_ms).map(|(text, _)| text)
}

/// Sections of the brief, in order, after the open contradictions.
const BRIEF_ROOMS: &[(Room, &str)] = &[
    (Room::Conventions, "conventions"),
    (Room::Workshop, "procedures"),
    (Room::Problems, "known problems"),
    (Room::OpenWork, "open work"),
    (Room::Decisions, "decisions"),
    (Room::Architecture, "architecture"),
];

fn brief(
    read: &Read<'_>,
    view: &ProjectMemoryView,
    caller: &MemoryCaller,
    budget: u32,
    now_ms: u64,
) -> Result<(String, Vec<Subject>), StoreError> {
    let branch = caller.branch.as_deref();
    let mut meta = vec![meta_line(view, now_ms)];
    let scope = branch.map_or_else(
        || "scope: whole project".to_owned(),
        |b| format!("scope: project and branch {}", render::clean(b)),
    );
    let first_build = view.initializing() && view.version == 0;
    if first_build {
        meta.push(
            "initialisation in progress: the first version of this project's memory is still being built. \
             This is NOT a complete brief: only the notes available so far are listed."
                .to_owned(),
        );
    } else if view.initializing() {
        meta.push("an initial build is running: memories may change when it publishes.".to_owned());
    }
    meta.push(format!(
        "{scope} | open notes: {} | open contradictions: {}",
        view.open_annotations, view.open_contradictions
    ));
    let mut page = Page::new(budget);
    if !first_build {
        let mut cards = Vec::new();
        for memory in read.memories()? {
            if memory.room() == Room::Archive || memory.status == MemoryStatus::Superseded {
                continue;
            }
            let card = read.card(&memory)?;
            if in_scope(&card, branch) {
                cards.push((memory.updated_ms, card));
            }
        }
        cards.sort_by(|(a_ms, a), (b_ms, b)| {
            status_rank(a.status)
                .cmp(&status_rank(b.status))
                .then(b_ms.cmp(a_ms))
        });
        let contradicted = |card: &MemoryCard| {
            card.open
                .iter()
                .any(|a| a.kind == AnnotationKind::Contradiction)
        };
        for (_, card) in cards.iter().filter(|(_, c)| contradicted(c)) {
            page.add(render::card_entry(
                card,
                now_ms,
                Some("open contradictions"),
            ));
        }
        for (room, section) in BRIEF_ROOMS {
            for (_, card) in cards
                .iter()
                .filter(|(_, c)| c.address.room() == *room && !contradicted(c))
            {
                page.add(render::card_entry(card, now_ms, Some(section)));
            }
        }
        if cards.is_empty() {
            page.note("No memories yet.");
        }
    }
    let mut notes: Vec<_> = read
        .open_annotations()?
        .into_iter()
        .filter(|a| {
            (first_build || a.target_memory().is_none()) && note_in_scope(&a.scopes, branch)
        })
        .collect();
    notes.sort_by_key(|a| std::cmp::Reverse(a.seq));
    let listed = notes
        .len()
        .min(if first_build { usize::MAX } else { BRIEF_NOTES });
    for a in notes.iter().take(listed) {
        page.add(render::annotation_entry(
            a,
            now_ms,
            Some("notes not yet in a memory"),
        ));
    }
    if notes.len() > listed {
        page.note(&format!(
            "{} older notes not listed: find them with `connecto memory search <text>`.",
            notes.len() - listed
        ));
    }
    if first_build && notes.is_empty() {
        page.note("No notes yet.");
    }
    page.note("Read one in full: connecto memory read <id>. Leave a note: connecto memory propose <<'EOF'");
    Ok(page.finish(&meta))
}

fn outcome_words(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Running => "running".to_owned(),
        RunOutcome::Annotated { written, rejected } => {
            format!("{written} notes written, {rejected} rejected")
        }
        RunOutcome::Published {
            version,
            applied,
            rejected,
        } => format!("published version {version} ({applied} changes, {rejected} rejected)"),
        RunOutcome::NoChange => "nothing to change".to_owned(),
        RunOutcome::Refused { reason, .. } => format!("nothing kept: {}", render::clean(reason)),
        RunOutcome::HeldByPause => {
            "held: Connecto was paused; it runs again after Resume".to_owned()
        }
        RunOutcome::Held { reason } => format!("held: {}", render::clean(reason)),
        RunOutcome::Cancelled => "cancelled".to_owned(),
        RunOutcome::Interrupted => "interrupted (the app had stopped)".to_owned(),
        RunOutcome::Failed { reason } => format!("failed: {}", render::clean(reason)),
    }
}

fn bootstrap_words(view: &ProjectMemoryView) -> String {
    let b = &view.bootstrap;
    let progress = if b.packets_total > 0 {
        format!(" ({} of {} packets)", b.packets_done, b.packets_total)
    } else {
        String::new()
    };
    match &b.state {
        BootstrapState::NotStarted => "not started".to_owned(),
        BootstrapState::Queued => "queued".to_owned(),
        BootstrapState::Running => format!("initialisation in progress{progress}"),
        BootstrapState::Interrupted => format!("interrupted{progress}; it can resume"),
        BootstrapState::Failed { reason } => format!("failed: {}", render::clean(reason)),
        BootstrapState::Completed => "completed".to_owned(),
    }
}

/// `status`: counts and state only, no memory content.
fn status(reader: &Reader, view: &ProjectMemoryView, budget: u32, now_ms: u64) -> String {
    let mut lines = Vec::new();
    let counts = reader.read().counts();
    let totals = counts.as_ref().map_or(view.totals, |c| c.totals);
    let by_status: Vec<String> = MemoryStatus::ALL
        .iter()
        .filter(|s| totals.get(**s) > 0)
        .map(|s| format!("{} {}", totals.get(*s), s.word()))
        .collect();
    lines.push(format!(
        "enabled: yes | version: {} | memories: {}{}",
        view.version,
        totals.total(),
        if by_status.is_empty() {
            String::new()
        } else {
            format!(" ({})", by_status.join(", "))
        }
    ));
    let (open, contradictions) = counts
        .as_ref()
        .map_or((view.open_annotations, view.open_contradictions), |c| {
            (c.open_annotations, c.open_contradictions)
        });
    lines.push(format!(
        "open notes: {open} | open contradictions: {contradictions}"
    ));
    lines.push(format!(
        "freshness: {}",
        view.considered_through_ms.map_or_else(
            || "no event considered yet".to_owned(),
            |ms| format!("events considered through {}", render::when(ms, now_ms))
        )
    ));
    let b = &view.backlog;
    let mut backlog = format!("backlog: {} events queued", b.queued);
    if let Some(oldest) = b.oldest_queued_ms.filter(|_| b.queued > 0) {
        let _ = write!(backlog, ", oldest {}", render::ago(oldest, now_ms));
    }
    let _ = write!(
        backlog,
        " | failed batches: {} | lost when full: {}",
        b.failed_batches, b.dropped_on_full
    );
    if let Some(why) = &b.last_failure {
        let _ = write!(backlog, " | last failure: {}", render::clean(why));
    }
    lines.push(backlog);
    for cycle in [Cycle::Development, Cycle::Dream] {
        let state = view.cycle(cycle);
        let last = state.and_then(|s| s.last.as_ref()).map_or_else(
            || "no run yet".to_owned(),
            |run| {
                format!(
                    "last run {}: {}",
                    render::when(run.started_ms, now_ms),
                    outcome_words(&run.outcome)
                )
            },
        );
        let running = if state.is_some_and(|s| s.running) {
            " | running now"
        } else {
            ""
        };
        lines.push(format!("{}: {last}{running}", cycle.word()));
    }
    lines.push(format!("initial build: {}", bootstrap_words(view)));
    lines.push(view.next_dream_ms.map_or_else(
        || "next dream: none scheduled (dreams run only while the app is open)".to_owned(),
        |ms| {
            format!(
                "next dream: {} (only while the app is open)",
                render::utc(ms)
            )
        },
    ));
    let limit = usize::try_from(budget)
        .unwrap_or(usize::MAX)
        .saturating_mul(render::CHARS_PER_TOKEN);
    let mut used = 0;
    let mut out = String::new();
    let mut left = 0;
    for line in &lines {
        let cost = line.chars().count() + 1;
        if used + cost <= limit {
            used += cost;
            out.push_str(line);
            out.push('\n');
        } else {
            left += 1;
        }
    }
    if left > 0 {
        let _ = writeln!(
            out,
            "{left} more status lines not shown: connecto memory status --budget {}",
            budget
                .saturating_mul(2)
                .min(connecto_core::memory::command::MAX_BUDGET)
        );
    }
    let _ = writeln!(
        out,
        "[budget: about {} of {budget} tokens used; tokens are estimated as characters / 4]",
        used.div_ceil(render::CHARS_PER_TOKEN)
    );
    format!("[connecto memory] Status of this project's memory.\n{out}")
}

struct Proposal {
    verb: Verb,
    kind: AnnotationKind,
    about: Option<MemoryId>,
    room: Option<Room>,
    scope: Option<Scope>,
}

/// The kind of memory a note for `room` would become.
const fn room_kind(room: Room) -> Option<MemoryKind> {
    match room {
        Room::Architecture => Some(MemoryKind::Fact),
        Room::Workshop => Some(MemoryKind::Procedure),
        Room::Decisions => Some(MemoryKind::Decision),
        Room::Problems => Some(MemoryKind::Problem),
        Room::OpenWork => Some(MemoryKind::Work),
        Room::Conventions => Some(MemoryKind::Convention),
        Room::Entrance | Room::Archive => None,
    }
}

/// FNV-1a, for an event's dedup key.
fn fnv(parts: &[&str]) -> u64 {
    parts.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, part| {
        part.bytes().chain(std::iter::once(0)).fold(hash, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    })
}

fn propose(
    writer: &WriterHandle,
    caller: &MemoryCaller,
    p: Proposal,
    text: &str,
    known_secrets: &[String],
    now_ms: u64,
) -> CommandOutput {
    let refused = |why: &str| CommandOutput::text(format!("{why} Nothing was stored.\n"));
    if caller.terminal.is_empty() || render::clean(&caller.terminal) != caller.terminal {
        return refused("The calling terminal could not be identified.");
    }
    let Ok(redacted) = connecto_redact::redact(text, known_secrets) else {
        return refused("The text could not be checked for secrets.");
    };
    if connecto_core::memory::check_note_text(redacted.get()).is_err() {
        return refused("After secrets were removed, the text is empty or too long.");
    }
    let id = AnnotationId::from_entropy(crate::codec::entropy());
    let target = match (&p.about, p.room) {
        (Some(memory), _) => AnnotationTarget::Memory(memory.clone()),
        (None, Some(room)) => AnnotationTarget::New { room },
        (None, None) if p.kind == AnnotationKind::Task => AnnotationTarget::New {
            room: Room::OpenWork,
        },
        (None, None) => AnnotationTarget::New {
            room: Room::Decisions,
        },
    };
    // What the note would become, at the weakest standing of its kind: an
    // agent's words are a proposal, a plan or a hypothesis, never more.
    let explicit_kind = match (p.kind, p.room) {
        (AnnotationKind::Task, _) => Some(MemoryKind::Work),
        (_, Some(room)) => room_kind(room),
        _ => None,
    };
    let payload = serde_json::json!({
        "annotation": id.as_str(),
        "verb": p.verb.word(),
        "kind": p.kind.word(),
        "about": p.about.as_ref().map(MemoryId::as_str),
        "room": match &target {
            AnnotationTarget::New { room } => Some(room.word()),
            AnnotationTarget::Memory(_) => None,
        },
        "terminal": caller.terminal,
        "tool_session": caller.tool_session,
        "branch": caller.branch,
    })
    .to_string();
    let scope = p.scope.clone().unwrap_or(Scope::Project);
    // Every value that changes what is stored, as it will be stored: the same
    // note is the same only if all of them are.
    let target_key = match &target {
        AnnotationTarget::Memory(id) => format!("memory:{id}"),
        AnnotationTarget::New { room } => format!("room:{}", room.word()),
    };
    let key = format!(
        "{PROPOSAL_EVENT}:{:016x}",
        fnv(&[
            &caller.terminal,
            caller.tool_session.as_deref().unwrap_or("\u{0}none"),
            caller.branch.as_deref().unwrap_or("\u{0}none"),
            p.verb.word(),
            p.kind.word(),
            &target_key,
            &scope.to_string(),
            redacted.get(),
        ])
    );
    let event = NewEvent {
        dedup_key: key,
        kind: PROPOSAL_EVENT.to_owned(),
        created_ms: now_ms,
        payload,
        priority: true,
        internal: false,
    };
    let mut evidence = NewEvidence::new(EvidenceKind::AgentProposal, now_ms, redacted.clone());
    evidence.terminal = Some(caller.terminal.clone());
    evidence.tool_session.clone_from(&caller.tool_session);
    evidence.branch.clone_from(&caller.branch);
    let annotation = NewAnnotation {
        id: Some(id),
        created_ms: now_ms,
        kind: p.kind,
        target,
        text: redacted,
        scopes: vec![scope],
        sources: Vec::new(),
        verification: Verification::Unverified,
        origin: AnnotationOrigin::Agent {
            terminal: caller.terminal.clone(),
        },
        memory_kind: explicit_kind,
        standing: None,
    };
    let stored = writer.call(move |store| store_proposal(store, event, evidence, annotation));
    match stored {
        Ok(Ok(Stored::Duplicate(Some(id)))) => refused(&format!(
            "This session already proposed the same note: {id} (read it with `connecto memory read {id}`)."
        )),
        Ok(Ok(Stored::Duplicate(None))) => refused("This session already proposed the same note."),
        Ok(Ok(Stored::New(a))) => {
            let about = match &a.target {
                AnnotationTarget::Memory(id) => format!(" about {id}"),
                AnnotationTarget::New { room } => format!(" for the room {}", room.word()),
            };
            CommandOutput {
                text: format!(
                    "Stored note {id} ({kind}{about}), origin agente:{terminal}, verification non_verificata.\n\
                     Other terminals can read it now: connecto memory read {id}\n\
                     It stays a proposal until the user confirms it; the development cycle will consider it first.\n",
                    id = a.id,
                    kind = a.kind.word(),
                    terminal = render::clean(&caller.terminal),
                ),
                proposed: true,
            }
        }
        Ok(Err(StoreError::UnknownMemory(id))) => {
            refused(&format!("There is no memory {id} in this project."))
        }
        Ok(Err(error)) | Err(error) => refused(&format!(
            "The note could not be stored: {}.",
            render::clean(&error.to_string())
        )),
    }
}

/// What [`store_proposal`] did.
#[derive(Debug)]
enum Stored {
    New(Annotation),
    /// The same note from the same session was already stored; its
    /// annotation, when found.
    Duplicate(Option<AnnotationId>),
}

/// The proposal write, in one transaction: the priority event, the evidence
/// of that event and the annotation sourced on it are written together or
/// not at all. A second submission of the same note by the same session
/// finds the first event's key and writes nothing.
fn store_proposal(
    store: &mut Store,
    event: NewEvent,
    mut evidence: NewEvidence,
    mut annotation: NewAnnotation,
) -> Result<Stored, StoreError> {
    let project_id = store.project_id().to_owned();
    let tx = store.begin()?;
    let existing: Option<i64> = tx
        .query_row(
            "SELECT seq FROM event WHERE dedup_key = ?1",
            [&event.dedup_key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(seq) = existing {
        let subject: Option<String> = tx
            .query_row(
                "SELECT subject FROM source_link WHERE evidence_seq = ?1 AND subject LIKE 'p-%'",
                [seq],
                |r| r.get(0),
            )
            .optional()?;
        return Ok(Stored::Duplicate(
            subject.and_then(|s| AnnotationId::parse(&s).ok()),
        ));
    }
    if let AnnotationTarget::Memory(target) = &annotation.target {
        let memory = Read::new(&tx)
            .memory(target)?
            .ok_or_else(|| StoreError::UnknownMemory(target.clone()))?;
        annotation.memory_kind = Some(memory.kind);
    }
    // An agent's note claims the weakest standing of its kind, never more;
    // a correction or contradiction claims none.
    if !annotation.kind.needs_target() {
        annotation.standing = annotation.memory_kind.map(Standing::default_for);
    }
    let seq = schema::next_counter(&tx, "event_seq")?;
    tx.execute(
        "INSERT INTO event (seq, project_id, dedup_key, kind, state, priority, created_ms, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            to_i64(seq),
            project_id,
            event.dedup_key,
            event.kind,
            EventState::Queued.word(),
            event.priority,
            to_i64(event.created_ms),
            event.payload
        ],
    )?;
    evidence.event_seq = Some(seq);
    let evidence = insert_evidence_in(&tx, &project_id, evidence)?;
    annotation.sources = vec![evidence.id];
    let annotation = create_annotation_in(&tx, &project_id, annotation)?;
    tx.commit()?;
    Ok(Stored::New(annotation))
}
