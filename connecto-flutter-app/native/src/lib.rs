//! The Flutter peer owns no terminals. Its outbound side uses the same manager,
//! device pairing, pinned hosts and reconnect supervisor as the desktop app.
mod attachments;

use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use connecto_core::question;
use connecto_core::session::APP_SESSION;
use connecto_core::{
    GuardDenied, HostLink, PairingPhase, RemoteControl, RemoteGuard, RemotePeer, TermActivity,
    TermSnapshot, TerminalEntry, ToolKind,
};
use connecto_net::{
    auth::DeviceKeySigners,
    host::{HostTerminal, HostTerminals},
    manager::{DEFAULT_RELAY_URL, ManagerConfig, RemoteManager},
    pairing,
    project_panels::ProjectAsk,
    reconnect::SupervisorConfig,
    remote_terminals::RosterTerminal,
    secrets::FileSecrets,
    siri::SiriNotify,
    wire::{MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS},
};
use serde_json::{Value, json};

static MANAGER: OnceLock<Arc<RemoteManager>> = OnceLock::new();
struct ClientOnly(AtomicBool);
impl HostTerminals for ClientOnly {
    fn list(&self) -> Vec<HostTerminal> {
        Vec::new()
    }
}
impl RemoteGuard for ClientOnly {
    fn enabled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    fn set_enabled(&self, on: bool) {
        self.0.store(on, Ordering::Release);
    }
    fn connected(&self, _: &RemotePeer, _: Result<(), &str>) {}
    fn attach(&self, _: &RemotePeer, _: &str, _: u64) -> Result<(), GuardDenied> {
        Err(GuardDenied::Refused)
    }
    fn detach(&self, _: &RemotePeer, _: &str, _: u64) {}
    fn input(&self, _: &RemotePeer, _: &str, _: u64, _: &[u8]) -> Result<(), GuardDenied> {
        Err(GuardDenied::Refused)
    }
    fn resize(&self, _: &RemotePeer, _: &str, _: u64) -> Result<(), GuardDenied> {
        Err(GuardDenied::Refused)
    }
    fn manage(
        &self,
        _: &RemotePeer,
        action: &'static str,
        _: &str,
        _: u64,
    ) -> Result<(), GuardDenied> {
        if action == "pair" && self.enabled() {
            Ok(())
        } else {
            Err(GuardDenied::Refused)
        }
    }
}

fn initialize(dir: PathBuf) -> Result<(), String> {
    let key_dir = dir.join("keys");
    std::fs::create_dir_all(&key_dir).map_err(|e| e.to_string())?;
    let secrets = Arc::new(FileSecrets(key_dir));
    let policy = Arc::new(ClientOnly(AtomicBool::new(false)));
    MANAGER.get_or_init(|| {
        RemoteManager::start_chat_client(ManagerConfig {
            dir,
            secrets: secrets.clone(),
            guard: policy.clone(),
            terminals: policy,
            signers: Arc::new(DeviceKeySigners(secrets)),
            wake: Arc::new(|| {}),
            supervisor: SupervisorConfig::DEFAULT,
            bind: None,
            share_loopback: false,
            device_name: "Connecto Chat".into(),
            kdf: pairing::KdfParams::PRODUCTION,
            pairing_lifetime: pairing::CODE_LIFETIME,
            default_relay: Some(DEFAULT_RELAY_URL.into()),
        })
    });
    Ok(())
}

fn connected(link: &HostLink) -> bool {
    matches!(
        link,
        HostLink::Connected | HostLink::Direct | HostLink::Relayed
    )
}

/// What the app's chooser offers for `tool` in `project`: the host's models,
/// and whether the host reads a chosen one (one from before models offers only
/// the tool's default).
fn models(
    manager: &RemoteManager,
    host: &str,
    project: &str,
    tool: ToolKind,
) -> Result<Value, String> {
    Ok(match manager.models(host, project, tool)? {
        Some(models) => json!({"supported":true,"default":models.default,"models":models.models}),
        None => json!({"supported":false,"default":null,"models":[]}),
    })
}

/// The project's `.functionalMemory/` files on the host, for a voice call's
/// context (ADR 0033): `{"files": [{"path", "text"}], "truncated", "status":
/// {"state", "commit", "updatedMs", "message", "auto"}}`. The index first.
fn functional_memory(
    manager: &RemoteManager,
    host: &str,
    project: &str,
    update: bool,
) -> Result<Value, String> {
    let (files, status) = manager.functional_memory(host, project, update)?;
    let list: Vec<Value> = files
        .files
        .iter()
        .map(|f| json!({"path": f.path, "text": f.text}))
        .collect();
    Ok(json!({
        "files": list,
        "truncated": files.truncated,
        "status": {
            "state": status.state.name(),
            "commit": status.commit,
            "updatedMs": status.updated_ms,
            "message": status.message,
            "auto": status.auto,
        },
    }))
}

fn link(link: &HostLink) -> String {
    match link {
        HostLink::Connected | HostLink::Direct | HostLink::Relayed => "Connected".into(),
        HostLink::Connecting => "Connecting".into(),
        HostLink::Disconnected => "Disconnected".into(),
        HostLink::Reconnecting { retry_in_secs } => format!("Reconnecting in {retry_in_secs}s"),
        HostLink::Refused(reason) => reason.clone(),
    }
}

fn state(manager: &RemoteManager) -> Value {
    let view = manager.view();
    let phase = match &view.pairing {
        PairingPhase::ShowingCode {
            my_code,
            expires_in_secs,
        }
        | PairingPhase::WaitingForPeer {
            my_code,
            expires_in_secs,
        } => {
            json!({"phase":"code","code":my_code,"seconds":expires_in_secs,"waiting":matches!(view.pairing,PairingPhase::WaitingForPeer{..})})
        }
        PairingPhase::Preparing => json!({"phase":"Preparing"}),
        PairingPhase::NameNeeded { suggested_name } => {
            json!({"phase":"name","name":suggested_name})
        }
        PairingPhase::Failed { message } => json!({"phase":"failed","message":message}),
        PairingPhase::Connecting | PairingPhase::Confirming | PairingPhase::Keeping => {
            json!({"phase":"Confirming pairing"})
        }
        _ => json!({"phase":"idle"}),
    };
    let hosts: Vec<_> = view
        .hosts
        .iter()
        .map(|h| {
            // Stale while disconnected: nothing is shown working then.
            let working = if connected(&h.link) {
                manager.working_projects(&h.name)
            } else {
                Default::default()
            };
            json!({"name":h.name,"link":link(&h.link),
                "projects":manager.projects(&h.name).iter().map(|p|json!({"id":p.id,"name":p.name,"working":working.contains(&p.id)})).collect::<Vec<_>>()
            })
        })
        .collect();
    json!({"hosts":hosts,"relay":view.relay_url,"pairing":phase,"error":view.last_error})
}

fn screen(entry: &TerminalEntry) -> (TermSnapshot, String) {
    let mut snap = TermSnapshot::default();
    entry.session.poll_snapshot(&mut snap);
    let text = (0..snap.rows)
        .map(|r| {
            snap.row(r)
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (snap, text)
}

fn status(activity: TermActivity) -> &'static str {
    match activity {
        TermActivity::Working => "Running",
        TermActivity::NeedsUser => "Needs an answer",
        TermActivity::Starting => "Starting",
        TermActivity::NeedsLogin => "Login required",
        TermActivity::NotStarted | TermActivity::Exited => "Stopped",
        TermActivity::Failed => "Failed",
        _ => "Ready",
    }
}

/// What a terminal is doing, in words for the call's voice assistant.
fn activity_words(activity: TermActivity) -> &'static str {
    match activity {
        TermActivity::NotStarted => "not started",
        TermActivity::Starting => "starting",
        TermActivity::NeedsLogin => "needs a login",
        TermActivity::Working => "working",
        TermActivity::NeedsUser => "waiting for the user",
        TermActivity::Idle => "idle",
        TermActivity::Interrupted => "interrupted",
        TermActivity::Done => "done",
        TermActivity::Exited => "stopped",
        TermActivity::Failed => "failed",
        TermActivity::Plain => "shell",
    }
}

/// The project's terminals at a glance, for a call: every terminal of the
/// project the host lists (not only the chat this client attaches to), with
/// labels the host controls. No agent turn, nothing written to any chat, and
/// no screen text or window title, which programs write.
fn terminals(roster: &[RosterTerminal]) -> Value {
    let rows: Vec<Value> = roster
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "name": t.name,
                "tool": t.tool.label(),
                "state": activity_words(t.state),
                "inTurn": t.in_turn,
                // Who it belongs to, as `connecto list` says: the agent that
                // asked for it, else the one that started it. None: the
                // person opened it.
                "startedBy": t.owner,
                "appSession": t.app_session,
                // How many, never what: a job's label is its command line.
                "backgroundJobs": t.background.len(),
            })
        })
        .collect();
    json!({ "terminals": rows })
}

fn current_question(entry: &TerminalEntry, display: &str) -> Option<question::Question> {
    if matches!(
        entry.state,
        TermActivity::NotStarted | TermActivity::Failed | TermActivity::Exited
    ) {
        None
    } else {
        question::active(entry.tool, display)
    }
}

/// The project's task board, kept by its app session agent (ADR 0029).
const BOARD_PATH: &str = ".connecto/board.json";
const MAX_BOARD_BYTES: usize = 512 * 1024;

/// The board file's text, read from the project folder through the host's
/// file read (as one of its terminals, which the read is checked against);
/// `null` while there is none. Unreadable (no terminal to read it as, or
/// not UTF-8) is an error, never an empty board.
fn board(
    control: &dyn connecto_core::TerminalsControl,
    terminal: Option<&TerminalEntry>,
) -> Result<Value, String> {
    let Some(terminal) = terminal else {
        return Err("The board can't be read: this project has no terminal open.".into());
    };
    match control.fetch_file(&terminal.id, BOARD_PATH) {
        Ok((_, bytes)) if bytes.len() > MAX_BOARD_BYTES => {
            Err("The task board is too large.".into())
        }
        Ok((_, bytes)) => String::from_utf8(bytes)
            .map(|text| json!({ "text": text }))
            .map_err(|_| "The task board is not valid text.".into()),
        // The host's one refusal for a missing file.
        Err(why) if why.starts_with("no such file") => Ok(json!({"text":null})),
        Err(why) => Err(why),
    }
}

/// A chat completion from host `host`'s conversation model: `{"text"}`, or
/// `{"error", "unavailable"}`, where `unavailable` means the host has none
/// to offer (none set up, or too old) rather than that this one failed.
fn complete(manager: &RemoteManager, host: &str, v: &Value) -> Result<Value, String> {
    use connecto_net::reconnect::CompletionError;
    use connecto_net::wire::{Completion, Role};
    let messages = v["messages"]
        .as_array()
        .ok_or("Missing messages")?
        .iter()
        .map(|m| {
            let role = m["role"].as_str().and_then(Role::from_name)?;
            Some((role, m["content"].as_str()?.to_owned()))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or("Bad messages")?;
    let request = Completion {
        messages,
        max_tokens: v["maxTokens"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(120),
        temperature: v["temperature"]
            .as_f64()
            .map_or(30, |t| (t.clamp(0.0, 2.0) * 100.0).round() as u8),
        wait_ms: v["timeoutMs"]
            .as_u64()
            .map_or(5000, |ms| u32::try_from(ms.min(60_000)).unwrap_or(60_000)),
    };
    Ok(match manager.complete(host, &request) {
        Ok(text) => json!({ "text": text }),
        Err(CompletionError::Unavailable(why)) => json!({"error": why, "unavailable": true}),
        Err(CompletionError::Failed(why)) => json!({"error": why, "unavailable": false}),
    })
}

/// Whether `text`, what the person has said so far in a voice call, is a
/// finished turn, and (with a `context`) whether it fits the call, as host
/// `host`'s Jev judges it: `{"complete": 0.0..=1.0, "fits": 0.0..=1.0 or
/// null}`, or `{"error", "unavailable"}`, where `unavailable` means the host
/// cannot judge at all (no Jev key, or too old) rather than that this one
/// failed.
fn turn_check(manager: &RemoteManager, host: &str, text: &str, v: &Value) -> Value {
    use connecto_net::wire::TurnVerdict;
    let wait = std::time::Duration::from_millis(v["timeoutMs"].as_u64().unwrap_or(1200).min(5000));
    let context = v["context"].as_str().unwrap_or("");
    match manager.turn_check(host, text, context, wait) {
        TurnVerdict::Judged { complete, fits } => json!({
            "complete": f64::from(complete) / 100.0,
            "fits": fits.map(|f| f64::from(f) / 100.0),
        }),
        TurnVerdict::Unavailable(why) => json!({"error": why, "unavailable": true}),
        TurnVerdict::Failed(why) => json!({"error": why, "unavailable": false}),
    }
}

/// The host ask behind one of Siri's operations (`ios/Runner/Siri`), from
/// its fields; `None` for any other operation. The terminal is the app
/// session's id as `chat` gave it (the wire's number).
fn siri_ask(op: &str, v: &Value) -> Result<Option<ProjectAsk>, String> {
    let field = |k: &str| {
        v[k].as_str()
            .map(String::from)
            .ok_or_else(|| format!("Missing {k}"))
    };
    let terminal = || {
        field("id")?
            .parse::<u32>()
            .map_err(|_| "The session changed. Please try again.".to_owned())
    };
    // Swift names the fields as Dart does; the host reads them as the wire has them.
    let notify = |n: &Value| -> Result<SiriNotify, String> {
        SiriNotify::from_json(&json!({
            "device_token": n["deviceToken"],
            "live_activity_token": n["liveActivityToken"],
            "key": n["key"],
            "env": n["env"],
            "topic": n["topic"],
            "locale": n["locale"],
        }))
        .ok_or_else(|| "Bad notification settings".to_owned())
    };
    Ok(Some(match op {
        "siriAsk" => ProjectAsk::SiriAsk {
            request_id: field("requestId")?,
            terminal: terminal()?,
            text: field("text")?,
            question: v["question"].as_str().map(String::from),
            notify: match &v["notify"] {
                Value::Null => None,
                n => Some(notify(n)?),
            },
        },
        "siriResult" => ProjectAsk::SiriResult {
            request_id: field("requestId")?,
        },
        "siriDelivered" => ProjectAsk::SiriDelivered {
            request_id: field("requestId")?,
        },
        "siriStatus" => ProjectAsk::SiriStatus {
            terminal: terminal()?,
        },
        "activityToken" => ProjectAsk::ActivityToken {
            request_id: field("requestId")?,
            token: field("token")?,
        },
        "pushRegister" => ProjectAsk::PushRegister {
            old_device_token: v["oldDeviceToken"].as_str().map(String::from),
            to: notify(v)?,
        },
        _ => return Ok(None),
    }))
}

/// The questions the project's terminals ask now, or the answer to one of
/// them, for a voice call (`connecto_net::voice_answers` on the host): any
/// terminal the phone's latest list grants it, a coder's or a reviewer's
/// too. `questions`: `{"questions": [{"terminal" (its wire id), "shown",
/// "name", "tool", "appSession", "id", "occurrence", "kind", "text", "prompt",
/// "voiceText", "chat", "task", "options": [{"index", "label", "tapOnly"}],
/// "selected"}], "voice"}`. `answer` (`terminal`, `question`, `occurrence`, and `option`
/// or `text`): `{"outcome": "answered" | "sent"}`, written by the host only
/// while that terminal still shows that very question.
///
/// A host from before these asks cannot write conditionally: its app
/// session's question is shown from the phone's own mirror, with nothing to
/// answer by voice (`"voice": false`), and `answer` is refused.
fn questions_or_answer(
    manager: &RemoteManager,
    host: &str,
    project: &str,
    op: &str,
    v: &Value,
    app: Option<&TerminalEntry>,
) -> Result<Value, String> {
    use connecto_net::voice_answers::{self, AnswerChoice};
    let ask = if op == "questions" {
        ProjectAsk::Questions
    } else {
        let choice = match (v["option"].as_u64(), v["text"].as_str()) {
            (Some(n), None) => {
                AnswerChoice::Option(usize::try_from(n).map_err(|_| "Unknown choice")?)
            }
            (None, Some(text)) => AnswerChoice::Text(text.to_owned()),
            _ => return Err("Give either an option or a text.".into()),
        };
        ProjectAsk::QuestionAnswer {
            terminal: v["terminal"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("Missing terminal")?,
            question: v["question"].as_str().ok_or("Missing question")?.to_owned(),
            occurrence: v["occurrence"]
                .as_str()
                .ok_or("Missing occurrence")?
                .to_owned(),
            choice,
        }
    };
    match manager.siri(host, project, &ask) {
        Ok(mut answer) => {
            if op == "questions" {
                answer["voice"] = json!(true);
            }
            Ok(answer)
        }
        Err(why) if connecto_net::siri::is_unknown_ask(&why) => {
            if op != "questions" {
                return Err(
                    "This Mac's Connecto can't answer questions by voice: update it, or answer on screen."
                        .into(),
                );
            }
            let rows: Vec<Value> = app
                .and_then(|t| {
                    let seen = voice_answers::look(t)?;
                    let wire = t.id.parse::<u32>().ok()?;
                    seen.question
                        .map(|q| shown_only(voice_answers::question_json(t, wire, &q, None)))
                })
                .into_iter()
                .collect();
            Ok(json!({"questions": rows, "voice": false}))
        }
        Err(why) => Err(why),
    }
}

/// A call's prompt (a task to start, a question to analyse) for the app
/// session, typed by the host only while the app session is between turns,
/// shows no question and has nothing in its input line
/// (`connecto_net::voice_answers::send_prompt`): `{"outcome"}`. A Mac from
/// before that ask cannot make sure of it, so nothing is sent there.
fn send_prompt(
    manager: &RemoteManager,
    host: &str,
    project: &str,
    v: &Value,
    app: Option<&TerminalEntry>,
) -> Result<Value, String> {
    let app = app.ok_or("The app session is no longer open")?;
    if v["id"].as_str() != Some(app.id.as_str()) {
        return Err("The session changed. Please try again.".into());
    }
    let ask = ProjectAsk::SendPrompt {
        terminal: app
            .id
            .parse::<u32>()
            .map_err(|_| "The session changed. Please try again.")?,
        text: v["text"].as_str().ok_or("Missing text")?.to_owned(),
    };
    match manager.siri(host, project, &ask) {
        Err(why) if connecto_net::siri::is_unknown_ask(&why) => Err(
            "This Mac's Connecto can't make sure the agent is free before a call types into it: update it."
                .into(),
        ),
        other => other,
    }
}

/// A question to show and leave to a tap: no words and no choice of it
/// taken by voice (a host that cannot write conditionally).
fn shown_only(mut row: Value) -> Value {
    row["voiceText"] = json!(false);
    if let Some(options) = row["options"].as_array_mut() {
        for o in options {
            o["tapOnly"] = json!(true);
        }
    }
    row
}

/// The longest a Siri call waits when the caller names no `timeoutMs`.
const SIRI_WAIT_MS: u64 = 10_000;

/// A Siri ask's answer, within the caller's `timeoutMs` (what is left of
/// Siri's budget) in all. A host from before Siri answers in words Siri can
/// tell apart: `{"outcome": "unsupported"}` to an ask (the plain send
/// follows, with no promise to notify), `{"state": "unsupported"}` to the
/// rest. An ask that never reached the host is `{"outcome": "not-sent"}`;
/// one that went with no answer in time, `{"outcome": "unconfirmed"}` (asked
/// again only with the same request id).
fn siri(
    manager: &RemoteManager,
    host: &str,
    project: &str,
    op: &str,
    ask: &ProjectAsk,
    wait: std::time::Duration,
) -> Result<Value, String> {
    use connecto_net::siri::SiriCallError;
    let asking = op == "siriAsk";
    match manager.siri_within(host, project, ask, wait) {
        Ok(answer) => Ok(answer),
        Err(SiriCallError::UnknownAsk) => Ok(if asking {
            json!({"outcome": "unsupported"})
        } else {
            json!({"state": "unsupported"})
        }),
        Err(SiriCallError::NotSent(_)) if asking => Ok(json!({"outcome": "not-sent"})),
        Err(SiriCallError::Unconfirmed) if asking => Ok(json!({"outcome": "unconfirmed"})),
        Err(SiriCallError::Refused(why)) => Err(why),
        Err(other) => Err(other.to_string()),
    }
}

/// The wait a Siri call may take: its `timeoutMs`, at most a minute.
fn siri_wait(v: &Value) -> std::time::Duration {
    std::time::Duration::from_millis(v["timeoutMs"].as_u64().unwrap_or(SIRI_WAIT_MS).min(60_000))
}

fn request(v: &Value) -> Result<Value, String> {
    let field = |k: &str| v[k].as_str().ok_or_else(|| format!("Missing {k}"));
    let op = field("op")?;
    if op == "init" {
        initialize(PathBuf::from(field("dir")?))?;
        return Ok(Value::Null);
    }
    let manager = MANAGER.get().ok_or("Peer has not started")?;
    match op {
        "state" => return Ok(state(manager)),
        "pair" => manager.start_mutual_pairing()?,
        "code" => manager.submit_peer_code(field("code")?)?,
        "name" => manager.finish_pairing(field("name")?)?,
        "cancel" => manager.cancel_pairing(),
        "connect" => manager.connect(field("host")?),
        "forget" => {
            let host = field("host")?;
            manager.forget_host(host)?;
            attachments::forget(host);
        }
        "detach" => attachments::detach(
            (field("host")?, field("project")?, field("id")?),
            field("token")?,
        ),
        "relay" => manager.set_relay(
            Some(field("url")?.into()),
            v["token"].as_str().map(String::from),
        )?,
        "complete" => return complete(manager, field("host")?, v),
        "turnCheck" => return Ok(turn_check(manager, field("host")?, field("text")?, v)),
        _ => {
            let host = field("host")?;
            if !manager
                .view()
                .hosts
                .iter()
                .any(|h| h.name == host && connected(&h.link))
            {
                return Err("Host disconnected".into());
            }
            let project = field("project")?;
            if !manager.projects(host).iter().any(|p| p.id == project) {
                return Err("Project is no longer open".into());
            }
            let agent = || {
                ToolKind::from_name(field("tool")?)
                    .filter(|t| t.is_agent())
                    .ok_or_else(|| "Unknown provider".to_owned())
            };
            if op == "models" {
                return models(manager, host, project, agent()?);
            }
            if matches!(op, "functional_memory" | "functional_memory_update") {
                return functional_memory(manager, host, project, op == "functional_memory_update");
            }
            if let Some(ask) = siri_ask(op, v)? {
                return siri(manager, host, project, op, &ask, siri_wait(v));
            }
            if op == "terminals" {
                // From the host's whole list: this chat client attaches only
                // the app session, and its view has nothing else.
                let roster = manager
                    .project_roster(host, project)
                    .ok_or("Project unavailable")?;
                return Ok(terminals(&roster));
            }
            let control = manager
                .project_terminals(host, project)
                .ok_or("Project unavailable")?;
            let view = control.view();
            let entry = view.terminals.iter().find(|t| {
                t.name.as_deref() == Some(APP_SESSION) && t.parent.is_none() && t.tool.is_agent()
            });
            if matches!(op, "questions" | "answer") {
                return questions_or_answer(manager, host, project, op, v, entry);
            }
            if op == "sendPrompt" {
                return send_prompt(manager, host, project, v, entry);
            }
            if op == "board" {
                return board(control.as_ref(), entry.or_else(|| view.terminals.first()));
            }
            if op == "create" {
                let tool = agent()?;
                // Absent or empty: the agent's own default.
                let model = v["model"]
                    .as_str()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(String::from);
                if entry.is_none() {
                    control.add_on_model(tool, Some(APP_SESSION.into()), model)?;
                }
                return Ok(Value::Null);
            }
            let Some(entry) = entry else {
                if op == "chat" {
                    // The agents the host project's Tools setting allows, once
                    // its settings are here (read before what it offers, so a
                    // reply landing in between never adds one it leaves out).
                    let loading = control.agents_loading();
                    let agents: Vec<&str> = ToolKind::ALL
                        .into_iter()
                        .filter(|t| t.is_agent() && control.offers(*t))
                        .map(ToolKind::label)
                        .collect();
                    return Ok(json!({
                        "status": "New",
                        "error": view.last_error,
                        "agents": agents,
                        "agentsLoading": loading,
                    }));
                }
                return Err("The app session is no longer open".into());
            };
            let (snap, display) = screen(entry);
            let dialog = current_question(entry, &display);
            let prompt = dialog.as_ref().map(|q| q.text.as_str());
            let text_answer = dialog.as_ref().is_some_and(|q| q.accepts_text);
            let status = if prompt.is_some() {
                "Needs an answer"
            } else {
                status(entry.state)
            };
            if op == "chat" {
                let history = entry.session.chat_history();
                let question = history.as_ref().and(prompt);
                let chat_ready = history.is_some();
                let attach = (chat_ready && entry.session.chat_attachments_supported()).then(
                    || json!({"maxBytes": MAX_ATTACHMENT_BYTES, "maxFiles": MAX_ATTACHMENTS}),
                );
                let display_history = history.or_else(|| entry.session.cached_chat_history());
                return Ok(
                    json!({"id":entry.id,"provider":entry.tool.label(),"status":status,"question":question,"textAnswer":question.is_some() && text_answer,
                    "chatReady":chat_ready,"attach":attach,"messages":display_history.as_ref().map(|h|&h.messages),
                    "revision":display_history.as_ref().map(|h|h.revision),
                    "error":entry.session.chat_error().or_else(||view.last_error.clone())}),
                );
            }
            if field("id")? != entry.id {
                return Err("The session changed. Please try again.".into());
            }
            let scope = (host, project, entry.id.as_str());
            if op == "attach" {
                if !entry.session.chat_attachments_supported() {
                    return Err("This host can't receive files right now. Check the connection or update Connecto on the host.".into());
                }
                return attachments::stage(scope, Path::new(field("path")?));
            }
            if matches!(op, "send" | "key") && entry.session.chat_history().is_none() {
                return Err("Wait for project chat to connect before sending.".into());
            }
            let chat_input = |bytes| {
                entry
                    .session
                    .send_chat_input(bytes)
                    .map_err(|e| e.to_string())
            };
            match op {
                "resume" if matches!(status, "Stopped" | "Failed") => control.restart(&entry.id)?,
                // A fresh start: the agent and its history go; the app then
                // offers the agent chooser again.
                "clear" => {
                    control.close(&entry.id)?;
                }
                "stop" if status == "Running" => entry
                    .session
                    .send_input(vec![27])
                    .map_err(|e| e.to_string())?,
                "key" if prompt.is_some() && prompt == v["question"].as_str() => {
                    let bytes = match field("key")? {
                        "up" => b"\x1b[A".as_slice(),
                        "down" => b"\x1b[B",
                        "enter" => b"\r",
                        "tab" => b"\t",
                        "space" => b" ",
                        "escape" => b"\x1b",
                        _ => return Err("Unknown choice".into()),
                    };
                    chat_input(bytes.to_vec())?;
                }
                "send"
                    if matches!(status, "Ready" | "Needs an answer")
                        && prompt == v["question"].as_str()
                        && (prompt.is_none() || text_answer) =>
                {
                    let text = field("text")?;
                    let files = attachments::resolve(scope, &v["attachments"])?;
                    if prompt.is_some() && !files.is_empty() {
                        return Err("Answer the question before sending attachments.".into());
                    }
                    if (text.trim().is_empty() && files.is_empty())
                        || text.len() > 60000
                        || text
                            .chars()
                            .any(|c| c.is_control() && c != '\n' && c != '\t')
                    {
                        return Err("Enter a message of up to 60,000 bytes.".into());
                    }
                    if !snap.modes.bracketed_paste && text.contains('\n') {
                        return Err("This prompt accepts one line at a time.".into());
                    }
                    let mut modes = snap.modes;
                    let text_with_attachments;
                    let text = if files.is_empty() {
                        text
                    } else {
                        let paths = files
                            .iter()
                            .map(|file| {
                                entry.session.upload_chat_attachment(
                                    project,
                                    &file.token,
                                    &file.name,
                                    &file.bytes,
                                )
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        let current = control.view();
                        let live = current
                            .terminals
                            .iter()
                            .find(|candidate| {
                                candidate.id == entry.id
                                    && Arc::ptr_eq(&candidate.session, &entry.session)
                            })
                            .ok_or("The session changed. Please try again.")?;
                        let (live_snap, display) = screen(live);
                        let live_dialog = current_question(live, &display);
                        let live_prompt = live_dialog.as_ref().map(|q| q.text.as_str());
                        if live_prompt != prompt
                            || !live_dialog.as_ref().map_or_else(
                                || matches!(crate::status(live.state), "Ready" | "Needs an answer"),
                                |question| question.accepts_text,
                            )
                        {
                            return Err("The session changed. Please try again.".into());
                        }
                        modes = live_snap.modes;
                        text_with_attachments = attachments::message(text, &paths)?;
                        if text_with_attachments.len() > 60000 {
                            return Err(
                                "Shorten the message to send it with the attachments.".into()
                            );
                        }
                        if !modes.bracketed_paste && text.contains('\n') {
                            return Err("This prompt accepts one line at a time.".into());
                        }
                        &text_with_attachments
                    };
                    chat_input(connecto_core::encode_paste(text, modes))?;
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    if let Err(reason) = chat_input(vec![13]) {
                        if files.is_empty() {
                            return Err(reason);
                        }
                        return Err("The message may already be typed in the agent's prompt. Check it on the host before retrying.".into());
                    }
                    attachments::clear_sent(scope, &files);
                }
                _ => return Err("The session changed. Check its current state.".into()),
            }
        }
    }
    Ok(Value::Null)
}

/// The caller owns a NUL-terminated UTF-8 request for this call, and releases
/// the returned allocation exactly once using connecto_free.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn connecto_call(raw: *const c_char) -> *mut c_char {
    let response = std::panic::catch_unwind(|| {
        if raw.is_null() {
            return Err("Empty request".into());
        }
        let v: Value = serde_json::from_slice(unsafe { CStr::from_ptr(raw) }.to_bytes())
            .map_err(|e| e.to_string())?;
        request(&v)
    })
    .unwrap_or_else(|_| Err("Peer operation failed".into()));
    let body = match response {
        Ok(data) => json!({"data":data}),
        Err(err) => json!({"error":err}),
    };
    CString::new(body.to_string()).map_or(std::ptr::null_mut(), CString::into_raw)
}

#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn connecto_free(p: *mut c_char) {
    if !p.is_null() {
        drop(unsafe { CString::from_raw(p) });
    }
}

// iroh's Android DNS resolver and interface monitor need the JVM and an
// Application context before the first endpoint is created.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_studio_connecto_connecto_1flutter_1app_PeerApplication_initializePeer(
    env: jni::JNIEnv<'_>,
    application: jni::objects::JObject<'_>,
) -> jni::sys::jboolean {
    static CONTEXT: OnceLock<jni::objects::GlobalRef> = OnceLock::new();
    let (Ok(vm), Ok(context)) = (env.get_java_vm(), env.new_global_ref(application)) else {
        return jni::sys::JNI_FALSE;
    };
    CONTEXT.get_or_init(|| {
        // Application.onCreate runs before Flutter starts. Keep the global
        // reference alive for as long as the network worker can use it.
        unsafe {
            ndk_context::initialize_android_context(
                vm.get_java_vm_pointer().cast(),
                context.as_obj().as_raw().cast(),
            )
        };
        context
    });
    jni::sys::JNI_TRUE
}

#[cfg(test)]
mod terminals_tests {
    use super::*;
    use connecto_net::hosting::activity_byte;
    use connecto_net::remote_terminals::roster;
    use connecto_net::wire::{BackgroundInfo, TerminalInfo};
    use std::time::Instant;

    fn info(
        id: u32,
        entry: &str,
        name: &str,
        parent: Option<u32>,
        state: TermActivity,
    ) -> TerminalInfo {
        TerminalInfo {
            id,
            name: name.into(),
            tool: 1,
            running: true,
            activity: activity_byte(state),
            cols: 80,
            rows: 24,
            project_id: "p".into(),
            project_name: "connecto".into(),
            parent,
            background: Vec::new(),
            entry: Some(entry.into()),
            owner: None,
        }
    }

    #[test]
    fn the_status_snapshot_says_what_each_terminal_does() {
        // As the host lists them: wire numbers, each with the host's own id.
        let lead = info(7, "claude-23", "claude-23", None, TermActivity::Working);
        let mut coder = info(8, "claude-62", "coder", Some(7), TermActivity::Idle);
        coder.background.push(BackgroundInfo {
            id: "b1".into(),
            kind: "shell".into(),
            label: "cargo test".into(),
            description: Some("runs the tests".into()),
            seconds: 4,
        });
        // A person's own terminal, asked for by no agent.
        let mine = info(9, "claude-75", "claude-75", None, TermActivity::NeedsUser);
        // Asked for by the coder, filed under the lead: it belongs to the coder.
        let mut helper = info(10, "claude-80", "check", Some(7), TermActivity::Idle);
        helper.owner = Some(8);
        // Another project's terminal is not this project's.
        let mut elsewhere = info(11, "claude-2", "claude-2", None, TermActivity::Idle);
        elsewhere.project_id = "q".into();
        let listed = [
            info(1, "claude-1", APP_SESSION, None, TermActivity::Working),
            coder,
            lead,
            mine,
            helper,
            elsewhere,
        ];
        let v = terminals(&roster(&listed, Instant::now(), "p"));
        let rows = v["terminals"].as_array().unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[3]["id"], "claude-75");
        assert_eq!(rows[3]["state"], "waiting for the user");
        assert_eq!(rows[3]["startedBy"], Value::Null);
        assert_eq!(rows[4]["startedBy"], "claude-62", "the agent that asked");
        assert!(
            rows.iter().all(|r| r.get("workingOn").is_none()),
            "no window text"
        );
        assert_eq!(rows[0]["state"], "working");
        assert_eq!(rows[0]["inTurn"], true);
        assert_eq!(rows[0]["appSession"], true);
        assert_eq!(rows[0]["tool"], "Claude");
        assert_eq!(rows[1]["id"], "claude-62");
        assert_eq!(rows[1]["name"], "coder");
        assert_eq!(rows[1]["startedBy"], "claude-23");
        assert_eq!(rows[1]["appSession"], false);
        assert_eq!(rows[1]["state"], "idle");
        assert_eq!(rows[1]["backgroundJobs"], 1);
        // A job's command and description stay on the phone.
        let text = v.to_string();
        assert!(!text.contains("cargo test") && !text.contains("runs the tests"));
    }
}

#[cfg(test)]
mod voice_tests {
    use super::*;

    /// On a host that cannot write conditionally, a question is shown and
    /// nothing of it is offered to the voice.
    #[test]
    fn an_older_hosts_question_is_shown_for_a_tap_only() {
        let row = shown_only(json!({
            "kind": "approval",
            "voiceText": true,
            "options": [{"index": 0, "label": "Yes", "tapOnly": false}],
        }));
        assert_eq!(row["voiceText"], false);
        assert_eq!(row["options"][0]["tapOnly"], true);
    }
}

#[cfg(test)]
mod siri_tests {
    use super::*;

    fn notify() -> Value {
        json!({
            "deviceToken": "ab12", "key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "env": "sandbox", "topic": "studio.connecto.connectoFlutterApp", "locale": "it",
        })
    }

    #[test]
    fn a_siri_ask_carries_the_phones_fields_as_the_host_reads_them() {
        let v = json!({"requestId": "r-1", "id": "7", "text": "ciao", "question": null, "notify": notify()});
        let Some(ProjectAsk::SiriAsk {
            request_id,
            terminal,
            text,
            question,
            notify: Some(to),
        }) = siri_ask("siriAsk", &v).unwrap()
        else {
            panic!("not a Siri ask with a destination")
        };
        assert_eq!(
            (request_id.as_str(), terminal, text.as_str(), question),
            ("r-1", 7, "ciao", None)
        );
        assert_eq!(to.device_token, "ab12");
        assert_eq!(to.locale, "it");
        // Without a destination, nothing is promised.
        let mut plain = v.clone();
        plain["notify"] = Value::Null;
        assert!(matches!(
            siri_ask("siriAsk", &plain).unwrap(),
            Some(ProjectAsk::SiriAsk { notify: None, .. })
        ));
        // A terminal that is not the wire's number is a changed session.
        let mut named = v;
        named["id"] = json!("claude-3");
        assert!(siri_ask("siriAsk", &named).is_err());
        assert!(siri_ask("chat", &json!({})).unwrap().is_none());
    }

    #[test]
    fn a_push_registration_names_the_token_it_replaces() {
        let mut v = notify();
        v["oldDeviceToken"] = json!("cd34");
        let Some(ProjectAsk::PushRegister {
            old_device_token,
            to,
        }) = siri_ask("pushRegister", &v).unwrap()
        else {
            panic!("not a push registration")
        };
        assert_eq!(old_device_token.as_deref(), Some("cd34"));
        assert_eq!(to.device_token, "ab12");
    }
}

#[cfg(test)]
mod peer_tests {
    use super::*;
    use connecto_net::trust::{KnownHost, KnownHosts};

    #[test]
    fn forget_removes_a_saved_offline_host_through_flutter_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut known = KnownHosts::default();
        known
            .add(KnownHost {
                name: "Offline Host".into(),
                id: [17; 32],
                relay: None,
                addrs: Vec::new(),
                paired_at: String::new(),
            })
            .unwrap();
        known.save(&dir.path().join("known_hosts")).unwrap();
        request(&json!({"op": "init", "dir": dir.path()})).unwrap();
        assert_eq!(
            request(&json!({"op": "state"})).unwrap()["hosts"][0]["name"],
            "Offline Host"
        );
        assert_eq!(
            request(&json!({"op": "forget", "host": "Offline Host"})).unwrap(),
            Value::Null
        );
        assert!(
            request(&json!({"op": "state"})).unwrap()["hosts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            KnownHosts::load(&dir.path().join("known_hosts"))
                .unwrap()
                .get("Offline Host")
                .is_none()
        );
    }
}
