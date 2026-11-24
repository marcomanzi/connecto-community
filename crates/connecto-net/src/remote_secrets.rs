//! A host project's secrets (ADR 0027), listed and changed from a paired
//! machine (ADR 0017), and the requests its terminals make for one.
//!
//! Only the project's own secrets: never the app-wide ones, never another
//! project's. A value goes one way, from the person typing it to the host's
//! vault, inside the peer link (QUIC with TLS 1.3 between the two machines'
//! keys; a relay passes packets it cannot read). Nothing the host sends back
//! holds one: the list has names, descriptions, uses and an SSH key's public
//! half, and an action's answer is empty or the vault's refusal, which never
//! names the value. A [`SecretValue`] prints as `…` in every debug output, so
//! no log line or audit record that formats an ask can show it; the audit
//! records the action and the secret's name.
//!
//! An SSH key a peer asks for is generated on the host and kept there; the
//! peer sees its public half only.

use std::sync::Arc;
use std::time::Duration;

use connecto_core::vault::{self, MAX_DESCRIPTION, MAX_NAME, MAX_REASON};
use connecto_core::{
    AskAnswer, AskKind, Binding, ChangeError, GeneratedKey, ProviderScope, SecretAsk, SecretEntry,
    VaultChange, VaultControl, VaultView,
};
use serde_json::{Value, json};

use crate::project_cache::{PanelKind, PanelValue};
use crate::project_panels::ProjectAsk;
use crate::reconnect::HostHandle;

/// The most bindings one secret has.
const MAX_BINDINGS: usize = 8;
/// The most programs a command binding names, and the longest text of any
/// field of a binding.
const MAX_PROGRAMS: usize = 32;
const MAX_FIELD: usize = 2048;
/// The most bytes of the secrets one page carries, as sent: well within an
/// answer ([`crate::project_panels::MAX_ANSWER`]).
const PAGE_BYTES: usize = 1024 * 1024;
/// The most bytes of secrets a peer reads of one project, over all pages:
/// a bound on what a host makes this machine hold, hundreds of thousands
/// of secrets. Past it, what was read is shown and the section says so.
pub const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
/// What the section says when a project keeps more than that.
pub const MORE_THAN_SHOWN: &str = "That project keeps more secrets than are shown here at once: Next secrets shows the ones after these.";
/// The most secrets one page carries, as read.
const MAX_PAGE_ENTRIES: usize = 100_000;
/// What a host whose vault has a problem says of it: the problem itself
/// may name other projects' and app-wide secrets.
pub const HOST_PROBLEM: &str = "Connecto on that machine cannot open some of its secrets. Open Settings > Secrets there to see which.";
/// The most requests one probe answer carries (the host keeps at most 20).
pub const MAX_ASKS: usize = 32;
/// Longest terminal id a paired host's request is shown with.
const MAX_TERMINAL_ID: usize = 120;
/// The most bytes of a request's proposed uses, as sent: with the host's
/// 20 waiting requests at most, every one fits in one answer
/// ([`crate::wire::MAX_SECRET_ASKS`]).
const MAX_PROPOSED: usize = 4 * 1024;
/// The longest public key shown.
const MAX_PUBLIC: usize = 16 * 1024;

/// A secret's value on its way to the host. It never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(…)")
    }
}

/// What a peer asks of one host project's secrets. Each is the project's
/// own: the host puts it in that project, whatever else it could name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsAction {
    /// Add a secret or replace its value (and description).
    Put {
        name: String,
        description: String,
        value: SecretValue,
    },
    /// Set how terminals may use it.
    SetBindings {
        name: String,
        bindings: Vec<Binding>,
    },
    Delete {
        name: String,
    },
    /// Make an SSH key pair on the host and keep it as `name`, used through
    /// the ssh-agent; the private half never leaves the host.
    GenerateKey {
        name: String,
        description: String,
    },
    /// Answer a request of one of the project's terminals.
    Answer {
        ask: u64,
        answer: RemoteAnswer,
    },
}

/// A peer's answer to a terminal's request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAnswer {
    Provide {
        value: SecretValue,
        bindings: Vec<Binding>,
    },
    /// An SSH key made and kept on the host, used only through its
    /// ssh-agent.
    GenerateKey,
    Decline,
}

impl SecretsAction {
    /// The action the host's guard records it as.
    #[must_use]
    pub const fn action(&self) -> &'static str {
        match self {
            Self::Put { .. } => "project-secret-put",
            Self::SetBindings { .. } => "project-secret-uses",
            Self::Delete { .. } => "project-secret-delete",
            Self::GenerateKey { .. } => "project-secret-generate",
            Self::Answer { .. } => "project-secret-answer",
        }
    }

    /// What the audit names besides the project: the secret, or the
    /// request. Never the value.
    #[must_use]
    pub fn audited(&self) -> String {
        match self {
            Self::Put { name, .. }
            | Self::SetBindings { name, .. }
            | Self::Delete { name }
            | Self::GenerateKey { name, .. } => format!("secret:{name}"),
            Self::Answer { ask, .. } => format!("secret-request:{ask}"),
        }
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Put {
                name,
                description,
                value,
            } => {
                json!({ "do": "put", "name": name, "description": description, "value": value.expose() })
            }
            Self::SetBindings { name, bindings } => {
                json!({ "do": "uses", "name": name, "bindings": bindings_json(bindings) })
            }
            Self::Delete { name } => json!({ "do": "delete", "name": name }),
            Self::GenerateKey { name, description } => {
                json!({ "do": "generate", "name": name, "description": description })
            }
            Self::Answer { ask, answer } => {
                let answer = match answer {
                    RemoteAnswer::Provide { value, bindings } => json!({
                        "give": "value", "value": value.expose(), "bindings": bindings_json(bindings)
                    }),
                    RemoteAnswer::GenerateKey => json!({ "give": "generated" }),
                    RemoteAnswer::Decline => json!({ "give": "nothing" }),
                };
                json!({ "do": "answer", "ask": ask, "answer": answer })
            }
        }
    }

    /// `None` when it is not an action this side knows, or not well formed.
    #[must_use]
    pub fn from_json(v: &Value) -> Option<Self> {
        let name = || {
            v.get("name")
                .and_then(Value::as_str)
                .filter(|n| vault::check_name(n).is_ok())
                .map(str::to_owned)
        };
        let description = || {
            v.get("description")
                .and_then(Value::as_str)
                .filter(|d| d.chars().count() <= MAX_DESCRIPTION)
                .map(str::to_owned)
        };
        let value = |v: &Value| {
            v.get("value")
                .and_then(Value::as_str)
                .filter(|value| value.len() <= vault::MAX_VALUE)
                .map(|value| SecretValue(value.to_owned()))
        };
        Some(match v.get("do")?.as_str()? {
            "put" => Self::Put {
                name: name()?,
                description: description()?,
                value: value(v)?,
            },
            "uses" => Self::SetBindings {
                name: name()?,
                bindings: bindings_from(v.get("bindings")?)?,
            },
            "delete" => Self::Delete { name: name()? },
            "generate" => Self::GenerateKey {
                name: name()?,
                description: description()?,
            },
            "answer" => {
                let a = v.get("answer")?;
                Self::Answer {
                    ask: v.get("ask")?.as_u64()?,
                    answer: match a.get("give")?.as_str()? {
                        "value" => RemoteAnswer::Provide {
                            value: value(a)?,
                            bindings: bindings_from(a.get("bindings")?)?,
                        },
                        "generated" => RemoteAnswer::GenerateKey,
                        "nothing" => RemoteAnswer::Decline,
                        _ => return None,
                    },
                }
            }
            _ => return None,
        })
    }
}

// ---- bindings ----

fn bindings_json(bindings: &[Binding]) -> Value {
    Value::Array(bindings.iter().map(binding_json).collect())
}

fn binding_json(binding: &Binding) -> Value {
    match binding {
        Binding::Http {
            base_url,
            header,
            prefix,
        } => json!({ "kind": "http", "base_url": base_url, "header": header, "prefix": prefix }),
        Binding::Command { programs, env } => {
            json!({ "kind": "command", "programs": programs, "env": env })
        }
        Binding::Ssh => json!({ "kind": "ssh" }),
        Binding::AgenticWorks => json!({ "kind": "agentic_works" }),
        Binding::GoogleServiceAccount => json!({ "kind": "google_service_account" }),
    }
}

/// Bindings as sent, or `None` when any is not one this side knows. Their
/// fields are checked where they are kept (the host's vault).
fn bindings_from(v: &Value) -> Option<Vec<Binding>> {
    let list = v.as_array()?;
    if list.len() > MAX_BINDINGS {
        return None;
    }
    list.iter().map(binding_from).collect()
}

fn binding_from(v: &Value) -> Option<Binding> {
    let text = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|t| t.len() <= MAX_FIELD)
            .map(str::to_owned)
    };
    Some(match v.get("kind")?.as_str()? {
        "http" => Binding::Http {
            base_url: text("base_url")?,
            header: text("header")?,
            prefix: text("prefix")?,
        },
        "command" => {
            let programs = v.get("programs")?.as_array()?;
            if programs.len() > MAX_PROGRAMS {
                return None;
            }
            Binding::Command {
                programs: programs
                    .iter()
                    .map(|p| {
                        p.as_str()
                            .filter(|p| p.len() <= MAX_FIELD)
                            .map(str::to_owned)
                    })
                    .collect::<Option<_>>()?,
                env: text("env")?,
            }
        }
        "ssh" => Binding::Ssh,
        "agentic_works" => Binding::AgenticWorks,
        "google_service_account" => Binding::GoogleServiceAccount,
        _ => return None,
    })
}

// ---- the list ----

/// A host project's secrets as the peer was sent them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretsAnswer {
    /// Each with the scope `Project("")`: whoever shows it names the project.
    pub entries: Vec<SecretEntry>,
    /// Where the host keeps values, in its words.
    pub protection: String,
    /// Why the host's vault cannot be read or changed now.
    pub problem: Option<String>,
    /// The secrets shown begin after this name (`None`: from the first).
    pub from: Option<String>,
    /// More follow after the last one shown: the name to go on from.
    pub next: Option<String>,
}

/// The project's own secrets in `view`, for a peer, in name order after
/// `after`, as many as one page holds (at least one): names, descriptions,
/// uses, times, and an SSH key's public half. Never a value, and never
/// what the vault says of another scope's secrets.
#[must_use]
pub fn secrets_json(
    view: &VaultView,
    project: &str,
    vault: &dyn VaultControl,
    after: Option<&str>,
) -> Vec<u8> {
    let scope = ProviderScope::Project(project.to_owned());
    let mut mine: Vec<&SecretEntry> = view
        .entries
        .iter()
        .filter(|e| e.scope == scope && after.is_none_or(|a| e.name.as_str() > a))
        .collect();
    mine.sort_by(|a, b| a.name.cmp(&b.name));
    let mut entries: Vec<Value> = Vec::new();
    let mut bytes = 0;
    let mut more = false;
    for e in mine {
        let entry = {
            let public = e
                .bindings
                .iter()
                .any(|b| matches!(b, Binding::Ssh))
                .then(|| vault.public_key(&e.scope, &e.name))
                .flatten();
            json!({
                "name": e.name,
                "description": e.description,
                "bindings": bindings_json(&e.bindings),
                "updated": e.updated_unix,
                "last_used": e.last_used_unix,
                "public_key": public,
            })
        };
        let size = entry.to_string().len();
        if !entries.is_empty() && bytes + size > PAGE_BYTES {
            more = true;
            break;
        }
        bytes += size;
        entries.push(entry);
    }
    let problem = view.problem.as_ref().map(|_| HOST_PROBLEM);
    json!({
        "entries": entries,
        "more": more,
        "protection": view.protection,
        "problem": problem,
    })
    .to_string()
    .into_bytes()
}

/// A host project's secrets, read page by page with `ask` to the end.
///
/// # Errors
/// The host's refusal; a page out of name order, or an empty page that
/// says more follow.
pub fn fetch_secrets(
    ask: impl FnMut(&ProjectAsk) -> Result<Vec<u8>, String>,
) -> Result<SecretsAnswer, String> {
    fetch_secrets_within(ask, MAX_TOTAL_BYTES, None)
}

/// As [`fetch_secrets`], beginning after the name `from`, and reading at
/// most `limit` bytes: past it, what was read is kept, [`MORE_THAN_SHOWN`]
/// is said, and [`SecretsAnswer::next`] is where to go on from. So every
/// secret is reachable however many there are, within a bound on memory.
///
/// # Errors
/// As [`fetch_secrets`].
pub fn fetch_secrets_within(
    mut ask: impl FnMut(&ProjectAsk) -> Result<Vec<u8>, String>,
    limit: usize,
    from: Option<String>,
) -> Result<SecretsAnswer, String> {
    let mut after = from.clone();
    let mut all = SecretsAnswer {
        from,
        ..SecretsAnswer::default()
    };
    let mut read = 0;
    loop {
        let bytes = ask(&ProjectAsk::Secrets {
            after: after.clone(),
        })?;
        read += bytes.len();
        let (page, more) = secrets_page_from(&bytes)?;
        let ascending = page.entries.windows(2).all(|w| w[0].name < w[1].name)
            && page
                .entries
                .first()
                .is_none_or(|e| after.as_deref().is_none_or(|a| e.name.as_str() > a));
        if !ascending {
            return Err("that machine sent its secrets out of order".to_owned());
        }
        if more && page.entries.is_empty() {
            return Err("that machine sent an empty page of secrets".to_owned());
        }
        after = page.entries.last().map(|e| e.name.clone());
        all.protection = page.protection;
        all.problem = page.problem;
        all.entries.extend(page.entries);
        if !more {
            return Ok(all);
        }
        if read >= limit {
            all.problem = Some(MORE_THAN_SHOWN.to_owned());
            all.next = after;
            return Ok(all);
        }
    }
}

/// # Errors
/// When the bytes are not a list of secrets.
pub fn secrets_from(bytes: &[u8]) -> Result<SecretsAnswer, String> {
    secrets_page_from(bytes).map(|(page, _)| page)
}

/// One page of secrets, and whether more follow.
///
/// # Errors
/// When the bytes are not a list of secrets.
pub fn secrets_page_from(bytes: &[u8]) -> Result<(SecretsAnswer, bool), String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| "not a list of secrets".to_owned())?;
    let text = |v: &Value, key: &str, max: usize| {
        v.get(key).and_then(Value::as_str).map(|t| {
            crate::project_panels::clean_text(t)
                .chars()
                .take(max)
                .collect::<String>()
        })
    };
    let entries = v
        .get("entries")
        .and_then(Value::as_array)
        .ok_or("not a list of secrets")?
        .iter()
        .take(MAX_PAGE_ENTRIES)
        .filter_map(|e| {
            let name = e
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| vault::check_name(n).is_ok())?;
            Some(SecretEntry {
                name: name.to_owned(),
                scope: ProviderScope::Project(String::new()),
                description: crate::remote_terminals::display_text(
                    &text(e, "description", MAX_DESCRIPTION).unwrap_or_default(),
                ),
                bindings: e
                    .get("bindings")
                    .and_then(bindings_from)
                    .unwrap_or_default(),
                updated_unix: e.get("updated").and_then(Value::as_u64).unwrap_or(0),
                last_used_unix: e.get("last_used").and_then(Value::as_u64),
                public_key: text(e, "public_key", MAX_PUBLIC)
                    .map(|k| k.trim().to_owned())
                    .filter(|k| k.starts_with("ssh-") && !k.contains('\n')),
            })
        })
        .collect();
    Ok((
        SecretsAnswer {
            entries,
            protection: text(&v, "protection", 400).unwrap_or_default(),
            problem: text(&v, "problem", 1000).filter(|p| !p.trim().is_empty()),
            ..SecretsAnswer::default()
        },
        v.get("more").and_then(Value::as_bool).unwrap_or(false),
    ))
}

// ---- the host's side ----

/// The words a refusal of a request that is not the project's says.
pub const NOT_WAITING: &str = "That request is no longer waiting.";

/// A peer's ask of `project`'s secrets (`project_name` is how the host's
/// person knows it), answered through the vault the window uses. Every
/// change is the project's own, and the host's person is told of it.
///
/// # Errors
/// The vault's refusal, which never holds the value.
pub fn answer_secrets(
    vault: &dyn VaultControl,
    project: &str,
    project_name: &str,
    ask: &ProjectAsk,
) -> Result<Vec<u8>, String> {
    let scope = ProviderScope::Project(project.to_owned());
    let action = match ask {
        ProjectAsk::Secrets { after } => {
            return Ok(secrets_json(
                &vault.view(),
                project,
                vault,
                after.as_deref(),
            ));
        }
        ProjectAsk::SecretsAct(action) => action,
        _ => return Err(crate::project_panels::UNKNOWN_ASK.to_owned()),
    };
    // The secret the action touches, for the host's notice of a change
    // that failed part way.
    let touched = match action {
        SecretsAction::Put { name, .. }
        | SecretsAction::SetBindings { name, .. }
        | SecretsAction::Delete { name }
        | SecretsAction::GenerateKey { name, .. } => Some(name.clone()),
        SecretsAction::Answer { ask, .. } => vault
            .view()
            .asks
            .iter()
            .find(|a| a.id == *ask && a.project.as_deref() == Some(project))
            .map(|a| a.name.clone()),
    };
    match carry_out(vault, project, project_name, &scope, action) {
        Ok(Some(told)) => {
            vault.note_remote_change(told);
            Ok(crate::project_panels::done_json())
        }
        Ok(None) => Ok(crate::project_panels::done_json()),
        Err(error) => {
            // The vault says, from inside the change, whether anything was
            // written: then the host's person is told, as of any change.
            if let (true, Some(name)) = (error.partial, &touched) {
                vault.note_remote_change(format!(
                    "A paired machine's change to the secret {name} of {project_name} did not finish. Look at it in Settings > Secrets: a secret whose use was being changed is usable by no terminal until its use is set again."
                ));
            }
            // A refusal of the change itself names only what the change
            // named; the vault's own failure is told in fixed words, so the
            // reply never depends on other secrets.
            Err(if error.invalid {
                error.why
            } else {
                VAULT_TROUBLE.to_owned()
            })
        }
    }
}

/// What a peer is told when the host's vault itself failed: its own words
/// may name other projects' or app-wide secrets, or its files.
pub const VAULT_TROUBLE: &str = "Connecto on that machine cannot change its secrets now. Open Settings > Secrets there to see why.";

/// Carry out `action` in `scope`: what to tell the host's person, or `None`
/// when nothing changed.
fn carry_out(
    vault: &dyn VaultControl,
    project: &str,
    project_name: &str,
    scope: &ProviderScope,
    action: &SecretsAction,
) -> Result<Option<String>, ChangeError> {
    let scope = scope.clone();
    let had = |name: &str| {
        vault
            .view()
            .entries
            .iter()
            .any(|e| e.scope == scope && e.name == name)
    };
    let told = match action {
        SecretsAction::Put {
            name,
            description,
            value,
        } => {
            let verb = if had(name) { "replaced" } else { "added" };
            vault.change(VaultChange::Put {
                scope: scope.clone(),
                name: name.clone(),
                description: description.clone(),
                value: value.expose().to_owned(),
            })?;
            format!("A paired machine {verb} the secret {name} of {project_name}.")
        }
        SecretsAction::SetBindings { name, bindings } => {
            if !had(name) {
                return Err(ChangeError::invalid(format!(
                    "There is no secret named {name} in this project."
                )));
            }
            vault.change(VaultChange::SetBindings {
                scope: scope.clone(),
                name: name.clone(),
                bindings: bindings.clone(),
            })?;
            format!("A paired machine changed how the secret {name} of {project_name} is used.")
        }
        SecretsAction::Delete { name } => {
            if !had(name) {
                return Ok(None);
            }
            vault.change(VaultChange::Delete {
                scope: scope.clone(),
                name: name.clone(),
            })?;
            format!("A paired machine removed the secret {name} of {project_name}.")
        }
        SecretsAction::GenerateKey { name, description } => {
            let verb = if had(name) { "replaced" } else { "added" };
            vault.change(VaultChange::GenerateKey {
                scope: scope.clone(),
                name: name.clone(),
                description: description.clone(),
            })?;
            format!(
                "A paired machine {verb} the secret {name} of {project_name} with an SSH key made here."
            )
        }
        SecretsAction::Answer { ask, answer } => {
            // Only a request of this project's terminals.
            let asked = vault
                .view()
                .asks
                .iter()
                .find(|a| a.id == *ask && a.project.as_deref() == Some(project))
                .map(|a| a.name.clone())
                .ok_or_else(|| ChangeError::invalid(NOT_WAITING.to_owned()))?;
            let (answer, verb) = match answer {
                RemoteAnswer::Provide { value, bindings } => (
                    AskAnswer::Provide {
                        scope: scope.clone(),
                        value: value.expose().to_owned(),
                        bindings: bindings.clone(),
                    },
                    "set",
                ),
                RemoteAnswer::GenerateKey => (
                    AskAnswer::GenerateSshKey {
                        scope: scope.clone(),
                    },
                    "set, with an SSH key made here,",
                ),
                RemoteAnswer::Decline => (AskAnswer::Decline, "declined"),
            };
            vault.change(VaultChange::Answer { ask: *ask, answer })?;
            format!(
                "A paired machine {verb} the secret {asked} a terminal of {project_name} asked for."
            )
        }
    };
    Ok(Some(told))
}

// ---- requests, as a peer is told of them ----

/// The requests waiting in `projects` (the connection's latest list), for a
/// peer. A request of a terminal outside them is never sent.
#[must_use]
pub fn asks_json(asks: &[SecretAsk], projects: &[String]) -> Vec<u8> {
    let mut bytes = 2;
    let list: Vec<Value> = asks
        .iter()
        .filter(|a| a.project.as_ref().is_some_and(|p| projects.contains(p)))
        .take(MAX_ASKS)
        .map(|a| {
            // Proposed uses too long to send are left out: the person sets
            // the use in the dialog. So a request always fits.
            let proposed = bindings_json(&a.proposed);
            let omitted = a.proposed_omitted || proposed.to_string().len() > MAX_PROPOSED;
            let proposed = if omitted {
                Value::Array(Vec::new())
            } else {
                proposed
            };
            json!({
                "id": a.id,
                "terminal": a.terminal,
                "project": a.project,
                "name": a.name,
                "kind": match a.kind {
                    AskKind::Token => "token",
                    AskKind::Password => "password",
                    AskKind::SshKey => "ssh-key",
                },
                "reason": a.reason,
                "proposed": proposed,
                "proposed_omitted": omitted,
            })
        })
        .take_while(|a| {
            bytes += a.to_string().len() + 1;
            bytes <= crate::wire::MAX_SECRET_ASKS
        })
        .collect();
    Value::Array(list).to_string().into_bytes()
}

/// The requests as sent; one that is not well formed is left out.
#[must_use]
pub fn asks_from(bytes: &[u8]) -> Vec<SecretAsk> {
    let Ok(Value::Array(list)) = serde_json::from_slice::<Value>(bytes) else {
        return Vec::new();
    };
    list.iter()
        .take(MAX_ASKS)
        .filter_map(|a| {
            let name = a
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| vault::check_name(n).is_ok() && n.len() <= MAX_NAME)?;
            // Checked whole, as the host checks its own terminals' (an older
            // host never did): an empty or generic one is not shown here,
            // and the person answers it on that machine.
            let reason = a.get("reason").and_then(Value::as_str)?;
            vault::check_reason(reason, name).ok()?;
            let proposed = a
                .get("proposed")
                .and_then(bindings_from)
                .unwrap_or_default();
            // As the host checks its own terminals' proposals (an older host
            // never did): one the form would turn into another use is not
            // shown here, and is answered on that machine.
            if !proposed.iter().all(|b| vault::check_proposed(b).is_ok()) {
                return None;
            }
            Some(SecretAsk {
                id: a.get("id")?.as_u64()?,
                terminal: vault::display_line(
                    a.get("terminal").and_then(Value::as_str)?,
                    MAX_TERMINAL_ID,
                ),
                project: Some(
                    a.get("project")
                        .and_then(Value::as_str)
                        .filter(|p| crate::remote_terminals::is_canonical_project_id(p))?
                        .to_owned(),
                ),
                name: name.to_owned(),
                kind: a
                    .get("kind")
                    .and_then(Value::as_str)
                    .and_then(AskKind::from_word)
                    .unwrap_or_default(),
                reason: vault::display_line(reason, MAX_REASON),
                proposed,
                // Said, never guessed: an empty list is no proposal.
                proposed_omitted: a
                    .get("proposed_omitted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

// ---- the peer's side ----

/// How long a host project's secrets are shown before they are asked for
/// again while they are on show.
const FRESH: Duration = Duration::from_secs(5);

/// What the section says for a host too old to keep secrets from here.
pub const SECRETS_OLD_HOST: &str = "The Connecto on that machine is too old to keep this project's secrets from here. Update Connecto there to see and change them from here.";

const LOADING: &str = "Asking that machine for this project's secrets…";
const NOT_CONNECTED: &str = "Not connected to that machine.";
const ONLY_PROJECT: &str =
    "From here, only this project's own secrets can be kept on that machine.";

/// One host project's secrets, kept on the host. Shown under `shown_as`,
/// the id the window knows the project by; every change is the host's to
/// make (its guard records it, and Pause refuses it). A value typed here is
/// sent to the host once and never comes back.
pub struct RemoteVault {
    host: Arc<HostHandle>,
    project: String,
    shown_as: String,
}

impl RemoteVault {
    #[must_use]
    pub const fn new(host: Arc<HostHandle>, project: String, shown_as: String) -> Self {
        Self {
            host,
            project,
            shown_as,
        }
    }

    fn is_old_host(&self) -> bool {
        self.host
            .peek_panel(&self.project, PanelKind::Secrets)
            .1
            .as_deref()
            .is_some_and(crate::remote_settings::old_host)
    }

    /// The project's own scope, or why not.
    fn check_scope(&self, scope: &ProviderScope) -> Result<(), String> {
        match scope {
            ProviderScope::Project(id) if *id == self.shown_as => Ok(()),
            _ => Err(ONLY_PROJECT.to_owned()),
        }
    }

    fn send(&self, action: SecretsAction) -> Result<(), String> {
        if !self.host.is_connected() {
            return Err(NOT_CONNECTED.to_owned());
        }
        if self.is_old_host() {
            return Err(SECRETS_OLD_HOST.to_owned());
        }
        let _ = self
            .host
            .project_command(&self.project, ProjectAsk::SecretsAct(action));
        Ok(())
    }
}

impl VaultControl for RemoteVault {
    fn view(&self) -> Arc<VaultView> {
        let (value, error) = self.host.panel(&self.project, PanelKind::Secrets, FRESH);
        let scope = ProviderScope::Project(self.shown_as.clone());
        let asks = self
            .host
            .secret_asks()
            .into_iter()
            .filter(|a| a.project.as_deref() == Some(&self.project))
            .map(|mut a| {
                a.project = Some(self.shown_as.clone());
                a
            })
            .collect();
        if error
            .as_deref()
            .is_some_and(crate::remote_settings::old_host)
        {
            return Arc::new(VaultView {
                problem: Some(SECRETS_OLD_HOST.to_owned()),
                ..VaultView::default()
            });
        }
        let Some(PanelValue::Secrets(answer)) = value else {
            return Arc::new(VaultView {
                problem: Some(error.unwrap_or_else(|| LOADING.to_owned())),
                asks,
                ..VaultView::default()
            });
        };
        Arc::new(VaultView {
            entries: answer
                .entries
                .iter()
                .cloned()
                .map(|mut e| {
                    e.scope = scope.clone();
                    e
                })
                .collect(),
            // A refused change, or a host that stopped answering, is said
            // where the section says its problems.
            problem: error.or_else(|| answer.problem.clone()),
            asks,
            protection: if answer.protection.is_empty() {
                String::new()
            } else {
                format!("On that machine: {}", answer.protection)
            },
            remote_changes: Vec::new(),
            pages: (answer.from.is_some() || answer.next.is_some())
                .then(|| connecto_core::SecretsPage {
                    scope: scope.clone(),
                    from: answer.from.clone(),
                    next: answer.next.clone(),
                })
                .into_iter()
                .collect(),
        })
    }

    fn put(
        &self,
        scope: &ProviderScope,
        name: &str,
        description: &str,
        value: &str,
    ) -> Result<(), String> {
        self.check_scope(scope)?;
        vault::check_name(name)?;
        let value = vault::clean_value(value)?;
        self.send(SecretsAction::Put {
            name: name.to_owned(),
            description: description.trim().to_owned(),
            value: SecretValue(value),
        })
    }

    fn set_bindings(
        &self,
        scope: &ProviderScope,
        name: &str,
        bindings: Vec<Binding>,
    ) -> Result<(), String> {
        self.check_scope(scope)?;
        self.send(SecretsAction::SetBindings {
            name: name.to_owned(),
            bindings,
        })
    }

    fn delete(&self, scope: &ProviderScope, name: &str) -> Result<(), String> {
        self.check_scope(scope)?;
        self.send(SecretsAction::Delete {
            name: name.to_owned(),
        })
    }

    fn answer(&self, ask: u64, answer: AskAnswer) -> Result<(), String> {
        let answer = match answer {
            AskAnswer::Provide {
                scope,
                value,
                bindings,
            } => {
                self.check_scope(&scope)?;
                RemoteAnswer::Provide {
                    value: SecretValue(vault::clean_value(&value)?),
                    bindings,
                }
            }
            AskAnswer::GenerateSshKey { scope } => {
                self.check_scope(&scope)?;
                RemoteAnswer::GenerateKey
            }
            AskAnswer::Decline => RemoteAnswer::Decline,
        };
        self.send(SecretsAction::Answer { ask, answer })
    }

    /// Followed by the command that carried it, so two refusals in the
    /// same words are told apart.
    fn answer_state(&self, ask: u64) -> Option<connecto_core::AnswerState> {
        use connecto_core::AnswerState;
        Some(match self.host.answer_outcome(&self.project, ask)? {
            None => AnswerState::Waiting,
            Some(Ok(())) => AnswerState::Taken,
            Some(Err(why)) => AnswerState::Refused(why),
        })
    }

    fn show_secrets_from(&self, scope: &ProviderScope, from: Option<String>) {
        if self.check_scope(scope).is_ok() {
            self.host.show_secrets_from(&self.project, from);
        }
    }

    /// A key for that machine is made there: see
    /// [`VaultControl::put_generated_key`].
    fn generate_ssh_key(&self, comment: &str) -> Result<GeneratedKey, String> {
        let _ = comment;
        Err("A key for that machine is made there, so its private half never leaves it.".to_owned())
    }

    fn put_generated_key(
        &self,
        scope: &ProviderScope,
        name: &str,
        description: &str,
    ) -> Result<(), String> {
        self.check_scope(scope)?;
        vault::check_name(name)?;
        self.send(SecretsAction::GenerateKey {
            name: name.to_owned(),
            description: description.trim().to_owned(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const VALUE: &str = "sk-live-0123456789abcdef";

    #[test]
    fn an_action_survives_json_and_its_value_never_prints() {
        let actions = [
            SecretsAction::Put {
                name: "GH".into(),
                description: "deploys".into(),
                value: SecretValue(VALUE.into()),
            },
            SecretsAction::SetBindings {
                name: "GH".into(),
                bindings: vec![
                    Binding::Http {
                        base_url: "https://api.github.com".into(),
                        header: "Authorization".into(),
                        prefix: "Bearer ".into(),
                    },
                    Binding::Command {
                        programs: vec!["/opt/homebrew/bin/gh".into()],
                        env: "GH_TOKEN".into(),
                    },
                    Binding::Ssh,
                    Binding::AgenticWorks,
                    Binding::GoogleServiceAccount,
                ],
            },
            SecretsAction::Delete { name: "GH".into() },
            SecretsAction::GenerateKey {
                name: "DEPLOY_KEY".into(),
                description: String::new(),
            },
            SecretsAction::Answer {
                ask: 4,
                answer: RemoteAnswer::Provide {
                    value: SecretValue(VALUE.into()),
                    bindings: vec![Binding::Ssh],
                },
            },
            SecretsAction::Answer {
                ask: 5,
                answer: RemoteAnswer::GenerateKey,
            },
            SecretsAction::Answer {
                ask: 6,
                answer: RemoteAnswer::Decline,
            },
        ];
        for action in actions {
            let ask = ProjectAsk::SecretsAct(action.clone());
            assert_eq!(ProjectAsk::decode(&ask.encode()).unwrap(), ask);
            for printed in [
                format!("{ask:?}"),
                format!("{action:?}"),
                action.audited(),
                ask.audited("p"),
            ] {
                assert!(!printed.contains(VALUE), "{printed}");
            }
        }
    }

    #[test]
    fn a_bad_name_value_or_binding_is_refused() {
        for bad in [
            json!({ "do": "put", "name": "1bad", "description": "", "value": VALUE }),
            json!({ "do": "put", "name": "OK", "description": "", "value": "x".repeat(vault::MAX_VALUE + 1) }),
            json!({ "do": "uses", "name": "OK", "bindings": [{ "kind": "teleport" }] }),
            json!({ "do": "answer", "ask": 1, "answer": { "give": "everything" } }),
            json!({ "do": "nothing" }),
        ] {
            assert_eq!(SecretsAction::from_json(&bad), None, "{bad}");
        }
    }

    /// A vault whose list is fixed; nothing else.
    struct Listed(VaultView);

    impl VaultControl for Listed {
        fn view(&self) -> Arc<VaultView> {
            Arc::new(self.0.clone())
        }
        fn put(&self, _: &ProviderScope, _: &str, _: &str, _: &str) -> Result<(), String> {
            Err("no".into())
        }
        fn set_bindings(&self, _: &ProviderScope, _: &str, _: Vec<Binding>) -> Result<(), String> {
            Err("no".into())
        }
        fn delete(&self, _: &ProviderScope, _: &str) -> Result<(), String> {
            Err("no".into())
        }
    }

    fn entry(name: &str, scope: ProviderScope, bindings: Vec<Binding>) -> SecretEntry {
        SecretEntry {
            name: name.into(),
            scope,
            description: String::new(),
            bindings,
            updated_unix: 0,
            last_used_unix: None,
            public_key: None,
        }
    }

    /// What the host's vault says of its problems may name app-wide and
    /// other projects' secrets: a peer is told only that there is one.
    #[test]
    fn a_vault_problem_naming_other_secrets_is_never_sent() {
        let vault = Listed(VaultView {
            entries: vec![
                entry("MINE", ProviderScope::Project("p".into()), vec![]),
                entry("OTHER_PROJECT_KEY", ProviderScope::Project("q".into()), vec![]),
                entry("APP_WIDE_KEY", ProviderScope::Global, vec![]),
            ],
            problem: Some(
                "These secrets could not be opened, so terminals cannot use them: APP_WIDE_KEY, OTHER_PROJECT_KEY. Set their values again.".into(),
            ),
            ..VaultView::default()
        });
        let bytes = secrets_json(&vault.view(), "p", &vault, None);
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("APP_WIDE_KEY"), "{text}");
        assert!(!text.contains("OTHER_PROJECT_KEY"), "{text}");
        let answer = secrets_from(&bytes).unwrap();
        assert_eq!(answer.entries.len(), 1);
        assert_eq!(answer.problem.as_deref(), Some(HOST_PROBLEM));
    }

    /// Secrets too many for one page come in name order, page after page,
    /// each within the page's size, and are read to the end.
    #[test]
    fn a_long_list_comes_in_pages_and_is_read_to_the_end() {
        let big = Binding::Command {
            programs: (0..MAX_PROGRAMS)
                .map(|n| format!("/{n}/{}", "x".repeat(1900)))
                .collect(),
            env: "TOKEN".into(),
        };
        let vault = Listed(VaultView {
            entries: (0..50)
                .rev()
                .map(|n| {
                    entry(
                        &format!("K{n:02}"),
                        ProviderScope::Project("p".into()),
                        vec![big.clone()],
                    )
                })
                .collect(),
            ..VaultView::default()
        });
        let mut pages = 0;
        let all = fetch_secrets(|ask| {
            let ProjectAsk::Secrets { after } = ask else {
                return Err("not a list".into());
            };
            pages += 1;
            let bytes = secrets_json(&vault.view(), "p", &vault, after.as_deref());
            assert!(bytes.len() < PAGE_BYTES + 128 * 1024, "{}", bytes.len());
            Ok(bytes)
        })
        .unwrap();
        assert!(pages > 1, "{pages}");
        let names: Vec<String> = all.entries.iter().map(|e| e.name.clone()).collect();
        let wanted: Vec<String> = (0..50).map(|n| format!("K{n:02}")).collect();
        assert_eq!(names, wanted);
    }

    fn listed(count: usize) -> Listed {
        Listed(VaultView {
            entries: (0..count)
                .map(|n| {
                    entry(
                        &format!("K{n:05}"),
                        ProviderScope::Project("p".into()),
                        vec![],
                    )
                })
                .collect(),
            ..VaultView::default()
        })
    }

    fn pages(vault: &Listed) -> impl FnMut(&ProjectAsk) -> Result<Vec<u8>, String> + '_ {
        move |ask| {
            let ProjectAsk::Secrets { after } = ask else {
                return Err("not a list".into());
            };
            Ok(secrets_json(&vault.view(), "p", vault, after.as_deref()))
        }
    }

    /// Many more secrets than anyone keeps are all listed, to the last.
    #[test]
    fn ten_thousand_and_one_secrets_are_listed_to_the_last() {
        let vault = listed(10_001);
        let all = fetch_secrets(pages(&vault)).unwrap();
        assert_eq!(all.entries.len(), 10_001);
        assert_eq!(all.entries.last().unwrap().name, "K10000");
        assert_eq!(all.problem, None);
    }

    /// Past what this machine reads of one project at once, what was read
    /// is kept, the section says there are more, and going on from where it
    /// stopped reaches every secret, to the last.
    #[test]
    fn past_the_bound_every_secret_is_reached_by_going_on() {
        let vault = listed(30_000);
        let mut from = None;
        let mut seen = Vec::new();
        let mut steps = 0;
        loop {
            let part = fetch_secrets_within(pages(&vault), 1, from.clone()).unwrap();
            assert_eq!(part.from, from);
            seen.extend(part.entries.iter().map(|e| e.name.clone()));
            steps += 1;
            assert!(steps < 100, "the list never ends");
            match part.next {
                Some(next) => {
                    assert_eq!(part.problem.as_deref(), Some(MORE_THAN_SHOWN));
                    from = Some(next);
                }
                None => break,
            }
        }
        assert!(steps > 1, "{steps}");
        let wanted: Vec<String> = (0..30_000).map(|n| format!("K{n:05}")).collect();
        assert_eq!(seen, wanted, "every secret once, in order");
    }

    /// The host's twenty waiting requests always fit in one answer, however
    /// long their reasons and proposed uses.
    #[test]
    fn every_waiting_request_fits_in_one_answer() {
        let huge = Binding::Command {
            programs: (0..MAX_PROGRAMS)
                .map(|n| format!("/{n}/{}", "x".repeat(1900)))
                .collect(),
            env: "TOKEN".into(),
        };
        let asks: Vec<SecretAsk> = (1..=20)
            .map(|id| SecretAsk {
                id,
                terminal: "claude-1".into(),
                project: Some(PROJECT.into()),
                name: format!("S{id}"),
                kind: AskKind::Token,
                reason: format!("Deploy staging {}", "\u{1F511}".repeat(MAX_REASON - 15)),
                proposed: vec![huge.clone(); MAX_BINDINGS],
                proposed_omitted: false,
            })
            .collect();
        let json = asks_json(&asks, &[PROJECT.into()]);
        let message = crate::wire::Message::SecretAsks(json.clone());
        assert!(message.encode().is_ok(), "{} bytes", json.len());
        let sent = asks_from(&json);
        assert_eq!(sent.len(), 20, "every one");
        assert!(
            sent.iter()
                .all(|a| a.proposed.is_empty() && a.proposed_omitted),
            "said, not guessed"
        );
        // Uses that fit are sent as proposed.
        let small = SecretAsk {
            proposed: vec![Binding::Ssh],
            proposed_omitted: false,
            ..asks[0].clone()
        };
        assert_eq!(
            asks_from(&asks_json(&[small], &[PROJECT.into()]))[0].proposed,
            vec![Binding::Ssh]
        );
    }

    /// A page out of order, or empty and saying more follow, is refused.
    #[test]
    fn a_bad_page_of_secrets_is_refused() {
        let page = |names: &[&str], more: bool| {
            let entries: Vec<Value> = names.iter().map(|n| json!({ "name": n })).collect();
            json!({ "entries": entries, "more": more })
                .to_string()
                .into_bytes()
        };
        let mut sent = vec![page(&["B"], true), page(&["A"], false)].into_iter();
        assert!(fetch_secrets(|_| Ok(sent.next().unwrap())).is_err());
        let mut sent = vec![page(&[], true)].into_iter();
        assert!(fetch_secrets(|_| Ok(sent.next().unwrap())).is_err());
    }

    const PROJECT: &str = "0123456789abcdef";

    /// A vault whose every change fails with `error`; it keeps what it is
    /// told.
    struct Failing {
        view: VaultView,
        error: ChangeError,
        told: std::sync::Mutex<Vec<String>>,
    }

    impl VaultControl for Failing {
        fn view(&self) -> Arc<VaultView> {
            Arc::new(self.view.clone())
        }
        fn put(&self, _: &ProviderScope, _: &str, _: &str, _: &str) -> Result<(), String> {
            Err(self.error.why.clone())
        }
        fn set_bindings(&self, _: &ProviderScope, _: &str, _: Vec<Binding>) -> Result<(), String> {
            Err(self.error.why.clone())
        }
        fn delete(&self, _: &ProviderScope, _: &str) -> Result<(), String> {
            Err(self.error.why.clone())
        }
        fn change(&self, _: VaultChange) -> Result<(), ChangeError> {
            Err(self.error.clone())
        }
        fn note_remote_change(&self, what: String) {
            self.told.lock().unwrap().push(what);
        }
    }

    fn failing(error: ChangeError) -> Failing {
        Failing {
            view: VaultView {
                entries: vec![entry("GH", ProviderScope::Project(PROJECT.into()), vec![])],
                ..VaultView::default()
            },
            error,
            told: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn put_gh() -> ProjectAsk {
        ProjectAsk::SecretsAct(SecretsAction::Put {
            name: "GH".into(),
            description: String::new(),
            value: SecretValue("a-long-value-1234".into()),
        })
    }

    /// The vault's own failure reaches a peer in fixed words, whatever it
    /// says (another project's secret, a file); a refusal of the change
    /// itself is told as it is.
    #[test]
    fn the_vaults_own_failure_is_told_in_fixed_words() {
        for why in [
            "The vault cannot be changed now: OTHER_PROJECT_KEY is there twice",
            "/Users/someone/Library/connecto/secrets/vault.json could not be written: denied",
        ] {
            let vault = failing(ChangeError::storage(why.into(), false));
            let reply = answer_secrets(&vault, PROJECT, "Project", &put_gh()).unwrap_err();
            assert_eq!(reply, VAULT_TROUBLE, "{why}");
        }
        let own = "A secret is at least 8 characters, so that it can be hidden.";
        let vault = failing(ChangeError::invalid(own.into()));
        assert_eq!(
            answer_secrets(&vault, PROJECT, "Project", &put_gh()).unwrap_err(),
            own
        );
    }

    /// The reply to a change never depends on other projects' secrets:
    /// with only another project's names changed, it is the same.
    #[test]
    fn the_reply_is_the_same_whatever_other_projects_keep() {
        let reply = |other: &str| {
            let vault = real_vault();
            vault
                .put(
                    &ProviderScope::Project(PROJECT.into()),
                    "GH",
                    "",
                    "a-long-value-1234",
                )
                .unwrap();
            vault
                .put(
                    &ProviderScope::Project("fedcba9876543210".into()),
                    other,
                    "",
                    "another-value-123",
                )
                .unwrap();
            let ask = ProjectAsk::SecretsAct(SecretsAction::SetBindings {
                name: "GH".into(),
                bindings: vec![Binding::Http {
                    base_url: "ftp://elsewhere".into(),
                    header: "Authorization".into(),
                    prefix: String::new(),
                }],
            });
            answer_secrets(&vault, PROJECT, "Project", &ask).unwrap_err()
        };
        // `needs` is a word of the refusal ("The address needs https://.").
        let first = reply("needs");
        assert_eq!(first, reply("Unrelated"));
        assert_ne!(
            first, VAULT_TROUBLE,
            "a refusal of the change itself: {first}"
        );
    }

    /// A change the vault says wrote something before failing is told to
    /// the host's person; one that wrote nothing is not.
    #[test]
    fn a_change_that_failed_part_way_is_told_to_the_host() {
        let generate = ProjectAsk::SecretsAct(SecretsAction::GenerateKey {
            name: "GH".into(),
            description: String::new(),
        });
        let vault = failing(ChangeError::storage(
            "the list could not be written".into(),
            true,
        ));
        assert!(answer_secrets(&vault, PROJECT, "Project", &generate).is_err());
        let told = vault.told.lock().unwrap().clone();
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("GH of Project did not finish"), "{told:?}");
        let vault = failing(ChangeError::storage(
            "the list could not be written".into(),
            false,
        ));
        assert!(answer_secrets(&vault, PROJECT, "Project", &generate).is_err());
        assert!(vault.told.lock().unwrap().is_empty());
    }

    fn real_vault() -> connecto_vault::Vault {
        connecto_vault::Vault::open(
            Arc::new(connecto_secrets::Secrets::new(
                Arc::new(connecto_secrets::MemoryItems::default()),
                None,
            )),
            Arc::new(connecto_vault::FixedKey([7; 32])),
            None,
        )
    }

    fn ask_in(vault: &connecto_vault::Vault, name: &str, proposed: Vec<Binding>) -> u64 {
        connecto_core::VaultBridge::ask(
            vault,
            &connecto_core::SecretCaller {
                terminal: "claude-1".into(),
                project: Some(PROJECT.into()),
                project_folder: None,
                owns_pid: Arc::new(|_| false),
            },
            connecto_core::AskRequest {
                name: name.into(),
                kind: AskKind::SshKey,
                reason: "to deploy the site to Fly.io".into(),
                proposed,
            },
            Box::new(|_| {}),
        )
        .unwrap()
    }

    /// An answer given before the host started again (a dialog left open,
    /// an answer queued across the reconnect) never meets a new request of
    /// the same project.
    #[test]
    fn an_answer_from_before_a_restart_never_meets_a_new_request() {
        let before = real_vault();
        let old = ask_in(&before, "TOKEN", vec![]);
        let after = real_vault();
        let new = ask_in(&after, "OTHER", vec![]);
        assert_ne!(old, new);
        let ask = ProjectAsk::SecretsAct(SecretsAction::Answer {
            ask: old,
            answer: RemoteAnswer::Provide {
                value: SecretValue("a-long-value-1234".into()),
                bindings: vec![],
            },
        });
        assert_eq!(
            answer_secrets(&after, PROJECT, "Project", &ask).unwrap_err(),
            NOT_WAITING
        );
        assert!(after.view().entries.is_empty());
        assert_eq!(after.view().asks.len(), 1, "the new request still waits");
    }

    /// A key made on the host for a request is used through its ssh-agent
    /// only, whatever uses the request or the peer named: a private key is
    /// never a web header.
    #[test]
    fn a_key_made_for_a_request_is_used_only_through_the_agent() {
        let vault = real_vault();
        let http = Binding::Http {
            base_url: "https://api.example.com".into(),
            header: "Authorization".into(),
            prefix: String::new(),
        };
        let id = ask_in(&vault, "DEPLOY", vec![http.clone(), Binding::Ssh]);
        let raw = json!({
            "ask": "secrets-act",
            "action": {
                "do": "answer",
                "ask": id,
                "answer": { "give": "generated", "bindings": bindings_json(&[http]) },
            },
        })
        .to_string();
        let ask = ProjectAsk::decode(raw.as_bytes()).unwrap();
        answer_secrets(&vault, PROJECT, "Project", &ask).unwrap();
        let entry = vault.view().entries[0].clone();
        assert_eq!(entry.bindings, vec![Binding::Ssh]);
        let listed = secrets_from(&secrets_json(&vault.view(), PROJECT, &vault, None)).unwrap();
        assert!(
            listed.entries[0].public_key.is_some(),
            "the peer sees its public half"
        );
    }

    /// A reason a terminal gives here reaches a paired machine exactly as
    /// it was checked and shown here; one longer than is shown is refused
    /// at once rather than cut, so the peer never drops what this machine
    /// showed.
    #[test]
    fn a_request_reaches_the_peer_as_it_was_checked_here() {
        let vault = real_vault();
        let caller = connecto_core::SecretCaller {
            terminal: "claude-1".into(),
            project: Some(PROJECT.into()),
            project_folder: None,
            owns_pid: Arc::new(|_| false),
        };
        let ask = |name: &str, reason: String| {
            connecto_core::VaultBridge::ask(
                &vault,
                &caller,
                connecto_core::AskRequest {
                    name: name.into(),
                    kind: AskKind::Token,
                    reason,
                    proposed: Vec::new(),
                },
                Box::new(|_| {}),
            )
        };
        // What it is for comes after where it would be cut.
        let overlong = format!("{}deploy the site to Fly.io", "please ".repeat(43));
        assert!(ask("LONG", overlong).is_err(), "refused, not cut");
        let fits = format!(
            "{} then deploy the site to Fly.io",
            "Build the docs and the release notes,".repeat(7)
        );
        assert!(fits.chars().count() <= MAX_REASON);
        ask("FITS", fits.clone()).unwrap();
        let here = connecto_core::VaultControl::view(&vault).asks.clone();
        let there = asks_from(&asks_json(&here, &[PROJECT.into()]));
        assert_eq!(there, here, "every request, as shown here");
        assert_eq!(there[0].reason, fits);
    }

    /// A host's request is shown only with a reason the person can judge,
    /// kept whole up to the reason's own limit, on one line.
    #[test]
    fn a_hosts_request_is_shown_only_with_a_clear_reason_kept_whole() {
        let long = format!(
            "{} then push the tag to github.com/acme/web",
            "Build the release notes and the changelog for 2.0".repeat(3)
        );
        assert!(long.chars().count() > 120 && long.chars().count() <= MAX_REASON);
        let sent = |reason: Value| {
            asks_from(
                json!([{
                    "id": 1,
                    "terminal": "claude-1\u{202e}",
                    "project": PROJECT,
                    "name": "GH",
                    "kind": "token",
                    "reason": reason,
                    "proposed": [],
                }])
                .to_string()
                .as_bytes(),
            )
        };
        for vague in [
            json!(""),
            json!("to deploy"),
            json!("Mi serve per questo lavoro"),
            Value::Null,
        ] {
            assert!(sent(vague.clone()).is_empty(), "{vague}");
        }
        let shown = sent(json!(long.clone()));
        assert_eq!(shown[0].reason, long);
        assert_eq!(shown[0].terminal, "claude-1");
        let quoted = sent(json!("Restart \u{201d}staging\u{2028}PostgreSQL"));
        assert_eq!(quoted[0].reason, "Restart 'staging PostgreSQL");
    }

    /// An older host may send a proposed program path the form would split
    /// into two: the request is not shown here, and is answered there.
    #[test]
    fn a_hosts_proposal_the_form_would_change_is_not_shown() {
        let sent = |program: &str| {
            asks_from(
                json!([{
                    "id": 1,
                    "terminal": "claude-1",
                    "project": PROJECT,
                    "name": "GH",
                    "kind": "token",
                    "reason": "Open pull requests on github.com/acme/web",
                    "proposed": [{"kind": "command", "programs": [program], "env": "GH_TOKEN"}],
                }])
                .to_string()
                .as_bytes(),
            )
        };
        assert!(sent("/opt/homebrew/bin/gh\n/opt/tools/deploy").is_empty());
        assert!(sent("/opt/homebrew/bin/gh ").is_empty());
        let fine = sent("/opt/homebrew/bin/gh");
        assert_eq!(fine.len(), 1);
        assert_eq!(
            fine[0].proposed,
            [Binding::Command {
                programs: vec!["/opt/homebrew/bin/gh".into()],
                env: "GH_TOKEN".into(),
            }]
        );
    }

    #[test]
    fn requests_of_projects_not_in_the_list_are_never_sent() {
        let ask = |id, project: &str| SecretAsk {
            id,
            terminal: "claude-1".into(),
            project: Some(project.into()),
            name: "GH".into(),
            kind: AskKind::SshKey,
            reason: "to push the release".into(),
            proposed: vec![Binding::Ssh],
            proposed_omitted: false,
        };
        let shared = "0123456789abcdef";
        let other = "fedcba9876543210";
        let sent = asks_from(&asks_json(
            &[ask(1, shared), ask(2, other)],
            &[shared.into()],
        ));
        assert_eq!(sent, vec![ask(1, shared)]);
    }
}
