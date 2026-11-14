//! The bridge server: the socket, caller identification, and one short-lived
//! handler thread per connection (section 5.11).
//!
//! Identity comes only from the peer process tree. There is no token, and
//! `CONNECTO_TERMINAL` is never trusted. Handlers have their own deadlines, so a
//! stuck actor delays only its own caller.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::{PaneControl, PaneStage, SaveState, SessionStatus, TerminalSession, ToolKind};
use connecto_gate::{Admitted, AgentAction, AgentRequest, Denial, TargetState};
use connecto_hook::bridge::{BadRequest, Op, Reply, key_bytes};
use connecto_hook::{MAX_FRAME, decode};
use crossbeam_channel::{RecvTimeoutError, Sender, bounded};
use serde_json::json;

use super::state::{Admission, AdmitKind, Drained, Msg, Owed};
use super::{AgentCommand, Caller, HANDLER_WAIT, Report, WriteKind, WriteOutcome};
use crate::local_socket::{self, Listener, Stream};
use crate::process_tree::parent_of;
use crate::relay::{Shared, Slot};
use crate::terminal_list::clean_name;
use crate::terminals::{Admit, Registry, Removal, TermError, tool_of};
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
}

impl Server {
    pub(crate) fn path(&self) -> &Path {
        &self.path
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

struct Ctx {
    shared: Arc<Shared>,
    registry: Registry,
    state: Sender<Msg>,
    live: AtomicUsize,
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
    Ok(Server { path, stop })
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
    if let Op::Drain { .. } = op {
        return drain(ctx, &caller, &mut stream);
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

fn dispatch(ctx: &Arc<Ctx>, caller: &Caller, op: Op) -> Reply {
    match op {
        Op::List => ask_state(ctx, |reply| Msg::List {
            caller: caller.clone(),
            reply,
        })
        .unwrap_or_else(|r| r),
        Op::Result { request } => ask_state(ctx, |reply| Msg::Result {
            caller: caller.clone(),
            request,
            reply,
        })
        .unwrap_or_else(|r| r),
        Op::Watch { target } => {
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
                reply,
            })
            .unwrap_or_else(|r| r)
        }
        Op::Unwatch { target } => ask_state(ctx, |reply| Msg::Watch {
            caller: caller.clone(),
            target,
            on: false,
            reply,
        })
        .unwrap_or_else(|r| r),
        Op::Read { target } => read(ctx, caller, &target),
        Op::Send { target, ask, text } => send(ctx, caller, &target, ask, &text),
        Op::Keys { target, keys } => keys_op(ctx, caller, &target, &keys),
        Op::Add { tool, name, model } => add(ctx, caller, &tool, name.as_deref(), model.as_deref()),
        Op::Start { target } => process(ctx, caller, &target, AgentAction::Start),
        Op::Close { target } => process(ctx, caller, &target, AgentAction::Close),
        Op::Drain { .. } => Reply::refused("bad_request", "drain is for the hook helper.", None),
    }
}

fn read(ctx: &Ctx, caller: &Caller, target: &str) -> Reply {
    let Some(slot) = ctx.shared.slot_by_id(target) else {
        return unknown_terminal();
    };
    // Decided on the session itself, not on a stage: a login starting at this
    // moment cannot slip through a stage check.
    let Some(session) = slot.pane.session() else {
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
    ctx.shared
        .gate()
        .agent_note(&caller.id, &slot.id, slot.pane.attempt());
    Reply::ok(json!({ "history_full": history_full })).with_body(text.into_bytes())
}

fn send(ctx: &Arc<Ctx>, caller: &Caller, target: &str, ask: bool, text: &str) -> Reply {
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
            caller.id
        )
    };
    let (reply, outcome) = bounded(1);
    let command = AgentCommand::Type {
        id: id.clone(),
        source: caller.id.clone(),
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

fn add(
    ctx: &Arc<Ctx>,
    caller: &Caller,
    tool: &str,
    name: Option<&str>,
    model: Option<&str>,
) -> Reply {
    let Some(kind) = ToolKind::from_name(tool) else {
        return Reply::refused("bad_request", "add takes claude, codex or shell.", None);
    };
    // An agent's name is checked like a person's: refused, never cut.
    let name = match name.map_or(Ok(None), clean_name) {
        Ok(name) => name,
        Err(why) => return Reply::refused("bad_request", &why, None),
    };
    // A model goes on the tool's command line: refused, never guessed at.
    if let Some(model) = model {
        if kind == ToolKind::Shell {
            return Reply::refused("bad_request", "A shell terminal has no model.", None);
        }
        if !crate::terminal_list::valid_model(model) {
            return Reply::refused(
                "bad_request",
                "A model is a plain name such as `opus`: letters, digits, `-`, `_`, `.`, `[` and `]`, at most 64.",
                None,
            );
        }
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
        model.map(str::to_owned),
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
fn drain(ctx: &Arc<Ctx>, caller: &Caller, stream: &mut Stream) {
    let drained: Option<Drained> = match ask_state(ctx, |reply| Msg::Drain {
        caller: caller.clone(),
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

fn unknown_terminal() -> Reply {
    Reply::refused("unknown_terminal", "No terminal has that id.", None)
}
