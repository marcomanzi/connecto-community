//! The bridge server: the socket, caller identification, and one short-lived
//! handler thread per connection (section 5.11).
//!
//! Identity comes only from the peer process tree. There is no token, and
//! `CONNECTO_TERMINAL` is never trusted. Handlers have their own deadlines, so a
//! stuck actor delays only its own caller.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::memory::MemoryCaller;
use connecto_core::{PaneControl, PaneStage, SaveState, SessionStatus, TerminalSession, ToolKind};
use connecto_gate::{Admitted, AgentAction, AgentRequest, Denial, TargetState};
use connecto_hook::bridge::{BadRequest, Op, Reply, key_bytes};
use connecto_hook::{MAX_FRAME, decode};
use crossbeam_channel::{RecvTimeoutError, Sender, bounded};
use serde_json::json;

use super::memory;
use super::state::{self, Admission, AdmitKind, Drained, Msg, Owed};
use super::{AgentCommand, Caller, HANDLER_WAIT, Report, WriteKind, WriteOutcome};
use crate::background;
use crate::local_socket::{self, Listener, Stream};
use crate::process_tree::parent_of;
use crate::projects::{BridgeLink, ProjectDirectory, Unresolved};
use crate::relay::{Shared, Slot};
use crate::terminal_list::clean_name;
use crate::terminals::{Admit, AgentLaunch, Registry, Removal, TermError, tool_of};
use connecto_store::Tool;

/// Handlers alive at once. Above this a connection gets `busy`.
const MAX_HANDLERS: usize = 32;
const READ_LIMIT: Duration = Duration::from_secs(1);
const WRITE_LIMIT: Duration = Duration::from_secs(5);
const STATE_WAIT: Duration = Duration::from_secs(10);
const PARENT_DEPTH: usize = 64;
/// A self-close stops its terminal this long after the reply is written.
const SELF_CLOSE_DELAY: Duration = Duration::from_millis(250);

/// A running server. Dropping it stops it and removes the socket.
pub(crate) struct Server {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    ctx: Weak<Ctx>,
}

impl Server {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The link the project directory keeps to this bridge.
    pub(crate) fn link(&self) -> BridgeLink {
        BridgeLink(Weak::clone(&self.ctx))
    }

    /// Let this bridge reach the projects `directory` knows (ADR 0035). One
    /// shared cell, read by the handlers and the bridge state alike.
    pub(crate) fn join(&self, directory: &Arc<ProjectDirectory>) {
        if let Some(ctx) = self.ctx.upgrade() {
            let _ = ctx.shared.directory.set(Arc::clone(directory));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the blocking accept, which then sees the flag.
        local_socket::wake(&self.path);
        local_socket::remove(&self.path);
    }
}

pub(crate) struct Ctx {
    pub(crate) shared: Arc<Shared>,
    registry: Registry,
    pub(crate) state: Sender<Msg>,
    live: AtomicUsize,
}

impl Ctx {
    /// The project is closing: it takes no more cross-project requests.
    pub(crate) fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::SeqCst)
    }

    /// This project's id, as the directory knows it.
    fn project_id(&self) -> String {
        self.shared
            .memory_project
            .get()
            .map(|(id, _)| id.clone())
            .unwrap_or_default()
    }
}

/// Counts a handler, on every way out.
struct HandlerGuard(Arc<Ctx>);

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Where this run's socket goes: `bridge-<app pid>.sock` in the checked private
/// folder (on Windows, a pipe with a random name; see
/// [`crate::platform::endpoint`]).
pub(crate) fn socket_path() -> io::Result<PathBuf> {
    // One relay per app run, so one socket. A second relay in one process (a
    // test) gets a number of its own.
    static RELAYS: AtomicUsize = AtomicUsize::new(0);
    let pid = std::process::id();
    let name = match RELAYS.fetch_add(1, Ordering::SeqCst) {
        0 => format!("bridge-{pid}.sock"),
        n => format!("bridge-{pid}-{n}.sock"),
    };
    crate::platform::endpoint("bridge", name)
}

/// Bind the socket. A stale file from a crashed run is replaced. Bound before
/// any terminal starts, so each knows whether it has a bridge. On Windows it is
/// a named pipe, and a name that is taken fails.
pub(crate) fn bind(path: &Path) -> io::Result<Listener> {
    local_socket::bind(path)
}

pub(crate) fn serve(
    listener: Listener,
    path: PathBuf,
    shared: Arc<Shared>,
    registry: Registry,
    state: Sender<Msg>,
) -> io::Result<Server> {
    let stop = Arc::new(AtomicBool::new(false));
    let ctx = Arc::new(Ctx {
        shared,
        registry,
        state,
        live: AtomicUsize::new(0),
    });
    let weak = Arc::downgrade(&ctx);
    let accept_stop = Arc::clone(&stop);
    thread::Builder::new()
        .name("bridge-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if accept_stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                if ctx.live.fetch_add(1, Ordering::SeqCst) >= MAX_HANDLERS {
                    ctx.live.fetch_sub(1, Ordering::SeqCst);
                    let mut stream = stream;
                    let _ = write_reply(
                        &mut stream,
                        &Reply::refused("busy", "Too many bridge connections at once.", None),
                    );
                    continue;
                }
                let guard = HandlerGuard(Arc::clone(&ctx));
                let handler_ctx = Arc::clone(&ctx);
                let spawned =
                    thread::Builder::new()
                        .name("bridge-handler".into())
                        .spawn(move || {
                            let _guard = guard;
                            handle(&handler_ctx, stream);
                        });
                if spawned.is_err() {
                    // The guard moved into the closure, which was dropped.
                }
            }
        })?;
    Ok(Server {
        path,
        stop,
        ctx: weak,
    })
}

fn write_reply(stream: &mut Stream, reply: &Reply) -> io::Result<()> {
    stream.set_write_timeout(Some(WRITE_LIMIT))?;
    reply.write_to(stream)
}

/// Read one request frame under one overall deadline.
fn read_frame(stream: &mut Stream) -> Result<Vec<u8>, Reply> {
    let deadline = Instant::now() + READ_LIMIT;
    let mut received = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        if let Some((header, rest)) = received.split_first_chunk::<4>() {
            let len = usize::try_from(u32::from_be_bytes(*header)).unwrap_or(usize::MAX);
            if len > MAX_FRAME {
                return Err(Reply::refused(
                    "too_large",
                    "The request is over the 256 KiB frame limit.",
                    None,
                ));
            }
            if let Some(body) = rest.get(..len) {
                return Ok(body.to_vec());
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Reply::refused(
                "bad_request",
                "The request did not arrive in time.",
                None,
            ));
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| Reply::refused("bad_request", "Could not read the request.", None))?;
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(Reply::refused(
                    "bad_request",
                    "The request was cut short.",
                    None,
                ));
            }
            Ok(count) => received.extend_from_slice(&chunk[..count]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                return Err(Reply::refused(
                    "bad_request",
                    "Could not read the request.",
                    None,
                ));
            }
        }
    }
}

/// The terminal whose live process the peer descends from, found by a parent
/// walk against the published root pids. The pair of id and attempt is read from
/// one live session, so a restart cannot mix them.
fn find_caller(shared: &Shared, peer: i32) -> Option<Caller> {
    let roots: Vec<(i32, Arc<Slot>, u64)> = shared
        .slots
        .load()
        .iter()
        .flatten()
        .filter_map(|slot| {
            let session = slot.pane.session()?;
            if session.status() != SessionStatus::Running {
                return None;
            }
            Some((session.pid()?, Arc::clone(slot), session.attempt()))
        })
        .collect();
    let mut current = peer;
    for _ in 0..PARENT_DEPTH {
        if let Some((_, slot, attempt)) = roots.iter().find(|(pid, _, _)| *pid == current) {
            return Some(Caller {
                slot: slot.index,
                uid: slot.uid,
                id: slot.id.clone(),
                attempt: *attempt,
                origin: None,
            });
        }
        if current <= 1 {
            return None;
        }
        current = parent_of(current)?;
    }
    None
}

fn denial_reply(denial: Denial, id: Option<&str>) -> Reply {
    let (code, reason) = match denial {
        Denial::Paused => (
            "paused",
            "Connecto is paused by the user. Nothing was typed.",
        ),
        Denial::StaleTarget => (
            "restarted",
            "The target restarted after this was accepted. Nothing was typed.",
        ),
        Denial::NotRunning => ("not_running", "The target has no process to type into."),
        Denial::ManualMode => (
            "bridge_off",
            "The target runs a version the bridge does not support.",
        ),
        Denial::AuditFailed => ("audit_failed", "The audit log failed, so nothing happened."),
        _ => ("bad_request", "The request was refused."),
    };
    Reply::refused(code, reason, id)
}

fn term_error_reply(error: &TermError) -> Reply {
    let code = match error {
        TermError::CapReached => "cap_reached",
        TermError::SaveFailed(_) => "save_failed",
        TermError::Unknown => "unknown_terminal",
        TermError::AlreadyRunning => "already_running",
        TermError::Busy => "busy",
        TermError::Paused => "paused",
        TermError::LoggingIn => "logging_in",
        TermError::Shutdown => "not_running",
        TermError::Timeout => "timeout",
        TermError::Restarted => "restarted",
        TermError::Name(_) => "bad_request",
        TermError::NotAllowed(_) => "not_allowed",
    };
    Reply::refused(code, &error.to_string(), None)
}

fn handle(ctx: &Arc<Ctx>, mut stream: Stream) {
    let reply_to = |stream: &mut Stream, reply: &Reply| {
        let _ = write_reply(stream, reply);
    };
    let Ok(peer) = local_socket::peer_pid(&stream) else {
        return;
    };
    let body = match read_frame(&mut stream) {
        Ok(body) => body,
        Err(reply) => return reply_to(&mut stream, &reply),
    };
    let Some(frame) = decode(&body) else {
        return reply_to(
            &mut stream,
            &Reply::refused("bad_request", "The frame is malformed.", None),
        );
    };
    let op = match Op::parse(frame.payload) {
        Ok(op) => op,
        Err(BadRequest::Version) => {
            return reply_to(
                &mut stream,
                &Reply::refused(
                    "bad_version",
                    "This command and Connecto speak different versions.",
                    None,
                ),
            );
        }
        Err(BadRequest::Malformed(why)) => {
            return reply_to(&mut stream, &Reply::refused("bad_request", &why, None));
        }
    };
    let Some(caller) = find_caller(&ctx.shared, peer) else {
        return reply_to(
            &mut stream,
            &Reply::refused(
                "not_a_terminal",
                "This command works only inside a Connecto terminal.",
                None,
            ),
        );
    };
    // A bridge-off terminal gets no operation: the reason says why. The drain
    // is the hook's own call and just prints nothing.
    let caller_off = ctx
        .shared
        .slot(caller.slot)
        .and_then(|slot| slot.pane.bridge_off_reason());
    if let Some(reason) = caller_off {
        let reply = if matches!(op, Op::Drain { .. }) {
            Reply::ok(json!({}))
        } else {
            Reply::refused(
                "bridge_off",
                &format!("The bridge is off for this terminal: {reason}"),
                None,
            )
        };
        return reply_to(&mut stream, &reply);
    }
    if let Op::Drain { prompt, holds, .. } = op {
        return drain(ctx, &caller, prompt.as_deref(), holds, &mut stream);
    }
    let reply = dispatch(ctx, &caller, op);
    reply_to(&mut stream, &reply);
}

fn ask_state<T>(ctx: &Ctx, make: impl FnOnce(Sender<T>) -> Msg) -> Result<T, Reply> {
    let (reply, answer) = bounded(1);
    ctx.state
        .try_send(make(reply))
        .map_err(|_| Reply::refused("busy", "Bridge state is busy.", None))?;
    answer.recv_timeout(STATE_WAIT).map_err(|err| match err {
        RecvTimeoutError::Timeout => {
            Reply::refused("timeout", "Bridge state did not answer in time.", None)
        }
        RecvTimeoutError::Disconnected => {
            Reply::refused("not_running", "Connecto is closing.", None)
        }
    })
}

fn paused_reply() -> Reply {
    Reply::refused(
        "paused",
        "Connecto is paused by the user. Nothing was done.",
        None,
    )
}

/// The target a writing operation names, with the refusals that need no state.
fn writable_target(ctx: &Ctx, target: &str) -> Result<Arc<Slot>, Reply> {
    let slot = ctx.shared.slot_by_id(target).ok_or_else(unknown_terminal)?;
    if slot.pane.stage() == PaneStage::LoggingIn {
        return Err(Reply::refused(
            "logging_in",
            "That terminal is running a login.",
            None,
        ));
    }
    if let Some(reason) = slot.pane.bridge_off_reason() {
        return Err(Reply::refused(
            "bridge_off",
            &format!("The bridge is off for that terminal: {reason}"),
            None,
        ));
    }
    if !slot.pane.is_live() {
        return Err(Reply::refused(
            "not_running",
            "That terminal has no process to type into. Use start.",
            None,
        ));
    }
    Ok(slot)
}

/// The refusal for using a terminal that belongs to another agent. A terminal
/// an agent started with `connecto add` belongs to that agent: only it, and
/// the other terminals it started, may type into the terminal, press keys
/// there, start it or close it. A terminal the user opened belongs to nobody,
/// and a child may always write to the agent that started it. Reading and
/// watching are open to all.
fn not_yours(ctx: &Ctx, caller: &Caller, target: &str) -> Option<Reply> {
    if caller.origin.is_some() {
        return not_open_to_all(ctx, target);
    }
    if caller.id == target {
        return None;
    }
    let view = ctx.registry.view_arc().load_full();
    let owner_of = |id: &str| {
        view.terminals
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.owner().map(str::to_owned))
    };
    let owner = owner_of(target)?;
    let callers_owner = owner_of(&caller.id);
    // Its starter, the starter itself, or a sibling the same agent started.
    if owner == caller.id
        || callers_owner.as_deref() == Some(target)
        || callers_owner.as_deref() == Some(owner.as_str())
    {
        return None;
    }
    Some(Reply::refused(
        "not_yours",
        &format!(
            "{target} was started by {owner} and belongs to it, so nothing was done. \
             Start your own terminal with `connecto add`."
        ),
        None,
    ))
}

fn dispatch(ctx: &Arc<Ctx>, caller: &Caller, op: Op) -> Reply {
    match op {
        Op::List => with_context(
            ctx,
            ask_state(ctx, |reply| Msg::List {
                caller: caller.clone(),
                reply,
            })
            .unwrap_or_else(|r| r),
            false,
        ),
        Op::Result { request } => ask_state(ctx, |reply| Msg::Result {
            caller: caller.clone(),
            request,
            reply,
        })
        .unwrap_or_else(|r| r),
        Op::Watch { target } => watch_op(ctx, caller, target, None),
        Op::Unwatch { target } => ask_state(ctx, |reply| Msg::Watch {
            caller: caller.clone(),
            target,
            on: false,
            still: None,
            reply,
        })
        .unwrap_or_else(|r| r),
        Op::Read { target } => read(ctx, caller, &target),
        Op::Send { target, ask, text } => send(ctx, caller, &target, ask, &text),
        Op::Keys { target, keys } => keys_op(ctx, caller, &target, &keys),
        Op::Add {
            tool,
            name,
            model,
            effort,
        } => add(ctx, caller, &tool, name, AgentLaunch { model, effort }),
        Op::Start { target } => process(ctx, caller, &target, AgentAction::Start),
        Op::Close { target } => process(ctx, caller, &target, AgentAction::Close),
        Op::Tasks { target, stop, task } => tasks_op(ctx, caller, &target, stop, task.as_deref()),
        Op::Drain { .. } => Reply::refused("bad_request", "drain is for the hook helper.", None),
        Op::Memory { verb, args, text } => memory_op(ctx, caller, &verb, &args, text.as_deref()),
        Op::Ops { verb, args, text } => ops_op(ctx, caller, &verb, &args, text.as_deref()),
        Op::Secret { verb, args, cwd } => secret_op(ctx, caller, &verb, &args, cwd.as_deref()),
        Op::SendFile { path } => send_file_op(ctx, caller, &path),
        Op::Works { verb, args, text } => works_op(ctx, caller, &verb, &args, text.as_deref()),
        Op::Projects => projects_op(ctx),
        Op::Agents { roles } => agents_op(ctx, caller, roles),
        Op::InProject { project, op } => in_project(ctx, caller, &project, *op),
    }
}

/// Each listed agent terminal's context use, read from its footer
/// (`context.rs`). `null` when it shows no figure, for a tool without a known
/// footer, and while its screen is busy: the screen is never waited on, so
/// one terminal in a redraw cannot hold up the list. `across` is a caller
/// from another project, who gets no figure from a terminal it may not
/// `read`, and whose screen is not even looked at.
fn with_context(ctx: &Ctx, mut reply: Reply, across: bool) -> Reply {
    let Some(terminals) = reply
        .header
        .get_mut("terminals")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return reply;
    };
    for terminal in terminals {
        let used = terminal
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !across || not_open_to_all(ctx, id).is_none())
            .and_then(|id| ctx.shared.slot_by_id(id))
            .filter(|slot| super::context::has_footer(slot.tool))
            .and_then(|slot| {
                let session = slot.pane.session()?;
                // A login's screen is not read, as `read` refuses it.
                session.hook()?;
                super::context::used_percent(slot.tool, &session.try_text_screen()?)
            });
        terminal["context"] = json!(used);
    }
    reply
}

/// `connecto agents [--roles]`: reading only, so it is allowed under Pause.
/// The second opinion avoids the caller's own CLI.
fn agents_op(ctx: &Ctx, caller: &Caller, roles: bool) -> Reply {
    let Some(who) = ops_caller(ctx, caller) else {
        return Reply::refused(
            "no_project",
            "This terminal has no project folder, so it has no agents to start.",
            None,
        );
    };
    // The id `connecto add` checks the Tools setting under.
    super::roster::command(
        &ctx.shared.layout.data_root,
        &ctx.project_id(),
        &who.folder,
        who.tool,
        roles,
    )
}

fn watch_op(
    ctx: &Arc<Ctx>,
    caller: &Caller,
    target: String,
    still: Option<state::StillGuard>,
) -> Reply {
    if let Some(slot) = ctx.shared.slot_by_id(&target)
        && slot.tool == Tool::Shell
    {
        return no_turns(&slot.id, "watch");
    }
    if let Some(slot) = ctx.shared.slot_by_id(&target)
        && let Some(reason) = slot.pane.bridge_off_reason()
    {
        return Reply::refused(
            "bridge_off",
            &format!("That terminal would never report events: {reason}"),
            None,
        );
    }
    ask_state(ctx, |reply| Msg::Watch {
        caller: caller.clone(),
        target,
        on: true,
        still,
        reply,
    })
    .unwrap_or_else(|r| r)
}

// ---- other projects (ADR 0035)

/// Across projects only terminals open to everyone may be used: not one an
/// agent started for its own work. Decided on the registry's own entry, which
/// carries the owner from the start: a terminal it does not list yet (a slot
/// installed before the list was published) is refused too.
fn not_open_to_all(ctx: &Ctx, target: &str) -> Option<Reply> {
    let view = ctx.registry.view_arc().load_full();
    let Some(entry) = view.terminals.iter().find(|entry| entry.id == target) else {
        return Some(unknown_terminal());
    };
    let owner = entry.owner()?;
    Some(Reply::refused(
        "not_yours",
        &format!(
            "{target} was started by {owner} for its own work, so nothing was done. Across \
             projects only terminals open to everyone can be used."
        ),
        None,
    ))
}

/// The terminals of an open project, for `connecto projects`.
fn terminals_json(ctx: &Ctx) -> Vec<serde_json::Value> {
    let view = ctx.registry.view_arc().load_full();
    view.terminals
        .iter()
        .filter_map(|entry| {
            let slot = ctx.shared.slot_by_id(&entry.id)?;
            Some(json!({
                "id": entry.id,
                "tool": entry.tool.name(),
                "state": state::state_word(&slot),
                "owner": entry.owner(),
            }))
        })
        .collect()
}

/// How many terminals a closed project's saved list holds.
fn saved_count(ctx: &Ctx, id: &str) -> Option<usize> {
    let path = ctx
        .shared
        .layout
        .data_root
        .join("projects")
        .join(id)
        .join("terminals.json");
    let text = std::fs::read_to_string(path).ok()?;
    crate::terminal_list::List::parse(&text)
        .ok()
        .map(|list| list.terminals.len())
}

/// `connecto projects`: what the directory knows, with each open project's
/// terminals. Reading only, so it is allowed under Pause.
fn projects_op(ctx: &Ctx) -> Reply {
    let own = ctx.project_id();
    let mut entries = ctx
        .shared
        .directory
        .get()
        .map(|directory| directory.entries())
        .unwrap_or_default();
    if !entries.iter().any(|entry| entry.id == own)
        && let Some((id, path)) = ctx.shared.memory_project.get()
    {
        entries.insert(
            0,
            crate::projects::ProjectEntry {
                id: id.clone(),
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path: path.clone(),
                link: None,
            },
        );
    }
    let projects: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            let this = entry.id == own;
            let open = if this {
                Some(())
            } else {
                entry.ctx().map(|_| ())
            };
            let terminals = if this {
                terminals_json(ctx)
            } else {
                entry
                    .ctx()
                    .map(|other| terminals_json(&other))
                    .unwrap_or_default()
            };
            let mut value = json!({
                "name": entry.name,
                "path": entry.path.display().to_string(),
                "open": open.is_some(),
                "this": this,
                "terminals": terminals,
            });
            if open.is_none() {
                value["count"] = json!(saved_count(ctx, &entry.id));
            }
            value
        })
        .collect();
    Reply::ok(json!({ "projects": projects }))
}

/// The verb of an op as the audit names it, and whether it may cross
/// projects.
const fn across_verb(op: &Op) -> (&'static str, bool) {
    match op {
        Op::List => ("list", true),
        Op::Read { .. } => ("read", true),
        Op::Send { .. } => ("send", true),
        Op::Result { .. } => ("result", true),
        Op::Watch { .. } => ("watch", true),
        Op::Unwatch { .. } => ("unwatch", true),
        Op::Keys { .. } => ("keys", false),
        Op::Add { .. } => ("add", false),
        Op::Start { .. } => ("start", false),
        Op::Close { .. } => ("close", false),
        Op::Tasks { .. } => ("tasks", false),
        Op::Memory { .. } => ("memory", false),
        Op::Ops { .. } => ("op", false),
        Op::Secret { .. } => ("secret", false),
        Op::SendFile { .. } => ("send_file", false),
        Op::Works { .. } => ("works", false),
        Op::Drain { .. } => ("drain", false),
        Op::Projects => ("projects", false),
        Op::Agents { .. } => ("agents", false),
        Op::InProject { .. } => ("in_project", false),
    }
}

/// The terminal or request an op names, for the audit.
fn named_in(op: &Op) -> String {
    match op {
        Op::Read { target }
        | Op::Send { target, .. }
        | Op::Watch { target }
        | Op::Unwatch { target }
        | Op::Keys { target, .. }
        | Op::Start { target }
        | Op::Close { target }
        | Op::Tasks { target, .. } => target.clone(),
        Op::Result { request } => request.clone(),
        _ => String::new(),
    }
}

/// The audit of one cross-project op, in both projects' records: the
/// caller's names where it went, the target's names who came.
struct CrossAudit<'a> {
    home: &'a Ctx,
    target: Option<&'a Ctx>,
    target_project: &'a str,
    /// The target project's id when the directory knows it, open or not: a
    /// closed one's record is written by this project's gate.
    target_id: Option<&'a str>,
    caller: &'a Caller,
    foreign_source: String,
    named: String,
    action: String,
}

impl CrossAudit<'_> {
    fn note(&self, decision: &str, reason: &str) {
        self.home.shared.gate().agent_cross_note(
            &self.caller.id,
            &format!("{}/{}", self.target_project, self.named),
            &self.action,
            decision,
            reason,
        );
        if let Some(target) = self.target {
            target.shared.gate().agent_cross_note(
                &self.foreign_source,
                &self.named,
                &self.action,
                decision,
                reason,
            );
        }
    }

    /// Record a decision in both projects, durably. False when either log
    /// did not take it in time.
    fn record(&self, decision: &str, reason: &str) -> bool {
        let home = self.home.shared.gate().agent_cross_record(
            &self.caller.id,
            &format!("{}/{}", self.target_project, self.named),
            &self.action,
            decision,
            reason,
        );
        let target = match (self.target, self.target_id) {
            (Some(target), _) => target.shared.gate().agent_cross_record(
                &self.foreign_source,
                &self.named,
                &self.action,
                decision,
                reason,
            ),
            (None, Some(id)) => self.home.shared.gate().agent_cross_record_for(
                id,
                &self.foreign_source,
                &self.named,
                &self.action,
                decision,
                reason,
            ),
            (None, None) => true,
        };
        home && target
    }

    /// Record a refusal in both projects, durably, and give it back. A
    /// refusal the logs could not take says so: it is never silently lost.
    fn deny(&self, reply: Reply) -> Reply {
        let text = |field: &str| {
            reply
                .header
                .get(field)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let code = text("code");
        if self.record("deny", &code) {
            return reply;
        }
        audit_failed_reply(reply)
    }

    /// The intent, durable in both projects before anything is done: if
    /// either log cannot take it, or Pause is on in either, nothing happens.
    /// Gives each project's Pause epoch, checked again right before acting.
    fn admit<'b>(&'b self, target: &'b Ctx) -> Result<Still<'b>, Reply> {
        let target_project = format!("{}/{}", self.target_project, self.named);
        let home = self.home.shared.gate().agent_cross_intent(
            &self.caller.id,
            &target_project,
            &self.action,
        );
        let both = home.and_then(|home| {
            target
                .shared
                .gate()
                .agent_cross_intent(&self.foreign_source, &self.named, &self.action)
                .map(|there| (home, there))
        });
        match both {
            Ok((home, there)) => Ok(Still {
                home: self.home,
                target,
                home_epoch: home,
                target_epoch: there,
            }),
            Err(denial) => Err(self.deny(denial_reply(denial, None))),
        }
    }
}

/// What an admitted cross-project op checks right before it acts: Pause has
/// stayed off, in the epochs it was admitted in, in both projects.
struct Still<'a> {
    home: &'a Ctx,
    target: &'a Ctx,
    home_epoch: u64,
    target_epoch: u64,
}

impl Still<'_> {
    fn ok(&self) -> bool {
        self.home.shared.gate().still_in(self.home_epoch)
            && self.target.shared.gate().still_in(self.target_epoch)
    }

    /// [`Still::ok`] for the target's state to ask, on its own thread.
    fn guard(&self) -> state::StillGuard {
        let (home, target) = (
            Arc::clone(self.home.shared.gate()),
            Arc::clone(self.target.shared.gate()),
        );
        let (home_epoch, target_epoch) = (self.home_epoch, self.target_epoch);
        Box::new(move || home.still_in(home_epoch) && target.still_in(target_epoch))
    }

    /// As [`Still::ok`], for a send the target's state admitted in `epoch`.
    fn ok_in(&self, epoch: u64) -> bool {
        self.ok() && epoch == self.target_epoch
    }
}

/// An op with `--project`: done in that project, by a caller that stays a
/// stranger there. Only talking and reading cross: list, read, send, result,
/// watch and unwatch. Pause in either project refuses them all, and each one,
/// allowed or refused, is recorded in both projects; an allowed one only once
/// both records are durable.
fn in_project(ctx: &Arc<Ctx>, caller: &Caller, query: &str, op: Op) -> Reply {
    let (verb, allowed) = across_verb(&op);
    let resolved = ctx
        .shared
        .directory
        .get()
        .map(|directory| directory.resolve(query));
    let entry = match resolved {
        Some(Ok(entry)) => Some(entry),
        Some(Err(Unresolved::Ambiguous(paths))) => {
            return Reply::refused(
                "ambiguous_project",
                &format!(
                    "Several projects have that name; give the path instead: {}",
                    paths.join(", ")
                ),
                None,
            );
        }
        _ => None,
    };
    let own = ctx.project_id();
    if let Some(entry) = &entry
        && entry.id == own
    {
        if !allowed {
            return not_across();
        }
        // Its own project: exactly as without the flag.
        return dispatch(ctx, caller, op);
    }
    let target = entry.as_ref().and_then(crate::projects::ProjectEntry::ctx);
    let own_name = ctx
        .shared
        .directory
        .get()
        .and_then(|directory| directory.by_id(&own))
        .map_or_else(|| own.clone(), |own| own.name);
    let foreign = Caller {
        origin: Some(Arc::new(super::Origin {
            project_id: own.clone(),
            project_name: own_name,
            instance: ctx.shared.bridge_instance,
        })),
        ..caller.clone()
    };
    let audit = CrossAudit {
        home: ctx,
        target: target.as_deref(),
        target_project: entry.as_ref().map_or(query, |entry| entry.id.as_str()),
        target_id: entry.as_ref().map(|entry| entry.id.as_str()),
        caller,
        foreign_source: foreign.audit_source(),
        named: named_in(&op),
        action: format!("cross_project_{verb}"),
    };
    let Some(entry) = entry.as_ref() else {
        return audit.deny(Reply::refused(
            "unknown_project",
            "No project has that name or path. `connecto projects` lists them.",
            None,
        ));
    };
    if !allowed {
        return audit.deny(not_across());
    }
    let Some(target) = target.as_ref() else {
        return audit.deny(Reply::refused(
            "project_closed",
            &format!(
                "Project {} is not open in Connecto, so nothing was done. Only the user can open it.",
                entry.name
            ),
            None,
        ));
    };
    if ctx.shared.pause().is_paused() || target.shared.pause().is_paused() {
        return audit.deny(paused_reply());
    }
    let still = match audit.admit(target) {
        Ok(still) => still,
        Err(reply) => return reply,
    };
    let mut reply = across(target, &foreign, op, &still);
    if reply.is_ok() {
        reply.header["project"] = json!(entry.name);
        audit.note("outcome", "");
    } else {
        reply = audit.deny(reply);
    }
    // Until the answer comes, the asker's own turns hold the asks it was
    // given, as for an ask in its own project. A debt, so a full queue
    // cannot lose it, owed before the asker hears the ask went out.
    if reply.is_ok()
        && verb == "send"
        && reply.header.get("ask").is_some()
        && let Some(request) = reply.header.get("id").and_then(serde_json::Value::as_str)
    {
        ctx.shared.owe(Owed::ForeignAsk(state::ForeignAsk {
            att: state::Att::from(caller),
            project_id: entry.id.clone(),
            project_instance: target.shared.bridge_instance,
            project_name: entry.name.clone(),
            request: request.to_owned(),
        }));
    }
    reply
}

/// A refusal the audit log could not take: `audit_failed`, with everything
/// the refusal said kept, its code as `refused_code`. A send's refusal may
/// carry its request id and say text was pasted or delivery is uncertain;
/// all of that stays, so the caller can recover without sending twice.
fn audit_failed_reply(mut reply: Reply) -> Reply {
    let text = |field: &str| {
        reply
            .header
            .get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (code, reason) = (text("code"), text("reason"));
    reply.header["code"] = json!("audit_failed");
    reply.header["refused_code"] = json!(code);
    reply.header["reason"] = json!(format!(
        "{reason} (Refused as {code}; that refusal could not be written to the audit log.)"
    ));
    reply
}

fn not_across() -> Reply {
    Reply::refused(
        "not_across_projects",
        "Across projects only list, read, send, result, watch and unwatch work, so nothing was \
         done.",
        None,
    )
}

/// The op itself, in the target project's bridge, for a caller from another.
/// Pause is checked again right before each acts, and before anything read
/// is handed back.
fn across(target: &Arc<Ctx>, foreign: &Caller, op: Op, still: &Still<'_>) -> Reply {
    if !still.ok() {
        return paused_reply();
    }
    let checked = |reply: Reply| {
        if reply.is_ok() && !still.ok() {
            paused_reply()
        } else {
            reply
        }
    };
    match op {
        Op::List => checked(with_context(
            target,
            ask_state(target, |reply| Msg::List {
                caller: foreign.clone(),
                reply,
            })
            .unwrap_or_else(|r| r),
            true,
        )),
        Op::Read { target: id } => match not_open_to_all(target, &id) {
            Some(reply) => reply,
            None => checked(read(target, foreign, &id)),
        },
        Op::Send {
            target: id,
            ask,
            text,
        } => {
            if let Some(reply) = not_open_to_all(target, &id) {
                return reply;
            }
            if let Some(slot) = target.shared.slot_by_id(&id)
                && slot.tool == Tool::Shell
            {
                return Reply::refused(
                    "not_across_projects",
                    &format!(
                        "{id} is a shell. Across projects a shell cannot be typed into, so nothing was done."
                    ),
                    None,
                );
            }
            let check = |epoch| still.ok_in(epoch);
            let mut reply = send_checked(target, foreign, &id, ask, &text, Some(&check));
            if ask && reply.is_ok() {
                reply.header["ask"] = json!(true);
            }
            reply
        }
        Op::Result { request } => checked(
            ask_state(target, |reply| Msg::Result {
                caller: foreign.clone(),
                request,
                reply,
            })
            .unwrap_or_else(|r| r),
        ),
        Op::Watch { target: id } => {
            if let Some(reply) = not_open_to_all(target, &id) {
                return reply;
            }
            // Pause is checked by the state right before the watch is set,
            // so a Pause while the op was queued neither sets it nor takes
            // back one set before.
            watch_op(target, foreign, id, Some(still.guard()))
        }
        Op::Unwatch { target: id } => match not_open_to_all(target, &id) {
            Some(reply) => reply,
            None => ask_state(target, |reply| Msg::Watch {
                caller: foreign.clone(),
                target: id,
                on: false,
                still: Some(still.guard()),
                reply,
            })
            .unwrap_or_else(|r| r),
        },
        _ => not_across(),
    }
}

/// `connecto works`: the caller's own project only, and how its agent was
/// launched, both from the process tree and the launch, never from what the
/// terminal says. Reading is allowed under Pause; changing anything is not.
fn works_op(
    ctx: &Arc<Ctx>,
    caller: &Caller,
    verb: &str,
    args: &[String],
    text: Option<&str>,
) -> Reply {
    // The mode of the very session the caller was found in: a restart in
    // another mode is another attempt, and gives nothing to this one.
    // The slot as the caller was found in it, not a later holder.
    let slot = ctx
        .shared
        .slot(caller.slot)
        .filter(|slot| slot.uid == caller.uid && slot.id == caller.id);
    let job = slot
        .as_ref()
        .is_some_and(|slot| slot.job.load(Ordering::SeqCst));
    let launched = slot
        .and_then(|slot| slot.pane.session())
        .filter(|session| session.attempt() == caller.attempt)
        .and_then(|session| session.agent_mode());
    let who =
        memory::project_of_key(ctx.shared.project_key.load(Ordering::SeqCst)).map(|project_id| {
            connecto_core::agentic_works::WorksCaller {
                terminal: caller.id.clone(),
                launched,
                job,
                project_id,
                attempt: caller.attempt,
            }
        });
    let state = ctx.state.clone();
    let asker = caller.clone();
    let notify: connecto_core::AskReply = Box::new(move |text| {
        let _ = state.send(Msg::Notice {
            caller: asker,
            text,
        });
    });
    super::works::command(
        &*super::works::works_bridge(),
        who.as_ref(),
        ctx.shared.pause().is_paused(),
        verb,
        args,
        text,
        notify,
    )
}

/// `connecto secret`. The caller is the bridge's, from the process tree:
/// its terminal, its project and folder, and which processes are its own
/// (an ssh-agent socket serves only those). Listing is allowed under Pause;
/// asking for or using a secret is not.
fn secret_op(
    ctx: &Arc<Ctx>,
    caller: &Caller,
    verb: &str,
    args: &[String],
    cwd: Option<&str>,
) -> Reply {
    if verb != "list" && verb != "help" && ctx.shared.pause().is_paused() {
        return paused_reply();
    }
    let project = memory::project_of_key(ctx.shared.project_key.load(Ordering::SeqCst));
    let folder = ctx
        .shared
        .slot(caller.slot)
        .and_then(|slot| slot.pane.project_path());
    let shared = Arc::clone(&ctx.shared);
    let (id, attempt) = (caller.id.clone(), caller.attempt);
    let who = connecto_core::SecretCaller {
        terminal: caller.id.clone(),
        project,
        project_folder: folder,
        owns_pid: Arc::new(move |pid| {
            find_caller(&shared, pid)
                .is_some_and(|found| found.id == id && found.attempt == attempt)
        }),
    };
    let state = ctx.state.clone();
    let asker = caller.clone();
    let notify: connecto_core::AskReply = Box::new(move |text| {
        let _ = state.send(Msg::Notice {
            caller: asker,
            text,
        });
    });
    super::vault::command(
        &*super::vault::vault_bridge(),
        &who,
        verb,
        args,
        cwd.map(Path::new),
        notify,
    )
}

/// `connecto memory`: allowed under Pause (it types into no terminal). The
/// caller is the bridge's: its terminal, the tool's session id that terminal
/// reported, and the branch checked out in the project.
fn memory_op(ctx: &Ctx, caller: &Caller, verb: &str, args: &[String], text: Option<&str>) -> Reply {
    let project = memory::project_of_key(ctx.shared.project_key.load(Ordering::SeqCst));
    let slot = ctx.shared.slot(caller.slot);
    let who = MemoryCaller {
        terminal: caller.id.clone(),
        tool_session: slot.as_ref().and_then(|slot| slot.tool_session()),
        branch: slot
            .as_ref()
            .and_then(|slot| slot.pane.project_path())
            .and_then(|path| memory::branch_of(&path)),
    };
    memory::command(
        &*memory::memory_bridge(),
        project.as_deref(),
        &who,
        verb,
        args,
        text,
    )
}

/// Who runs `connecto op`, from the process tree: `None` without a project.
fn ops_caller(ctx: &Ctx, caller: &Caller) -> Option<connecto_core::ops::OpsCaller> {
    let project_id = memory::project_of_key(ctx.shared.project_key.load(Ordering::SeqCst))?;
    let slot = ctx.shared.slot(caller.slot)?;
    Some(connecto_core::ops::OpsCaller {
        terminal: caller.id.clone(),
        tool: crate::terminals::kind(slot.tool),
        project_id,
        folder: slot.pane.project_path()?,
        holds: false,
    })
}

/// `connecto op`. Reading is allowed under Pause; running is not.
fn ops_op(ctx: &Ctx, caller: &Caller, verb: &str, args: &[String], text: Option<&str>) -> Reply {
    let who = ops_caller(ctx, caller);
    super::ops::command(
        &*super::ops::ops_bridge(),
        who.as_ref(),
        ctx.shared.pause().is_paused(),
        verb,
        args,
        text,
    )
}

fn read(ctx: &Ctx, caller: &Caller, target: &str) -> Reply {
    let Some(slot) = ctx.shared.slot_by_id(target) else {
        return unknown_terminal();
    };
    // Decided on the session itself, not on a stage: a login starting at this
    // moment cannot slip through a stage check.
    let Some(session) = slot.pane.session() else {
        // A start that was refused says why: an agent that opened the
        // terminal has no window to read it in.
        if let connecto_core::PaneStage::Failed { reason } = slot.pane.stage() {
            let reason = connecto_redact::sanitize(&reason);
            return Reply::refused(
                "no_content",
                &format!("That terminal did not start: {reason}"),
                None,
            );
        }
        return Reply::refused(
            "no_content",
            "That terminal has never started in this run.",
            None,
        );
    };
    if session.hook().is_none() {
        return Reply::refused(
            "logging_in",
            "That terminal is running a login, and its screen can show a code.",
            None,
        );
    }
    let (text, history_full) = session.text_all();
    // The screen as it is, but with the vault's values hidden: whatever a
    // terminal printed, another terminal never reads a secret here.
    let text = connecto_redact::hide_known(&text);
    ctx.shared
        .gate()
        .agent_note(&caller.audit_source(), &slot.id, slot.pane.attempt());
    Reply::ok(json!({ "history_full": history_full })).with_body(text.into_bytes())
}

fn send(ctx: &Arc<Ctx>, caller: &Caller, target: &str, ask: bool, text: &str) -> Reply {
    send_checked(ctx, caller, target, ask, text, None)
}

/// A send, with a check of the Pause epoch the target's state admitted it
/// in, made before anything is queued: false refuses it as paused.
fn send_checked(
    ctx: &Arc<Ctx>,
    caller: &Caller,
    target: &str,
    ask: bool,
    text: &str,
    check: Option<&dyn Fn(u64) -> bool>,
) -> Reply {
    // The handler's own clock: the send's deadline runs from here, through
    // every queue it waits in.
    let enqueued = Instant::now();
    if ctx.shared.pause().is_paused() {
        return paused_reply();
    }
    let slot = match writable_target(ctx, target) {
        Ok(slot) => slot,
        Err(reply) => return reply,
    };
    if let Some(reply) = not_yours(ctx, caller, &slot.id) {
        return reply;
    }
    let shell = slot.tool == Tool::Shell;
    if shell && ask {
        return no_turns(&slot.id, "send --ask");
    }
    let clean = connecto_redact::sanitize(text);
    if clean.trim().is_empty() {
        return Reply::refused("bad_request", "The text is empty.", None);
    }
    if shell && let Err(why) = shell_line(text) {
        return Reply::refused("bad_request", &why, None);
    }
    let admission = match ask_state(ctx, |reply| Msg::Admit {
        caller: caller.clone(),
        target: slot.id.clone(),
        kind: AdmitKind::Send { ask },
        reply,
    }) {
        Ok(Ok(admission)) => admission,
        Ok(Err(reply)) | Err(reply) => return reply,
    };
    let Admission { id, slot, admitted } = admission;
    if let Some(check) = check
        && !check(admitted.pause_epoch)
    {
        forget(ctx, &slot, &id, WriteKind::Send);
        return paused_reply();
    }
    if ask {
        // The first ask a child gets names it, unless it has a name.
        ctx.registry.auto_name(&slot.id, &clean);
    }
    // An agent is told who is writing; a shell is only typed into, so it gets
    // the command line and nothing else.
    let typed = if shell {
        clean.trim_end_matches(['\n', '\r']).to_owned()
    } else {
        format!(
            "[connecto from {}, request {id}. Sent by another agent, not the user.]\n{clean}",
            caller.label()
        )
    };
    let (reply, outcome) = bounded(1);
    let command = AgentCommand::Type {
        id: id.clone(),
        source: caller.label(),
        text: typed,
        admitted,
        reply,
        enqueued,
    };
    if slot.send_agent(command).is_err() {
        forget(ctx, &slot, &id, WriteKind::Send);
        return Reply::refused("queue_full", "That terminal's write queue is full.", None);
    }
    match outcome.recv_timeout(HANDLER_WAIT) {
        Ok(WriteOutcome::Typed) => Reply::ok(json!({ "id": id, "state": "typed" })),
        Ok(WriteOutcome::Refused(denial)) => denial_reply(denial, None),
        Ok(WriteOutcome::PausedBeforeEnter) => Reply::refused(
            "paused_before_enter",
            "Connecto was paused after the text was pasted. It may be sitting in the target's input box, and Enter was not pressed.",
            Some(&id),
        ),
        Ok(WriteOutcome::Restarted) => Reply::refused(
            "restarted",
            "The target restarted after the text was pasted. Nothing more was typed.",
            Some(&id),
        ),
        Ok(WriteOutcome::NotRunning { pasted: true }) => Reply::refused(
            "not_running",
            "The target ended after the text was pasted.",
            Some(&id),
        ),
        Ok(WriteOutcome::Failed { pasted: true, why }) => {
            Reply::refused("not_running", &why, Some(&id))
        }
        Ok(WriteOutcome::NotRunning { pasted: false } | WriteOutcome::Busy) => Reply::refused(
            "not_running",
            "The target has no process to type into.",
            None,
        ),
        Ok(WriteOutcome::Failed { pasted: false, why }) => {
            Reply::refused("not_running", &why, None)
        }
        Ok(WriteOutcome::NotReady { why }) => Reply::refused("not_ready", &why, None),
        Ok(WriteOutcome::Waiting { note }) => {
            // The user is typing in the target: the request is out, and its
            // outcome reaches bridge state (and an `--ask` sender) later.
            ctx.shared.owe(Owed::Waiting { id: id.clone() });
            Reply::ok(json!({ "id": id, "state": "waiting", "note": note }))
        }
        Err(RecvTimeoutError::Timeout) => {
            ctx.shared.owe(Owed::TimedOut { id: id.clone() });
            Reply::refused(
                "timeout",
                "No outcome in 30 s. The text may or may not have been typed, and it is not retried.",
                Some(&id),
            )
        }
        Err(RecvTimeoutError::Disconnected) => {
            Reply::refused("not_running", "The target's session ended.", None)
        }
    }
}

/// Drop a request that never reached an actor, with no id given out.
fn forget(ctx: &Ctx, slot: &Slot, id: &str, kind: WriteKind) {
    ctx.shared.report_for(
        slot,
        Report::Typing {
            id: id.to_owned(),
            kind,
            outcome: WriteOutcome::Failed {
                pasted: false,
                why: "not queued".to_owned(),
            },
        },
    );
}

/// What a shell terminal answers to a command that needs a turn to end.
fn no_turns(id: &str, what: &str) -> Reply {
    Reply::refused(
        "no_turns",
        &format!(
            "{id} is a shell: it has no turns, so {what} cannot tell when a command has finished. \
             Use `connecto send {id} '<command>'` to run a command, then `connecto read {id}` to see its output."
        ),
        None,
    )
}

/// The command line a shell may be sent: one line, no control characters
/// (use `connecto keys` for Ctrl-C and the like). A trailing newline is dropped.
fn shell_line(text: &str) -> Result<(), String> {
    let line = text.trim_end_matches(['\n', '\r']);
    if line.contains(['\n', '\r']) {
        return Err(
            "A shell takes one command line per send. Send each line separately, or join them with `;` or `&&`."
                .to_owned(),
        );
    }
    if line.chars().any(|c| c.is_control() && c != '\t') {
        return Err(
            "A command line cannot hold control characters. Use `connecto keys` for keys such as Ctrl-C."
                .to_owned(),
        );
    }
    Ok(())
}

fn keys_op(ctx: &Arc<Ctx>, caller: &Caller, target: &str, keys: &[String]) -> Reply {
    let mut bytes = Vec::new();
    for key in keys {
        match key_bytes(key) {
            Some(more) => bytes.push(more),
            None => {
                return Reply::refused(
                    "unknown_key",
                    &format!("{key:?} is not a key name. Nothing was sent."),
                    None,
                );
            }
        }
    }
    if ctx.shared.pause().is_paused() {
        return paused_reply();
    }
    let slot = match writable_target(ctx, target) {
        Ok(slot) => slot,
        Err(reply) => return reply,
    };
    if let Some(reply) = not_yours(ctx, caller, &slot.id) {
        return reply;
    }
    let admission = match ask_state(ctx, |reply| Msg::Admit {
        caller: caller.clone(),
        target: slot.id.clone(),
        kind: AdmitKind::Keys,
        reply,
    }) {
        Ok(Ok(admission)) => admission,
        Ok(Err(reply)) | Err(reply) => return reply,
    };
    let Admission { id, slot, admitted } = admission;
    let (reply, outcome) = bounded(1);
    let command = AgentCommand::Keys {
        id: id.clone(),
        source: caller.id.clone(),
        keys: bytes,
        admitted,
        reply,
        enqueued: Instant::now(),
    };
    if slot.send_agent(command).is_err() {
        return Reply::refused("queue_full", "That terminal's write queue is full.", None);
    }
    match outcome.recv_timeout(HANDLER_WAIT) {
        Ok(WriteOutcome::Typed) => Reply::ok(json!({ "id": id, "state": "typed" })),
        Ok(WriteOutcome::Refused(denial)) => denial_reply(denial, None),
        Ok(_) | Err(RecvTimeoutError::Disconnected) => {
            Reply::refused("not_running", "The keys were not sent.", None)
        }
        Err(RecvTimeoutError::Timeout) => Reply::refused(
            "timeout",
            "No outcome in 30 s. The keys may or may not have been sent.",
            Some(&id),
        ),
    }
}

/// The audit record and the admitted epoch of a process-control action.
fn control_gate(
    ctx: &Ctx,
    caller: &Caller,
    action: AgentAction,
    target_name: &str,
    session: usize,
    attempt: u64,
    epoch: u64,
) -> Result<(), Reply> {
    let request = AgentRequest {
        action,
        source: caller.id.clone(),
        target: connecto_core::SessionRef { session, attempt },
        target_name: target_name.to_owned(),
        message: action.name().to_owned(),
        digest: String::new(),
        audit_body: None,
        admitted: Admitted {
            pause_epoch: epoch,
            attempt,
        },
    };
    ctx.shared
        .gate()
        .agent_submit(&request, &TargetState::ready(attempt))
        .map(|_| ())
        .map_err(|denial| denial_reply(denial, None))
}

fn changed_reply(changed: &crate::terminals::Changed) -> Reply {
    let mut header = json!({ "id": changed.id, "state": changed.state, "saved": changed.saved });
    if let Some(word) = changed.removal.word() {
        header["removal"] = json!(word);
    }
    if let Removal::Kept(why) = &changed.removal {
        header["reason"] = json!(why);
    }
    Reply::ok(header)
}

/// Why this project does not take an agent of `kind`: its Tools setting
/// leaves it out. Read as saved, at each add, as a launch reads it. Says
/// which agents it does take, so the asking agent can choose one of them.
fn excluded_by_project(ctx: &Ctx, kind: ToolKind) -> Option<String> {
    crate::project_settings::tool_refusal(&ctx.shared.layout.data_root, &ctx.project_id(), kind)
}

fn add(
    ctx: &Arc<Ctx>,
    caller: &Caller,
    tool: &str,
    name: Option<String>,
    launch: AgentLaunch,
) -> Reply {
    let Some(kind) = ToolKind::from_name(tool) else {
        return Reply::refused(
            "bad_request",
            "add takes claude, codex, pi, antigravity, opencode or shell.",
            None,
        );
    };
    // An agent's name is checked like a person's: refused, never cut.
    let name = match name.as_deref().map_or(Ok(None), clean_name) {
        Ok(name) => name,
        Err(why) => return Reply::refused("bad_request", &why, None),
    };
    // A model goes on the tool's command line: refused, never guessed at.
    if let Some(model) = &launch.model {
        if kind == ToolKind::Shell {
            return Reply::refused("bad_request", "A shell terminal has no model.", None);
        }
        if !crate::terminal_list::valid_model(model) {
            return Reply::refused(
                "bad_request",
                "A model is a plain name such as `opus` or, for pi, `provider/model`: letters, digits, `-`, `_`, `.`, `/`, `:`, `[` and `]`, at most 64.",
                None,
            );
        }
    }
    // An effort too: refused when the tool's command line does not take it.
    if let Some(effort) = &launch.effort {
        if kind == ToolKind::Shell {
            return Reply::refused("bad_request", "A shell terminal has no effort.", None);
        }
        if !crate::terminal_list::valid_effort(kind, effort) {
            return Reply::refused(
                "bad_request",
                &format!("{} does not take the effort {effort:?}.", kind.label()),
                None,
            );
        }
    }
    if let Some(why) = excluded_by_project(ctx, kind) {
        return Reply::refused("not_allowed", &why, None);
    }
    let pause = ctx.shared.pause();
    let epoch = pause.epoch();
    if pause.is_paused() {
        return paused_reply();
    }
    if let Err(reply) = control_gate(
        ctx,
        caller,
        AgentAction::Add,
        tool,
        caller.slot,
        caller.attempt,
        epoch,
    ) {
        return reply;
    }
    match ctx.registry.agent_add(
        tool_of(kind),
        name,
        launch,
        Some(caller.id.clone()),
        epoch,
        HANDLER_WAIT,
    ) {
        Ok(changed) => {
            if let Some(slot) = ctx.shared.slot_by_id(&changed.id) {
                slot.activity.fetch_add(1, Ordering::Relaxed);
            }
            changed_reply(&changed)
        }
        Err(error) => term_error_reply(&error),
    }
}

fn process(ctx: &Arc<Ctx>, caller: &Caller, target: &str, action: AgentAction) -> Reply {
    let pause = ctx.shared.pause();
    let epoch = pause.epoch();
    if pause.is_paused() {
        return paused_reply();
    }
    let Some(slot) = ctx.shared.slot_by_id(target) else {
        return unknown_terminal();
    };
    if let Some(reply) = not_yours(ctx, caller, &slot.id) {
        return reply;
    }
    // A stopped terminal of a CLI the project has since left out does not
    // start again, as one would not be added.
    if action == AgentAction::Start
        && let Some(why) = excluded_by_project(ctx, crate::terminals::kind(slot.tool))
    {
        return Reply::refused("not_allowed", &why, None);
    }
    let attempt = slot.pane.attempt();
    if let Err(reply) = control_gate(ctx, caller, action, &slot.id, slot.index, attempt, epoch) {
        return reply;
    }
    slot.activity.fetch_add(1, Ordering::Relaxed);
    if action == AgentAction::Start {
        return match ctx
            .registry
            .agent_start(&slot.id, epoch, attempt, HANDLER_WAIT)
        {
            Ok(changed) => changed_reply(&changed),
            Err(error) => term_error_reply(&error),
        };
    }
    // Close. On your own terminal the client dies with it, so the reply comes
    // first and the Stop 250 ms later, only if the attempt is still the one
    // admitted. The delayed Stop is not checked against Pause again: stopping
    // is the safe direction.
    if slot.index == caller.slot {
        let registry = ctx.registry.clone();
        let shared = Arc::clone(&ctx.shared);
        let (id, index, admitted) = (slot.id.clone(), slot.index, caller.attempt);
        let delayed = thread::Builder::new()
            .name("self-close".into())
            .spawn(move || {
                thread::sleep(SELF_CLOSE_DELAY);
                if shared
                    .slot(index)
                    .is_some_and(|slot| slot.pane.attempt() == admitted)
                {
                    // No epoch: the documented Pause exception. The attempt is
                    // checked again by the registry, where this may have queued.
                    let admit = Admit {
                        epoch: None,
                        attempt: admitted,
                    };
                    // The client is gone by now, so a failure goes where the user
                    // looks: the registry puts a refused removal in the tab
                    // strip, and this logs both that and a close that failed.
                    match registry.agent_close(&id, admit, HANDLER_WAIT) {
                        Ok(changed) => {
                            if let Removal::Kept(why) = &changed.removal {
                                eprintln!("{id} was closed but not removed: {why}");
                            }
                        }
                        Err(err) => eprintln!("the delayed close of {id} did not happen: {err}"),
                    }
                }
            });
        return if delayed.is_ok() {
            // The removal follows the stop, and this reply is written before it,
            // so it can promise only what is known now: a list that cannot be
            // written is not changed, and otherwise the removal depends on the
            // save that comes later.
            let view = ctx.registry.view_arc().load_full();
            let mut header = json!({
                "id": slot.id,
                "state": "closing",
                "saved": view.save != SaveState::Unsaved,
            });
            if view.save == SaveState::ReadOnly {
                header["removal"] = json!("kept");
                header["reason"] = json!("the terminal list is read-only this run");
            } else {
                header["removal"] = json!("pending");
                header["reason"] = json!(
                    "closing needs the list to be saved; if that fails the terminal stays stopped and Connecto says why"
                );
            }
            Reply::ok(header)
        } else {
            Reply::refused("busy", "Could not schedule the close.", None)
        };
    }
    let admit = Admit {
        epoch: Some(epoch),
        attempt,
    };
    match ctx.registry.agent_close(&slot.id, admit, HANDLER_WAIT) {
        Ok(changed) => changed_reply(&changed),
        Err(error) => term_error_reply(&error),
    }
}

/// The hook helper's call. Only the helper uses it, and it is allowed under
/// Pause, because under Pause notices arrive as context.
fn drain(ctx: &Arc<Ctx>, caller: &Caller, prompt: Option<&str>, holds: bool, stream: &mut Stream) {
    // The prompt and the project's operations (ADR 0031): a line for the
    // agent, or the prompt stopped because Connecto ran the operation.
    let advice = match (prompt, ops_caller(ctx, caller)) {
        (Some(prompt), Some(mut who)) if !ctx.shared.pause().is_paused() => {
            who.holds = holds;
            super::ops::ops_bridge().on_prompt(&who, prompt)
        }
        _ => connecto_core::ops::PromptAdvice::Nothing,
    };
    let ops_note = match advice {
        connecto_core::ops::PromptAdvice::Intercept { reason, ticket } => {
            // The hook confirms with `connecto op intercept <ticket>` and only
            // then prints the block: nothing runs before it has the reply,
            // and a hook that gave up never blocks. The inbox is not
            // drained: its notices wait for the next prompt.
            let body = json!({ "decision": "block", "reason": reason })
                .to_string()
                .into_bytes();
            let header = json!({ "intercept": ticket });
            let _ = write_reply(stream, &Reply::ok(header).with_body(body));
            return;
        }
        connecto_core::ops::PromptAdvice::Suggest(line) => Some(line),
        connecto_core::ops::PromptAdvice::Nothing => None,
    };
    // A new memory version waits for this prompt: nothing was woken for it.
    let memory_version = memory::project_of_key(ctx.shared.project_key.load(Ordering::SeqCst))
        .and_then(|project| memory::memory_bridge().take_version_notice(&project, &caller.id));
    let drained: Option<Drained> = match ask_state(ctx, |reply| Msg::Drain {
        caller: caller.clone(),
        memory_version,
        ops_note,
        reply,
    }) {
        Ok(drained) => drained,
        Err(reply) => {
            let _ = write_reply(stream, &reply);
            return;
        }
    };
    let Some(drained) = drained else {
        let _ = write_reply(stream, &Reply::ok(json!({})));
        return;
    };
    // A drop guard releases the reservation on every way out but a good write.
    struct Release<'a> {
        ctx: &'a Ctx,
        reservation: u64,
        written: bool,
    }
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            if self.reservation != 0 {
                // Owed, not sent: a full queue must not leave notices reserved.
                self.ctx.shared.owe(Owed::DrainDone {
                    reservation: self.reservation,
                    written: self.written,
                });
            }
        }
    }
    let mut release = Release {
        ctx,
        reservation: drained.reservation,
        written: false,
    };
    let reply = Reply::ok(json!({})).with_body(drained.body);
    let ok = write_reply(stream, &reply).is_ok();
    release.written = ok;
}

/// `connecto tasks`: the background work a terminal's agent left running,
/// open to all like `read`. With `stop`, its background shells are stopped,
/// with the rules of `keys`: not under Pause, only a terminal of yours, and
/// through the Gate.
/// `connecto send-file <path>`: offer the file to the machines viewing this
/// Connecto that show the caller's project. They download it from the offer
/// (see `connecto_core::offers`); nothing is sent until one asks.
fn send_file_op(ctx: &Ctx, caller: &Caller, path: &str) -> Reply {
    if ctx.shared.pause().is_paused() {
        return paused_reply();
    }
    let Some(project) = memory::project_of_key(ctx.shared.project_key.load(Ordering::SeqCst))
    else {
        return Reply::refused(
            "bad_request",
            "This terminal is in no project, so no other machine can see it.",
            None,
        );
    };
    match connecto_core::offers::shared().offer(&project, &caller.id, std::path::Path::new(path)) {
        Ok(offer) => Reply::ok(json!({
            "note": format!(
                "Offered {} ({}) to the machines connected to this Connecto that show this project: each shows it with a Download button, for {} minutes. Nothing is sent until one downloads it. If no machine is connected, tell the user to connect from the other Mac and ask again.",
                offer.name,
                size_words(offer.bytes),
                connecto_core::offers::OFFER_TTL.as_secs() / 60,
            ),
        })),
        Err(why) => Reply::refused("bad_request", &format!("Not offered: {why}."), None),
    }
}

fn size_words(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if bytes < 1024 * 1024 {
        format!("{} KB", bytes.div_ceil(1024).max(1))
    } else {
        format!("{mb:.1} MB")
    }
}

fn tasks_op(ctx: &Ctx, caller: &Caller, target: &str, stop: bool, task: Option<&str>) -> Reply {
    let Some(slot) = ctx.shared.slot_by_id(target) else {
        return unknown_terminal();
    };
    if !stop {
        let running = slot.background_shown();
        return Reply::ok(json!({
            "id": slot.id,
            "state": state::state_word(&slot),
            "tasks": background::json(&running, Instant::now()),
        }));
    }
    let pause = ctx.shared.pause();
    let epoch = pause.epoch();
    if pause.is_paused() {
        return paused_reply();
    }
    if let Some(reply) = not_yours(ctx, caller, &slot.id) {
        return reply;
    }
    // Refusals that need no Gate come first: nothing to stop is not an action.
    if let Err(why) = background::shell_commands(&slot.background_shown(), task) {
        return Reply::refused("nothing_to_stop", &why, None);
    }
    let attempt = slot.pane.attempt();
    if let Err(reply) = control_gate(
        ctx,
        caller,
        AgentAction::StopTasks,
        &slot.id,
        slot.index,
        attempt,
        epoch,
    ) {
        return reply;
    }
    slot.activity.fetch_add(1, Ordering::Relaxed);
    match background::stop(&slot, task, true, |_| {}) {
        Ok(note) => Reply::ok(json!({ "id": slot.id, "state": "stopping", "note": note })),
        Err(why) => Reply::refused("not_stopped", &why, None),
    }
}

fn unknown_terminal() -> Reply {
    Reply::refused("unknown_terminal", "No terminal has that id.", None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A send that failed once the text was out, whose refusal the audit
    /// log could not take: the caller still gets the request id and what
    /// became of the text, so it can check before sending again.
    #[test]
    fn an_unrecorded_refusal_keeps_the_request_id_and_what_was_delivered() {
        let refused = Reply::refused(
            "timeout",
            "No outcome in 30 s. The text may or may not have been typed, and it is not retried.",
            Some("r7"),
        );
        let reply = audit_failed_reply(refused);
        assert!(!reply.is_ok());
        assert_eq!(reply.header["code"], "audit_failed");
        assert_eq!(reply.header["refused_code"], "timeout");
        assert_eq!(reply.header["id"], "r7");
        let reason = reply.header["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("may or may not have been typed"),
            "{reason}"
        );
        assert!(!reason.contains("nothing was done"), "{reason}");
    }
}
