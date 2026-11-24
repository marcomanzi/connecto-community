//! `connecto works`: an agent sets up its own project's Agentic Works with
//! the same actions as the sheet (Terminal → Agentic Works…).
//!
//! The caller is the bridge's: its terminal and project from the process
//! tree, and the permission mode its agent was launched with, from the
//! launch. Three rules follow from them.
//!
//! - What makes Connecto run a command or start an agent (`link`, `publish`,
//!   `setup`, `test`) is for an agent started in Bypass mode only: it can run
//!   any command without asking already, so this gives it nothing new. Any
//!   other caller, and every job's own terminal, is refused and told how the
//!   user does it in the sheet, where they review it.
//! - A Cloud SQL password is used only for the instance and user Connecto
//!   asked the user for it for, by a request Connecto words itself. A
//!   connection string is only ever a connection string.
//! - No reply holds a vault value: every one is cleaned of them, JSON by
//!   value so it stays JSON.

use super::{
    Database, GoogleCredential, Result, State, Works, WorksAction, WorksControl, check, db,
    draft_fields, optional, provider, secret_name, store,
};
use connecto_core::agentic_works::{
    ALL_DATABASES, DEFAULT_DATABASE, JobAgent, MAX_PARALLEL, database_name,
};
use connecto_core::{
    AgentMode, AskKind, AskReply, AskRequest, Binding, SecretCaller, ToolKind, VaultBridge,
    VaultControl,
    agentic_works::{
        CloudSql, InstanceName, IpType, WorksBridge, WorksCaller, cloudsql_setup_steps,
    },
};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    future::Future,
    sync::{Arc, mpsc},
    time::Duration,
};

/// How long a command waits for its outcome: the bridge answers within 30 s.
const WAIT: Duration = Duration::from_secs(25);
/// How long `publish` and `setup` wait for a setup command before saying
/// it is still running.
const SETUP_WAIT: Duration = Duration::from_secs(8);
/// Longest service account key the command takes.
const KEY_MAX: usize = 64 * 1024;
/// The names Connecto gives the passwords it asks for, and the copies a
/// link keeps (`mod.rs`): never a connection string.
const ASKED_PREFIX: &str = "cloudsql-pw-";
const STAGED_PREFIX: &str = "cloudsql-password-";
/// What stands for a vault value in a reply.
const HIDDEN: &str = "[REDACTED]";

pub(super) const HELP: &str = "usage: connecto works <command>   (this project's Agentic Works)
  show [<type>]                 the databases, the work types and the last outcome;
                                with a type, its published definition as JSON
  databases                     the linked databases and how each last answered
  test-db [--db <NAME>]         test the linked databases (or the one named): each
                                step, changing nothing
  test-db [database options]    test a database before linking it
  link [--db <NAME>] --secret <SECRET> [--create]
                                link a PostgreSQL database: SECRET is a project secret
                                holding the connection string, with the agentic_works use
  link [--db <NAME>] --cloudsql <project:region:instance> --database <DB> [--user <USER>]
       [--key <SECRET> | --adc] [--ip public|private|psc] [--iam] [--create]
                                link Google Cloud SQL (--key: an imported service
                                account key; --adc: your gcloud login; --iam: sign in
                                as the service account, no password)
  unlink --db <NAME> [--force]  stop taking jobs from that database (its queue stays)
  key-import <file.json>        keep a service account key in the vault (prints its name)
  guide --cloudsql ... options  the gcloud and SQL steps that prepare Cloud SQL
  check [--file <def.json>]     validate a definition (or on stdin), publishing nothing
  publish <type> [--file <def.json>] [--db <NAME,NAME...>|all]
                                publish a work type (definition on stdin otherwise) for
                                every linked database, or only those named; its setup
                                command runs first
  setup <type>                  run a published type's setup again
  test <type> [--db <NAME>] [--prompt <text>] [--cli agy|claude|codex|pi|opencode] [--model M] [--effort E]
                                queue a job (prompt on stdin otherwise); prints its id
  job <id> [--db <NAME>]        a job's status, result JSON and reason
  parallel [<N>]                how many jobs run at once (1 to 32, 1 by default), and
                                those running; with N, set it
  agent [auto | <cli> [--model <M>]]
                                the agent for jobs whose type names none (session CLI
                                blank, agy or antigravity): auto (Antigravity, else the
                                first usable stand-in) or one CLI the project's Tools
                                allow, with model M or (no --model) its default model;
                                alone, show it and the CLIs and models offered
  ask '<question>'              (a job's own agent only) ask the user about this job when
                                stuck (question on stdin otherwise), then end the turn: a
                                notice wakes it when the reply is in
  answer                        (a job's own agent only) read that reply
A project takes jobs from several databases, each linked under a name such as dev,
staging or production; work types are published once, for all of them. --db names one;
it may be left out while the project has a single database (link: the first is named
default). A republished type keeps its databases unless --db is given.
With parallel above 1, jobs run at once in the project folder, each with its own folder
(CONNECTO_JOB_DIR): prepare must write only there.
--create makes the queue in a new database; without it, an existing queue is validated.
A definition is one JSON object:
  {\"instructions\": \"... {{prompt}} ...\", \"schema\": {<JSON Schema of the result>},
   \"setup\": {\"argv\": [\"tool/setup.sh\"]},                        (optional)
   \"prepare\": {\"argv\": [\"tool/prepare.py\"], \"timeout_s\": 600}}  (optional; then
                                the instructions contain {{prepared}})
A chosen agent is never replaced: a job it cannot run fails saying why. A work type that
names its agent keeps it, and its own model.
link, publish, setup, test, key-import, parallel <N> and agent <...> run commands, start agents or replace a
credential with the user's rights: only an agent started in Bypass mode may use them, and
never a job's agent or a terminal it started. Others are told how the user does it.
A connection string: ask the user with
  connecto secret request <NAME> --agentic-works --reason '<why>'
A Cloud SQL password: `link` or `test-db` asks the user for it, for that instance and
user only. Secret values are never shown.";

/// The verbs that change something, written to the audit log.
fn changes(verb: &str) -> bool {
    matches!(
        verb,
        "link" | "unlink" | "key-import" | "publish" | "setup" | "test" | "parallel" | "agent"
    )
}

/// A reply: text, or JSON kept as JSON.
enum Out {
    Text(String),
    Json(Value),
}

/// Options: `--flag value` for most, bare `--flag` for `SWITCHES`, and
/// words.
#[derive(Default)]
struct Options {
    values: HashMap<String, String>,
    switches: HashSet<String>,
    words: Vec<String>,
}

const SWITCHES: &[&str] = &["--adc", "--iam", "--create", "--force"];

impl Options {
    fn parse(args: &[String], allowed: &[&str]) -> Result<Self> {
        let mut out = Self::default();
        let mut words = args.iter();
        while let Some(word) = words.next() {
            if !word.starts_with("--") {
                out.words.push(word.clone());
                continue;
            }
            if !allowed.contains(&word.as_str()) {
                return Err(format!(
                    "unknown option {word:?} here. See `connecto works help`."
                ));
            }
            if SWITCHES.contains(&word.as_str()) {
                out.switches.insert(word.clone());
                continue;
            }
            let value = words.next().ok_or(format!("{word} needs a value"))?;
            if out.values.insert(word.clone(), value.clone()).is_some() {
                return Err(format!("{word} is given twice"));
            }
        }
        Ok(out)
    }
    fn value(&self, flag: &str) -> Option<String> {
        self.values
            .get(flag)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }
    fn on(&self, flag: &str) -> bool {
        self.switches.contains(flag)
    }
    /// The database `--db` names, checked.
    /// Given blank, it is refused, never taken as left out.
    fn db(&self) -> Result<Option<String>> {
        self.values
            .get("--db")
            .map(|name| database_name(name))
            .transpose()
    }
    /// The databases `--db` lists for a work type: None when it is not
    /// given, Some(None) for `all` (every database), else the names. A blank
    /// value or name is refused.
    fn db_list(&self) -> Result<Option<Option<Vec<String>>>> {
        let Some(raw) = self.values.get("--db") else {
            return Ok(None);
        };
        if raw.trim().eq_ignore_ascii_case(ALL_DATABASES) {
            return Ok(Some(None));
        }
        let names = raw
            .split(',')
            .map(|name| {
                if name.trim().is_empty() {
                    Err(format!(
                        "--db {raw:?} has a blank name: give database names separated by commas, or all."
                    ))
                } else {
                    database_name(name)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(Some(names)))
    }
    fn no_words(&self, verb: &str) -> Result<()> {
        match self.words.first() {
            None => Ok(()),
            Some(extra) => Err(format!("`works {verb}` does not take {extra:?}")),
        }
    }
    /// The one word a verb takes, such as a work type.
    fn one(&self, verb: &str, what: &str) -> Result<String> {
        match self.words.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(format!("`works {verb}` takes one {what}")),
        }
    }
}

const DATABASE: &[&str] = &[
    "--secret",
    "--cloudsql",
    "--database",
    "--user",
    "--key",
    "--adc",
    "--ip",
    "--iam",
];

/// A work type's name: what a producer writes in `work_type`.
fn kind_name(kind: &str) -> Result<String> {
    let kind = kind.trim();
    if kind.is_empty() || kind.chars().count() > 64 || kind.chars().any(char::is_control) {
        return Err("A work type is a short name, such as summary.".into());
    }
    Ok(kind.to_owned())
}

/// A definition as an agent gives it, in the stored form.
fn definition_of(text: Option<&str>) -> Result<store::Definition> {
    let draft = draft_fields(text.ok_or("Give the definition on stdin or with --file.")?)?;
    Ok(store::Definition {
        instructions: draft.instructions,
        schema: draft.schema,
        setup: optional(Some(draft.setup)),
        prepare: optional(Some(draft.prepare)),
    })
}

/// The database `--db` names, or the project's only one.
fn pick(links: Vec<store::Link>, named: Option<String>) -> Result<store::Link> {
    let names = || {
        links
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match named {
        Some(name) => {
            let list = names();
            links.into_iter().find(|l| l.name == name).ok_or_else(|| {
                if list.is_empty() {
                    format!("This project has no database named {name}: it has none linked.")
                } else {
                    format!("This project has no database named {name}. Its databases: {list}.")
                }
            })
        }
        None if links.len() > 1 => Err(format!(
            "This project has several databases ({}): say which with --db <NAME>.",
            names()
        )),
        None => links
            .into_iter()
            .next()
            .ok_or_else(|| "Link a database first: `connecto works link ...`.".to_owned()),
    }
}

/// The database the options name, how to sign in to it, and the
/// Connecto-asked password secret it uses (removed once a link keeps its
/// own copy).
struct Target {
    database: Database,
    password: Option<String>,
    asked: Option<String>,
}

/// Shorter than this, a value is hidden only where it stands alone: a whole
/// JSON value, or a word of its own in text.
const SHORT: usize = 8;

/// What no reply may show: every form of the vault's values (from 8 bytes,
/// as the rest of Connecto hides them), and this project's own Agentic Works
/// secrets whatever their length (a six-digit database password too).
struct Hider {
    /// Longest first, hidden wherever they appear.
    long: Vec<String>,
    /// Hidden where they stand alone.
    short: Vec<String>,
}

impl Hider {
    fn new(vault: &connecto_vault::Vault, project: &str) -> Self {
        let mut values: Vec<String> = VaultBridge::known(vault).to_vec();
        for name in vault.host_secrets(project, &Binding::AgenticWorks) {
            values.extend(vault.works_credential(project, &name));
        }
        for name in vault.host_secrets(project, &Binding::GoogleServiceAccount) {
            values.extend(vault.google_key(project, &name));
        }
        Self::of(&values)
    }

    fn of(values: &[String]) -> Self {
        let (mut long, mut short) = (Vec::new(), Vec::new());
        for value in values.iter().filter(|v| !v.trim().is_empty()) {
            // As given, and as it reads inside a JSON string.
            let quoted = serde_json::to_string(value).unwrap_or_default();
            let escaped = quoted
                .strip_prefix('"')
                .and_then(|q| q.strip_suffix('"'))
                .unwrap_or_default()
                .to_owned();
            for form in [value.clone(), escaped] {
                let list = if form.len() >= SHORT {
                    &mut long
                } else {
                    &mut short
                };
                if !form.is_empty() && !list.contains(&form) {
                    list.push(form);
                }
            }
        }
        long.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        Self { long, short }
    }

    fn text(&self, text: &str) -> String {
        let mut out = connecto_redact::sanitize(text);
        for form in &self.long {
            if out.contains(form.as_str()) {
                out = out.replace(form.as_str(), HIDDEN);
            }
        }
        for form in &self.short {
            out = alone(&out, form);
        }
        connecto_redact::hide_known(&out)
    }

    /// Whether `s` is a value to hide, whole.
    fn whole(&self, s: &str) -> bool {
        self.short.iter().chain(&self.long).any(|f| f == s)
    }

    /// `value` with the vault's values hidden in its strings, keys and
    /// numbers: still JSON.
    fn json(&self, value: Value) -> Value {
        match value {
            Value::String(s) if self.whole(&s) => Value::String(HIDDEN.to_owned()),
            Value::String(s) => Value::String(self.text(&s)),
            Value::Number(n) => {
                let shown = n.to_string();
                if self.whole(&shown) || self.text(&shown) != shown {
                    Value::String(HIDDEN.to_owned())
                } else {
                    Value::Number(n)
                }
            }
            Value::Array(items) => Value::Array(items.into_iter().map(|v| self.json(v)).collect()),
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| {
                        let key = if self.whole(&k) {
                            HIDDEN.to_owned()
                        } else {
                            self.text(&k)
                        };
                        (key, self.json(v))
                    })
                    .collect::<Map<String, Value>>(),
            ),
            other => other,
        }
    }
}

/// `text` with each `form` that stands alone (no letter or digit right
/// before or after it in the text) replaced.
fn alone(text: &str, form: &str) -> String {
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    let mut out = String::with_capacity(text.len());
    let mut before: Option<char> = None;
    let mut rest = text;
    while let Some(at) = rest.find(form) {
        let head = &rest[..at];
        if let Some(last) = head.chars().last() {
            before = Some(last);
        }
        let tail = &rest[at + form.len()..];
        out.push_str(head);
        if word(before) || word(tail.chars().next()) {
            out.push_str(form);
        } else {
            out.push_str(HIDDEN);
        }
        before = form.chars().last();
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// `text` as a shell word: as it is when it is plain, else single-quoted.
fn shell_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// `text` as an SQL string literal.
fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl WorksBridge for Works {
    fn command(
        &self,
        caller: &WorksCaller,
        verb: &str,
        args: &[String],
        text: Option<&str>,
        notify: AskReply,
    ) -> Result<String> {
        let out = self.cli(caller, verb, args, text, notify);
        // `parallel` and `agent` alone only read the setting.
        if changes(verb) && !(matches!(verb, "parallel" | "agent") && args.is_empty()) {
            self.state.audit.append_nowait(connecto_audit::Record {
                actor: caller.terminal.clone(),
                source: "connecto works".into(),
                target: args
                    .first()
                    .filter(|a| !a.starts_with("--"))
                    .cloned()
                    .unwrap_or_default(),
                action: format!("agentic_works.cli.{verb}"),
                outcome: if out.is_ok() { "ok" } else { "refused" }.into(),
                project: caller.project_id.clone(),
                ..connecto_audit::Record::default()
            });
        }
        let hider = Hider::new(&self.state.vault, &caller.project_id);
        match out {
            Ok(Out::Text(text)) => Ok(hider.text(&text)),
            Ok(Out::Json(value)) => {
                serde_json::to_string_pretty(&hider.json(value)).map_err(|e| e.to_string())
            }
            Err(why) => Err(hider.text(&why)),
        }
    }
}

impl Works {
    fn cli(
        &self,
        caller: &WorksCaller,
        verb: &str,
        args: &[String],
        text: Option<&str>,
        notify: AskReply,
    ) -> Result<Out> {
        let project = caller.project_id.as_str();
        let mut notify = Some(notify);
        let text_out = |r: Result<String>| r.map(Out::Text);
        match verb {
            "help" | "--help" => Ok(Out::Text(HELP.to_owned())),
            "show" => {
                let o = Options::parse(args, &[])?;
                match o.words.as_slice() {
                    [] => Ok(Out::Text(self.show(project))),
                    [kind] => self.show_type(project, kind).map(Out::Json),
                    _ => Err("`works show` takes at most a work type".into()),
                }
            }
            "databases" => {
                Options::parse(args, &[])?.no_words(verb)?;
                Ok(Out::Text(self.databases(project)))
            }
            "link" => {
                let mut allowed = DATABASE.to_vec();
                allowed.extend(["--create", "--db"]);
                let o = Options::parse(args, &allowed)?;
                o.no_words(verb)?;
                if let Err(why) = self.authority(caller) {
                    return Err(format!(
                        "Not linked. {why} Linking a database lets whoever writes to it start agents in this project. Check the database with `connecto works test-db ...`, then ask the user to link it in Terminal → Agentic Works…."
                    ));
                }
                let target = self.target(caller, &o, &mut notify)?.ok_or(
                    "Say which database: --secret <NAME>, or --cloudsql <project:region:instance> --database <DB>. See `connecto works help`.",
                )?;
                let create = o.on("--create");
                // Unnamed: the project's only database, or the first one.
                let name = match o.db()? {
                    Some(name) => name,
                    None => {
                        let links = self.state.store.links(project)?;
                        match links.as_slice() {
                            [] => DEFAULT_DATABASE.to_owned(),
                            [only] => only.name.clone(),
                            _ => {
                                return Err(format!(
                                    "This project has several databases ({}): say which to link or change with --db <NAME>, or give a new name to add one.",
                                    links
                                        .iter()
                                        .map(|l| l.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ));
                            }
                        }
                    }
                };
                let done = self.now(
                    project,
                    WorksAction::Link {
                        name,
                        database: target.database,
                        create,
                        password: target.password,
                    },
                )?;
                // The link keeps its own copy of the password.
                // Its record stays until the secret is gone: a password is
                // never left without its destination.
                if let Some(asked) = target.asked {
                    let scope = connecto_core::ProviderScope::Project(project.to_owned());
                    if VaultControl::delete(&*self.state.vault, &scope, &asked).is_ok() {
                        let _ = self.state.store.unbind_password(project, &asked);
                    }
                }
                Ok(Out::Text(format!(
                    "{done} Connecto takes jobs from it while this project is open. Next: publish a work type with `connecto works publish <type>`."
                )))
            }
            "test-db" => {
                let mut allowed = DATABASE.to_vec();
                allowed.push("--db");
                let o = Options::parse(args, &allowed)?;
                o.no_words(verb)?;
                let named = o.db()?;
                if named.is_some() && (o.values.len() > 1 || !o.switches.is_empty()) {
                    return Err("--db tests a linked database: give it alone, or the options of a database to test before linking it.".into());
                }
                let tests: Vec<(Option<String>, Database, Option<String>)> =
                    match self.target(caller, &o, &mut notify)? {
                        Some(target) => vec![(None, target.database, target.password)],
                        None => {
                            let links = self.state.store.links(project)?;
                            if links.is_empty() {
                                return Err(
                                    "This project has no linked database: give its options.".into(),
                                );
                            }
                            let links = match named {
                                Some(name) => vec![pick(links, Some(name))?],
                                None => links,
                            };
                            links
                                .into_iter()
                                .map(|l| (Some(l.name), l.database, None))
                                .collect()
                        }
                    };
                let several = tests.len() > 1;
                let mut reports = Vec::new();
                for (name, database, password) in tests {
                    let report = self
                        .now(project, WorksAction::Test { database, password })
                        .unwrap_or_else(|e| format!("✗ {e}"));
                    reports.push(match name {
                        Some(name) if several => format!("Database {name}:\n{report}"),
                        _ => report,
                    });
                }
                let report = reports.join("\n\n");
                if report.contains('✗') {
                    Err(report)
                } else {
                    Ok(Out::Text(report))
                }
            }
            "unlink" => {
                let o = Options::parse(args, &["--db", "--force"])?;
                o.no_words(verb)?;
                let name = o
                    .db()?
                    .ok_or("Say which database to remove with --db <NAME>.")?;
                if let Err(why) = self.authority(caller) {
                    return Err(format!(
                        "Not removed. {why} Ask the user to remove the database {name} in Terminal → Agentic Works…."
                    ));
                }
                text_out(self.now(
                    project,
                    WorksAction::Unlink {
                        name,
                        force: o.on("--force"),
                    },
                ))
            }
            "key-import" => {
                let o = Options::parse(args, &[])?;
                let file = o.one(verb, "key file")?;
                let json = text.ok_or("Give the key file's path.")?;
                // An import replaces the key kept for that account, which a
                // link may be using.
                if let Err(why) = self.authority(caller) {
                    return Err(format!(
                        "Not imported. {why} Ask the user to import {file} with Import key file… in Terminal → Agentic Works…."
                    ));
                }
                if json.len() > KEY_MAX {
                    return Err("That file is too large to be a service account key.".into());
                }
                let key = self.keep_key(project, json)?;
                Ok(Out::Text(format!(
                    "Imported the key of {} as {}. Link with --key {}. The key is in Connecto's vault: delete the downloaded file {file}.",
                    key.email, key.secret, key.secret
                )))
            }
            "guide" => {
                let o = Options::parse(args, DATABASE)?;
                o.no_words(verb)?;
                if o.value("--secret").is_some() {
                    return Err(
                        "`works guide` is for Cloud SQL: give --cloudsql and its options.".into(),
                    );
                }
                Ok(Out::Text(guide(&cloudsql_of(&o)?)))
            }
            "check" => {
                Options::parse(args, &[])?.no_words(verb)?;
                let def = definition_of(text)?;
                let checked = check(&def)?;
                Ok(Out::Text(format!(
                    "The definition is valid: instructions with {{{{prompt}}}}, a JSON Schema, {}, {}. Publish it with `connecto works publish <type>`.",
                    if checked.setup.is_some() {
                        "a setup command"
                    } else {
                        "no setup"
                    },
                    if checked.prepare.is_some() {
                        "a prepare command"
                    } else {
                        "no prepare"
                    },
                )))
            }
            "publish" => {
                let o = Options::parse(args, &["--db"])?;
                let kind = kind_name(&o.one(verb, "work type")?)?;
                let def = definition_of(text)?;
                let has_setup = check(&def)?.setup.is_some();
                // Republished without --db, a type keeps its databases.
                let databases = match o.db_list()? {
                    None => self.state.store.databases_of(project, &kind)?,
                    Some(given) => given,
                };
                if let Err(why) = self.authority(caller) {
                    return Err(self.hand_to_user(caller, &kind, &why, databases.as_deref())?);
                }
                let action = WorksAction::Publish {
                    kind: kind.clone(),
                    instructions: def.instructions,
                    schema: def.schema,
                    setup: def.setup,
                    prepare: def.prepare,
                    databases,
                };
                let wait = if has_setup { SETUP_WAIT } else { WAIT };
                match self.within(project, action, wait) {
                    Some(done) => text_out(done.map(|done| {
                        format!(
                            "{done} Queue a test job with `connecto works test {kind} --prompt '...'`."
                        )
                    })),
                    None => Ok(Out::Text(self.setup_running(project, &kind))),
                }
            }
            "setup" => {
                let o = Options::parse(args, &[])?;
                let kind = kind_name(&o.one(verb, "work type")?)?;
                if let Err(why) = self.authority(caller) {
                    return Err(format!(
                        "Setup not run. {why} Ask the user to click Re-run setup next to {kind} in Terminal → Agentic Works…."
                    ));
                }
                match self.within(
                    project,
                    WorksAction::Setup { kind: kind.clone() },
                    SETUP_WAIT,
                ) {
                    Some(done) => text_out(done),
                    None => Ok(Out::Text(self.setup_running(project, &kind))),
                }
            }
            "test" => {
                let o = Options::parse(args, &["--cli", "--model", "--effort", "--db"])?;
                let kind = kind_name(&o.one(verb, "work type")?)?;
                let prompt = text.ok_or("Give the prompt with --prompt or on stdin.")?;
                if let Err(why) = self.authority(caller) {
                    return Err(format!(
                        "Not queued. {why} A job starts an agent in this project and runs the type's prepare command. Ask the user to queue it with:\nINSERT INTO connecto_works.connecto_jobs (project_name, work_type, prompt) VALUES ({}, {}, '...') RETURNING id;",
                        sql_text(&self.state.project_name(project)),
                        sql_text(&kind)
                    ));
                }
                text_out(self.test(project, &kind, prompt, &o))
            }
            "parallel" => {
                let o = Options::parse(args, &[])?;
                let jobs = match o.words.as_slice() {
                    [] => return Ok(Out::Text(self.parallel(project))),
                    [n] => n.parse::<u32>().ok().filter(|n| (1..=MAX_PARALLEL).contains(n)).ok_or_else(|| {
                        format!("Give the number of jobs at once, from 1 to {MAX_PARALLEL}, not {n:?}.")
                    })?,
                    _ => return Err("`works parallel` takes one number at most".into()),
                };
                if let Err(why) = self.authority(caller) {
                    return Err(format!(
                        "Not changed. {why} More jobs at once start more agents and prepare commands in this project. Ask the user to set Jobs at once in Terminal → Agentic Works…."
                    ));
                }
                text_out(self.now(project, WorksAction::Parallel { jobs }))
            }
            "agent" => {
                let o = Options::parse(args, &["--model"])?;
                let model = o.value("--model");
                let agent = match o.words.as_slice() {
                    [] if model.is_none() => return Ok(Out::Text(self.job_agent(project))),
                    [] => return Err(
                        "Name the CLI the model is for: `connecto works agent <cli> --model <M>`."
                            .into(),
                    ),
                    [word] if matches!(word.as_str(), "auto" | "automatic") => {
                        if model.is_some() {
                            return Err("auto takes no --model: each stand-in has its own.".into());
                        }
                        JobAgent::Automatic
                    }
                    [word] => {
                        let tool = match word.as_str() {
                            "agy" => Some(ToolKind::Antigravity),
                            w => ToolKind::from_name(w).filter(|t| t.is_agent()),
                        }
                        .ok_or_else(|| {
                            format!("Give auto, or one of claude, codex, pi, antigravity, opencode, not {word:?}.")
                        })?;
                        JobAgent::Cli { tool, model }
                    }
                    _ => return Err("`works agent` takes auto or one CLI".into()),
                };
                if let Err(why) = self.authority(caller) {
                    return Err(format!(
                        "Not changed. {why} The agent for jobs decides which agent runs this project's jobs. Ask the user to set Agent for jobs in Terminal → Agentic Works…."
                    ));
                }
                text_out(self.now(project, WorksAction::JobAgent { agent }))
            }
            "ask" => {
                // The question as words, or (none given) as read from stdin.
                let question = match (args.is_empty(), text) {
                    (false, None) => args.join(" "),
                    (true, Some(text)) => text.to_owned(),
                    _ => {
                        return Err(
                            "`works ask` takes the question as words or on stdin, not both.".into(),
                        );
                    }
                };
                let notify = notify.take().ok_or("Nothing to answer to.")?;
                self.state.ask(caller, &question, notify).map(Out::Text)
            }
            "answer" => {
                Options::parse(args, &[])?.no_words(verb)?;
                self.state.job_answer(caller).map(Out::Text)
            }
            "job" => {
                let o = Options::parse(args, &["--db"])?;
                let id: i64 = o
                    .one(verb, "job id")?
                    .parse()
                    .map_err(|_| "A job id is a number, as `works test` printed it.")?;
                self.job(project, id, o.db()?).map(Out::Json)
            }
            other => Err(format!(
                "unknown works command {other:?}. See `connecto works help`."
            )),
        }
    }

    /// Whether `caller` may make Connecto run commands, start agents or
    /// change a credential: an agent whose own session Connecto started in
    /// Bypass mode, and not a job's agent nor one a job's agent started.
    fn authority(&self, caller: &WorksCaller) -> Result<()> {
        if self.state.job_lineage(caller) {
            return Err(
                "This terminal runs an Agentic Works job, or was started by one: a job's agent cannot change Agentic Works."
                    .into(),
            );
        }
        match caller.launched {
            Some(AgentMode::Bypass) => Ok(()),
            Some(mode) => Err(format!(
                "This terminal's agent was started in {} mode, and this would run commands, start agents or change a credential with the user's rights, outside its permissions: only an agent started in Bypass mode may do it from here.",
                mode.cli_value()
            )),
            None => Err(
                "Only an agent terminal started in Bypass mode may do this from the command line."
                    .into(),
            ),
        }
    }

    /// A definition the caller may not publish goes to the user, to review
    /// and publish in the sheet: this terminal becomes the type's author, and
    /// its final reply is what the sheet imports.
    /// The databases asked for go with it: the sheet imports them with the
    /// definition and shows them before publishing.
    fn hand_to_user(
        &self,
        caller: &WorksCaller,
        kind: &str,
        why: &str,
        databases: Option<&[String]>,
    ) -> Result<String> {
        if self.state.job_lineage(caller) {
            return Ok(format!("Not published. {why}"));
        }
        self.state
            .store
            .author(&caller.project_id, kind, &caller.terminal, databases)?;
        let scope = match databases {
            None => "every linked database".to_owned(),
            Some(list) => format!("the databases {} only", list.join(", ")),
        };
        Ok(format!(
            "Not published. {why} The definition is valid. To have the user review and publish it: end your turn with exactly the definition JSON as your whole final reply, nothing else. Then the user opens Terminal → Agentic Works…, enters the work type {kind}, clicks Import author reply, reviews it and publishes it, for {scope} as you asked (the sheet shows it). Tell them so before your final reply."
        ))
    }

    /// Run `action` and wait for its outcome.
    fn now(&self, project: &str, action: WorksAction) -> Result<String> {
        self.within(project, action, WAIT)
            .unwrap_or_else(|| Err("Still running: see `connecto works show` in a minute.".into()))
    }

    /// Run `action` and wait up to `wait` for its outcome; None when it is
    /// still running (its outcome then becomes the project's notice).
    fn within(&self, project: &str, action: WorksAction, wait: Duration) -> Option<Result<String>> {
        let (done, outcome) = mpsc::channel();
        self.spawn_action(project.to_owned(), action, Some(done));
        outcome.recv_timeout(wait).ok()
    }

    fn setup_running(&self, project: &str, kind: &str) -> String {
        format!(
            "Setup for {kind} is still running, in the shell terminal \"setup {kind} · {}\" of the agentic jobs project (Connecto Internal group); the user may need to answer it there. {kind} is published when setup succeeds. See the outcome with `connecto works show` later; do not poll in a loop.",
            self.state.project_name(project)
        )
    }

    /// Run `work` on the Agentic Works runtime and wait for it.
    fn block<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let (done, outcome) = mpsc::channel();
        self.handle.spawn(async move {
            let out = tokio::time::timeout(Duration::from_secs(20), work)
                .await
                .unwrap_or_else(|_| Err("Operation timed out.".into()));
            let _ = done.send(out);
        });
        outcome
            .recv_timeout(WAIT)
            .unwrap_or_else(|_| Err("Operation timed out.".into()))
    }

    fn show(&self, project: &str) -> String {
        let view = self.view(project);
        let name = self.state.project_name(project);
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Project: {}",
            if name.is_empty() { project } else { &name }
        );
        out.push_str(&self.databases(project));
        if view.types.is_empty() {
            let _ = writeln!(out, "Work types: none published.");
        } else {
            let _ = writeln!(out, "Work types:");
            for t in &view.types {
                let setup = match (&t.setup, &t.setup_ok_at) {
                    (false, _) => "no setup".to_owned(),
                    (true, Some(at)) => format!("setup succeeded {at}"),
                    (true, None) => "setup has not succeeded".into(),
                };
                let databases = match &t.databases {
                    None => "every database".to_owned(),
                    Some(list) => format!("databases {}", list.join(", ")),
                };
                let _ = writeln!(out, "  {}  ({databases}; {setup})", t.name);
            }
        }
        let names = |list: Vec<String>| {
            if list.is_empty() {
                "none".to_owned()
            } else {
                list.join(", ")
            }
        };
        let connection_strings: Vec<String> = self
            .state
            .vault
            .host_secrets(project, &Binding::AgenticWorks)
            .into_iter()
            .filter(|n| !self.is_password(project, n))
            .collect();
        let _ = writeln!(
            out,
            "Connection string secrets of this project: {}",
            names(connection_strings)
        );
        let _ = writeln!(
            out,
            "Google service account keys: {}",
            names(view.google_keys)
        );
        if !view.notice.is_empty() {
            let _ = writeln!(out, "Last outcome: {}", view.notice);
        }
        out
    }

    /// How many jobs run at once, and those running now.
    fn parallel(&self, project: &str) -> String {
        let view = self.view(project);
        let mut out = if view.parallel <= 1 {
            "Jobs at once: 1 (one job at a time, every database in turn).\n".to_owned()
        } else {
            format!(
                "Jobs at once: {} (shared by the databases: the one running the fewest gets the next).\n",
                view.parallel
            )
        };
        if view.running.is_empty() {
            out.push_str("Running: none.\n");
        } else {
            let _ = writeln!(out, "Running ({}):", view.running.len());
            for r in &view.running {
                let _ = writeln!(
                    out,
                    "  job {} of {}  {}  since {}",
                    r.id, r.database, r.kind, r.since
                );
                if !r.agent.is_empty() {
                    let said = match r.agent_stage {
                        connecto_core::agentic_works::AgentStage::Running => "on",
                        connecto_core::agentic_works::AgentStage::Dispatched => {
                            "dispatched to (starting)"
                        }
                        connecto_core::agentic_works::AgentStage::Selected => {
                            "selected (not started yet)"
                        }
                    };
                    let _ = writeln!(out, "    {said} {}", r.agent);
                }
            }
        }
        out
    }

    /// The agent for jobs that name none, and the CLIs and models offered.
    fn job_agent(&self, project: &str) -> String {
        let view = self.view(project);
        let mut out = format!("Agent for jobs: {}.\n", view.job_agent.label());
        out.push_str("Jobs whose type names an agent keep it, and their own model.\n");
        if view.job_agent_choices.is_empty() {
            out.push_str("Offered: none (the project's Tools allow no agent).\n");
        } else {
            out.push_str("Offered (the project's Tools):\n");
            for c in &view.job_agent_choices {
                let models = if c.models.is_empty() {
                    "its default model".to_owned()
                } else {
                    format!("its default model, {}", c.models.join(", "))
                };
                let _ = writeln!(out, "  {}  {models}", c.tool.name());
            }
        }
        out
    }

    /// The linked databases, each with how it last answered.
    fn databases(&self, project: &str) -> String {
        let view = self.view(project);
        if view.databases.is_empty() {
            return "Databases: none linked. Link one with `connecto works link ...` (see `connecto works help`).\n".into();
        }
        let mut out = String::from("Databases:\n");
        for d in &view.databases {
            let status = match d.status.ok {
                None => "not looked at yet".to_owned(),
                Some(true) => format!("ok at {}: {}", d.status.at, d.status.message),
                Some(false) => format!("FAILING at {}: {}", d.status.at, d.status.message),
            };
            let _ = writeln!(out, "  {}  {}  ({status})", d.name, d.database.label());
        }
        out.push_str(&self.parallel(project));
        let _ = writeln!(
            out,
            "Agent for jobs: {} (`connecto works agent`).",
            self.view(project).job_agent.label()
        );
        out
    }

    fn show_type(&self, project: &str, kind: &str) -> Result<Value> {
        let def = self.state.store.definition(project, kind)?;
        let databases = self.state.store.databases_of(project, kind)?;
        let json = |raw: &Option<String>| -> Value {
            raw.as_deref()
                .and_then(|r| serde_json::from_str(r).ok())
                .unwrap_or(Value::Null)
        };
        Ok(json!({
            "instructions": def.instructions,
            "schema": serde_json::from_str::<Value>(&def.schema)
                .unwrap_or_else(|_| Value::String(def.schema.clone())),
            "setup": json(&def.setup),
            "prepare": json(&def.prepare),
            "databases": databases.map_or_else(|| Value::String("all".into()), |l| json!(l)),
        }))
    }

    /// A Cloud SQL password secret: one Connecto asked for, or a linked
    /// database's own. Never used as a connection string.
    fn is_password(&self, project: &str, name: &str) -> bool {
        name.starts_with(ASKED_PREFIX)
            || name.starts_with(STAGED_PREFIX)
            || self.state.store.is_password(project, name).unwrap_or(true)
            || self
                .state
                .store
                .password_in_use(project, name)
                .unwrap_or(true)
    }

    /// The database the options name; None when they name none. Every
    /// refusal comes before any connection is opened.
    fn target(
        &self,
        caller: &WorksCaller,
        o: &Options,
        notify: &mut Option<AskReply>,
    ) -> Result<Option<Target>> {
        let project = caller.project_id.as_str();
        // How many database options are given: --db names the link, not the
        // database.
        let given = o.values.keys().filter(|k| *k != "--db").count();
        match (o.value("--secret"), o.value("--cloudsql")) {
            (Some(_), Some(_)) => Err("Give --secret or --cloudsql, not both.".into()),
            (None, None) => {
                if given == 0 && o.switches.iter().all(|s| s == "--create") {
                    Ok(None)
                } else {
                    Err("Database options need --secret <NAME> or --cloudsql <project:region:instance>.".into())
                }
            }
            (Some(name), None) => {
                if given > 1 || o.on("--adc") || o.on("--iam") {
                    return Err("--secret takes no Cloud SQL options.".into());
                }
                if self.is_password(project, &name) {
                    return Err(format!(
                        "{name} is a Cloud SQL password, not a connection string."
                    ));
                }
                if !self
                    .state
                    .vault
                    .host_secrets(project, &Binding::AgenticWorks)
                    .contains(&name)
                {
                    return Err(format!(
                        "This project has no secret {name} with the agentic_works use. Ask the user for the connection string with `connecto secret request {name} --agentic-works --reason '<why>'` and end your turn until they answer."
                    ));
                }
                Ok(Some(Target {
                    database: Database::ConnectionString(name),
                    password: None,
                    asked: None,
                }))
            }
            (None, Some(_)) => {
                let mut sql = cloudsql_of(o)?;
                if sql.database.is_empty() {
                    return Err("Give the database with --database <DB>.".into());
                }
                if sql.user.is_empty() && !sql.iam_auth {
                    return Err("Give the database user with --user <USER> (or use --iam).".into());
                }
                if o.value("--key").is_some() && o.on("--adc") {
                    return Err("Give --key or --adc, not both.".into());
                }
                if let GoogleCredential::Key(name) = &mut sql.credential {
                    let keys = self
                        .state
                        .vault
                        .host_secrets(project, &Binding::GoogleServiceAccount);
                    if name.is_empty() {
                        match keys.as_slice() {
                            [only] => name.clone_from(only),
                            [] => return Err("Import a service account key with `connecto works key-import <file.json>`, or use --adc for your gcloud login. `connecto works guide --cloudsql ...` shows how to make one.".into()),
                            many => return Err(format!("Choose the key with --key: {}.", many.join(", "))),
                        }
                    } else if !keys.contains(name) {
                        return Err(format!(
                            "This project has no service account key {name}: import it with `connecto works key-import <file.json>`."
                        ));
                    }
                }
                if sql.iam_auth {
                    return Ok(Some(Target {
                        database: Database::CloudSql(sql),
                        password: None,
                        asked: None,
                    }));
                }
                // A linked database's own password, for its own instance
                // and user only (as the sheet).
                let saved =
                    self.state
                        .store
                        .links(project)?
                        .into_iter()
                        .find_map(|l| match l.database {
                            Database::CloudSql(linked)
                                if !linked.password_secret.is_empty()
                                    && linked.connection_name.trim() == sql.connection_name
                                    && linked.user.trim() == sql.user =>
                            {
                                Some(linked.password_secret)
                            }
                            _ => None,
                        });
                if let Some(secret) = saved {
                    sql.password_secret = secret;
                    return Ok(Some(Target {
                        database: Database::CloudSql(sql),
                        password: None,
                        asked: None,
                    }));
                }
                // One the user gave Connecto for exactly this instance and user.
                let bound =
                    self.state
                        .store
                        .password_for(project, &sql.connection_name, &sql.user)?;
                if let Some(secret) = bound
                    && let Ok(value) = self.state.vault.works_credential(project, &secret)
                {
                    return Ok(Some(Target {
                        database: Database::CloudSql(sql),
                        password: Some(value),
                        asked: Some(secret),
                    }));
                }
                Err(self.ask_password(caller, &sql, notify)?)
            }
        }
    }

    /// Ask the user for the password of `sql`'s user on its instance, in
    /// Connecto's words, and keep it for that destination. Ok holds what to
    /// tell the agent.
    fn ask_password(
        &self,
        caller: &WorksCaller,
        sql: &CloudSql,
        notify: &mut Option<AskReply>,
    ) -> Result<String> {
        let project = caller.project_id.as_str();
        // One at a time: the look-up, the reservation and the request are
        // one step, so one destination gets one request.
        let _one = self
            .state
            .asking
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = &self.state.store;
        // The terminal whose request is waiting: it alone gets the notice.
        let waiting = |name: &str| {
            VaultControl::view(&*self.state.vault)
                .asks
                .iter()
                .find(|a| a.name == name && a.project.as_deref() == Some(project))
                .map(|a| a.terminal.clone())
        };
        let before = store.password_for(project, &sql.connection_name, &sql.user)?;
        if let Some(name) = &before {
            match waiting(name) {
                Some(asker) if asker == caller.terminal => {
                    return Ok(format!(
                        "Connecto already asked the user for the password of {} on {}. End your turn: a [connecto notice] tells you when they answer. Then run this command again.",
                        sql.user, sql.connection_name
                    ));
                }
                Some(asker) => {
                    return Ok(format!(
                        "Connecto already asked the user for the password of {} on {}, for terminal {asker}: that terminal gets the notice when they answer, not this one. Ask the user, or {asker}, whether they answered, then run this command again.",
                        sql.user, sql.connection_name
                    ));
                }
                None => {}
            }
            // Given since this command looked: use it.
            if self.state.vault.works_credential(project, name).is_ok() {
                return Ok(format!(
                    "The user gave the password of {} on {}: run this command again.",
                    sql.user, sql.connection_name
                ));
            }
        }
        let name = secret_name(
            ASKED_PREFIX,
            &uuid::Uuid::new_v4().simple().to_string()[..12],
        );
        // Reserved before it is asked for: the user is never shown a
        // request whose answer has no destination.
        store.bind_password(project, &sql.connection_name, &sql.user, &name)?;
        let who = SecretCaller {
            terminal: caller.terminal.clone(),
            project: Some(project.to_owned()),
            project_folder: None,
            owns_pid: Arc::new(|_| false),
        };
        let request = AskRequest {
            name: name.clone(),
            kind: AskKind::Password,
            reason: format!(
                "Connecto asks: the password of the database user {} on the Google Cloud SQL instance {}, for this project's Agentic Works. Connecto uses it for that instance and user only.",
                sql.user, sql.connection_name
            ),
            proposed: vec![Binding::AgenticWorks],
        };
        let asked = notify
            .take()
            .ok_or_else(|| "The request could not be made.".to_owned())
            .and_then(|notify| VaultBridge::ask(&*self.state.vault, &who, request, notify));
        if let Err(why) = asked {
            // Nothing was asked: the destination keeps what it had.
            let _ = match &before {
                Some(old) => store.bind_password(project, &sql.connection_name, &sql.user, old),
                None => store.unbind_password(project, &name),
            };
            return Err(why);
        }
        Ok(format!(
            "Asked the user for the password of {} on {}. End your turn now: a [connecto notice] tells you when they answer. Then run this command again.",
            sql.user, sql.connection_name
        ))
    }

    fn test(&self, project: &str, kind: &str, prompt: &str, o: &Options) -> Result<String> {
        if self.state.workspace.relay(project).is_none() {
            return Err("Open the local project first.".into());
        }
        let link = pick(self.state.store.links(project)?, o.db()?)?;
        self.state.store.definition(project, kind).map_err(|_| {
            format!("{kind} is not published: `connecto works publish {kind}` first.")
        })?;
        if let Some(list) = self.state.store.databases_of(project, kind)?
            && !list.contains(&link.name)
        {
            return Err(format!(
                "{kind} is published for {} only, not for the database {}: publish it again with --db to add it.",
                list.join(", "),
                link.name
            ));
        }
        let (database, queue, db_name) = (link.database, link.queue, link.name);
        let cli = o.value("--cli").unwrap_or_else(|| "agy".to_owned());
        provider(&cli)?;
        let name = self.state.project_name(project);
        let (s, project_id, kind_owned, prompt) = (
            Arc::clone(&self.state),
            project.to_owned(),
            kind.to_owned(),
            prompt.to_owned(),
        );
        let (model, effort) = (o.value("--model"), o.value("--effort"));
        let job = name.clone();
        let id = self.block(async move {
            let mut client = s
                .connect(&project_id, &database, None, &mut Vec::new())
                .await?;
            db::enqueue(
                &mut client,
                &project_id,
                &queue,
                &job,
                &kind_owned,
                &prompt,
                &cli,
                model.as_deref(),
                effort.as_deref(),
            )
            .await
        })?;
        Ok(format!(
            "Queued job {id} ({kind}) for {name} in the database {db_name}. Connecto claims it within a few seconds while the project is open and nothing is paused ({}, newest job first), and runs it in a terminal named agentic_works_{id}_…. It can take minutes: read the outcome later with `connecto works job {id} --db {db_name}`.",
            match self.state.store.parallel(project)? {
                1 => "one job at a time in this project, taking each database in turn".to_owned(),
                n => format!("up to {n} jobs at once in this project, shared by its databases"),
            }
        ))
    }

    fn job(&self, project: &str, id: i64, named: Option<String>) -> Result<Value> {
        let link = pick(self.state.store.links(project)?, named)?;
        let (database, queue, db_name) = (link.database, link.queue, link.name);
        let (s, project_id): (Arc<State>, String) = (Arc::clone(&self.state), project.to_owned());
        let row = self.block(async move {
            let mut client = s
                .connect(&project_id, &database, None, &mut Vec::new())
                .await?;
            db::job(&mut client, &project_id, &queue, id).await
        })?;
        let row = row.ok_or_else(|| {
            format!("There is no job {id} in the queue of this project's database {db_name}.")
        })?;
        Ok(json!({
            "database": db_name,
            "id": row.id,
            "project_name": row.project_name,
            "work_type": row.kind,
            "status": row.status,
            "result": row.result,
            "reason": row.reason,
        }))
    }
}

/// A Cloud SQL link from the options, trimmed as the sheet keeps them.
fn cloudsql_of(o: &Options) -> Result<CloudSql> {
    let connection_name = o.value("--cloudsql").unwrap_or_default();
    InstanceName::parse(&connection_name)?;
    let ip_type = match o.value("--ip") {
        None => IpType::Public,
        Some(ip) => IpType::from_word(&ip.to_ascii_uppercase())
            .ok_or_else(|| format!("--ip is public, private or psc, not {ip:?}."))?,
    };
    let credential = if o.on("--adc") {
        GoogleCredential::Adc
    } else {
        GoogleCredential::Key(o.value("--key").unwrap_or_default())
    };
    Ok(CloudSql {
        connection_name,
        database: o.value("--database").unwrap_or_default(),
        user: o.value("--user").unwrap_or_default(),
        ip_type,
        iam_auth: o.on("--iam"),
        credential,
        password_secret: String::new(),
    })
}

/// The Cloud SQL setup steps for these options, then the link that fits
/// them, as text for the agent.
fn guide(sql: &CloudSql) -> String {
    let steps = cloudsql_setup_steps(
        sql,
        "Then run `connecto works key-import <that file>`. Connecto keeps the key in its encrypted vault; delete the downloaded copy afterwards.",
    );
    let mut out = String::new();
    for step in steps {
        let _ = writeln!(out, "{}", step.title);
        if let Some(code) = step.code {
            let _ = writeln!(out, "{code}");
        }
        if !step.after.is_empty() {
            let _ = writeln!(out, "{}", step.after);
        }
        out.push('\n');
    }
    // Typed values are shell-quoted; a placeholder stays as it is.
    let or = |value: &str, placeholder: &str| {
        if value.is_empty() {
            placeholder.to_owned()
        } else {
            shell_word(value)
        }
    };
    let mut link = format!(
        "connecto works link --cloudsql {} --database {}",
        or(&sql.connection_name, "<project:region:instance>"),
        or(&sql.database, "<DB>")
    );
    if !sql.user.is_empty() || !sql.iam_auth {
        let _ = write!(link, " --user {}", or(&sql.user, "<USER>"));
    }
    match &sql.credential {
        GoogleCredential::Adc => link.push_str(" --adc"),
        GoogleCredential::Key(name) if !name.is_empty() => {
            let _ = write!(link, " --key {}", shell_word(name));
        }
        GoogleCredential::Key(_) => {}
    }
    if sql.ip_type != IpType::Public {
        let _ = write!(link, " --ip {}", sql.ip_type.word().to_ascii_lowercase());
    }
    if sql.iam_auth {
        link.push_str(" --iam");
    }
    link.push_str(" --create");
    let _ = writeln!(
        out,
        "Then check it with `connecto works test-db` and the same options (without --create), and link:\n{link}"
    );
    if !sql.iam_auth {
        out.push_str("The first time, Connecto asks the user for the database user's password: end your turn and run the command again once they answered.\n");
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn options_take_known_flags_once() {
        let o = Options::parse(&words(&["summary", "--cli", "claude"]), &["--cli"]).unwrap();
        assert_eq!(o.one("test", "type").unwrap(), "summary");
        assert_eq!(o.value("--cli").as_deref(), Some("claude"));
        assert!(Options::parse(&words(&["--cli", "a", "--cli", "b"]), &["--cli"]).is_err());
        assert!(Options::parse(&words(&["--model", "a"]), &["--cli"]).is_err());
        assert!(Options::parse(&words(&["--cli"]), &["--cli"]).is_err());
        let o = Options::parse(&words(&["--create"]), &["--create"]).unwrap();
        assert!(o.on("--create"));
        assert!(Options::parse(&words(&["--password-secret", "P"]), DATABASE).is_err());
    }

    #[test]
    fn a_definition_is_the_author_reply_form() {
        let def = definition_of(Some(
            r#"{"instructions":"Sum {{prompt}}","schema":{"type":"object"}}"#,
        ))
        .unwrap();
        assert_eq!(def.setup, None);
        assert_eq!(def.prepare, None);
        assert!(check(&def).is_ok());
        let def = definition_of(Some(
            r#"{"instructions":"Sum {{prompt}} {{prepared}}","schema":{"type":"object"},"prepare":{"argv":["tool/p.py"]}}"#,
        ))
        .unwrap();
        assert!(check(&def).unwrap().prepare.is_some());
        assert!(definition_of(Some(r#"{"schema":{}}"#)).is_err());
        assert!(definition_of(None).is_err());
        assert!(
            check(&definition_of(Some(r#"{"instructions":"no","schema":{}}"#)).unwrap()).is_err()
        );
    }

    fn sql(args: &[&str]) -> Result<CloudSql> {
        cloudsql_of(&Options::parse(&words(args), DATABASE).unwrap())
    }

    #[test]
    fn cloudsql_options_are_checked_and_the_guide_fits_them() {
        let s = sql(&[
            "--cloudsql",
            "p:europe-west2:i",
            "--database",
            "app",
            "--ip",
            "private",
            "--iam",
            "--adc",
        ])
        .unwrap();
        assert_eq!(s.ip_type, IpType::Private);
        assert!(s.iam_auth);
        assert_eq!(s.credential, GoogleCredential::Adc);
        assert!(sql(&["--cloudsql", "p:r:i", "--ip", "public "]).is_ok());
        let bad = sql(&["--cloudsql", "p:r:i", "--ip", "vpn"]).unwrap_err();
        assert!(bad.contains("--ip is public, private or psc"), "{bad}");
        assert!(sql(&["--cloudsql", "not-a-name"]).is_err());

        let password = guide(
            &sql(&[
                "--cloudsql",
                "p:r:i",
                "--database",
                "app",
                "--user",
                "works",
            ])
            .unwrap(),
        );
        assert!(
            password.contains("gcloud iam service-accounts create"),
            "{password}"
        );
        assert!(password.contains("connecto works key-import"), "{password}");
        assert!(password.contains("\"works\""), "{password}");
        assert!(password.contains("--user works --create"), "{password}");
        assert!(password.contains("asks the user for the database user's password"));
        assert!(!password.contains("--password-secret"));
        // IAM: no password, no user to type.
        let iam = guide(&sql(&["--cloudsql", "p:r:i", "--database", "app", "--iam"]).unwrap());
        assert!(
            iam.contains("--iam --create") && !iam.contains("--user"),
            "{iam}"
        );
        assert!(!iam.contains("password"), "{iam}");
        // My gcloud login: no key to make or import.
        let adc = guide(
            &sql(&[
                "--cloudsql",
                "p:r:i",
                "--database",
                "app",
                "--user",
                "u",
                "--adc",
            ])
            .unwrap(),
        );
        assert!(
            adc.contains("gcloud auth application-default login"),
            "{adc}"
        );
        assert!(
            !adc.contains("key-import") && adc.contains(" --adc"),
            "{adc}"
        );
    }

    /// A vault as the app opens one, in memory, with `secrets` of the
    /// project `p` used as `binding`.
    fn vault(secrets: &[(&str, &str, Binding)]) -> connecto_vault::Vault {
        let store = connecto_secrets::Secrets::new(
            Arc::new(connecto_secrets::MemoryItems::default()),
            None,
        );
        let vault = connecto_vault::Vault::open(
            Arc::new(store),
            Arc::new(connecto_vault::FixedKey([7; 32])),
            None,
        );
        let scope = connecto_core::ProviderScope::Project("p".into());
        for (name, value, binding) in secrets {
            vault.put(&scope, name, "test", value).unwrap();
            vault
                .set_bindings(&scope, name, vec![binding.clone()])
                .unwrap();
        }
        vault
    }

    #[test]
    fn replies_hide_the_vaults_values_and_json_stays_json() {
        let dsn = "postgresql://u:s3cr\"et@h/db";
        let v = vault(&[
            ("DB", dsn, Binding::AgenticWorks),
            // The shortest password the vault keeps, all digits: a JSON
            // number in a result.
            ("PIN", "12345678", Binding::AgenticWorks),
            (
                "TOKEN",
                "tok-abcdefghijk",
                Binding::Http {
                    base_url: "https://api.example.com".into(),
                    header: "Authorization".into(),
                    prefix: "Bearer ".into(),
                },
            ),
        ]);
        // No shorter value can be in the vault, so none can escape the
        // forms it hides: it refuses one, typed or given to a request.
        let scope = connecto_core::ProviderScope::Project("p".into());
        assert!(v.put(&scope, "SHORT", "test", "123456").is_err());
        let hider = Hider::new(&v, "p");
        let text = hider.text(&format!(
            "failed: {dsn}, pin 12345678, tok-abcdefghijk, order 123456789\u{1b}[31m"
        ));
        assert!(
            !text.contains("s3cr") && !text.contains("tok-abc"),
            "{text}"
        );
        assert!(!text.contains("pin 12345678"), "{text}");
        assert!(!text.contains('\u{1b}'));
        let result = json!({
            "answer": dsn,
            "pin": 12_345_678,
            "pin_text": "12345678",
            "note": "the pin is 12345678.",
            "escaped": serde_json::to_string(dsn).unwrap(),
            "12345678": true,
            "fine": 42,
        });
        let shown = serde_json::to_string(&hider.json(result)).unwrap();
        let back: Value = serde_json::from_str(&shown).unwrap();
        for key in ["answer", "pin", "pin_text"] {
            assert_eq!(back[key], HIDDEN, "{key}: {shown}");
        }
        assert_eq!(back["note"], "the pin is [REDACTED].");
        assert_eq!(back[HIDDEN], true);
        assert_eq!(back["fine"], 42);
        assert!(
            !shown.contains("s3cr") && !shown.contains("12345678"),
            "{shown}"
        );
    }

    /// A value kept before the vault's minimum (or a form of one shorter
    /// than it) is hidden where it stands alone, so output stays usable.
    #[test]
    fn a_short_value_is_hidden_where_it_stands_alone() {
        let hider = Hider::of(&["123456".to_owned()]);
        let text = hider.text("pin 123456, (123456), order 1234567, a123456");
        assert_eq!(text, "pin [REDACTED], ([REDACTED]), order 1234567, a123456");
        let back = hider.json(json!({"pin": 123_456, "s": "123456", "n": 1_234_567, "123456": 1}));
        assert_eq!(
            back,
            json!({"pin": HIDDEN, "s": HIDDEN, "n": 1_234_567, HIDDEN: 1})
        );
    }

    #[test]
    fn typed_values_are_quoted_for_the_shell_and_for_sql() {
        assert_eq!(shell_word("app"), "app");
        assert_eq!(shell_word("p:europe-west2:i"), "p:europe-west2:i");
        assert_eq!(shell_word("my db"), "'my db'");
        assert_eq!(shell_word("o'brien; rm -rf ~"), "'o'\\''brien; rm -rf ~'");
        assert_eq!(shell_word("$(id)"), "'$(id)'");
        assert_eq!(sql_text("Bob's project"), "'Bob''s project'");
        let g = guide(
            &sql(&[
                "--cloudsql",
                "p:r:i",
                "--database",
                "my db",
                "--user",
                "o'brien",
                "--key",
                "k ey",
            ])
            .unwrap(),
        );
        assert!(
            g.contains("--database 'my db' --user 'o'\\''brien' --key 'k ey' --create"),
            "{g}"
        );
    }

    fn caller(launched: Option<AgentMode>) -> WorksCaller {
        WorksCaller {
            terminal: "claude-1".into(),
            project_id: "00000000000000ab".into(),
            launched,
            job: false,
            attempt: 1,
        }
    }

    const DESTINATION: &[&str] = &[
        "--cloudsql",
        "p:europe-west2:i",
        "--database",
        "app",
        "--user",
        "works",
        "--adc",
    ];

    /// Eight commands at once for one destination: one request, one record,
    /// and the answer is usable there.
    #[test]
    fn simultaneous_password_requests_make_one_usable_binding() {
        let (_temp, service) = super::super::tests::light();
        let who = caller(Some(AgentMode::Bypass));
        let project = who.project_id.clone();
        let args = words(DESTINATION);
        let replies: Vec<Result<String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| service.command(&who, "test-db", &args, None, Box::new(|_| {})))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let asked = replies
            .iter()
            .filter(|r| r.as_ref().is_err_and(|e| e.contains("Asked the user")))
            .count();
        let already = replies
            .iter()
            .filter(|r| r.as_ref().is_err_and(|e| e.contains("already asked")))
            .count();
        assert_eq!((asked, already), (1, 7), "{replies:?}");
        // Another terminal is told who gets the notice, not to wait for one.
        let mut second = who.clone();
        second.terminal = "claude-2".into();
        let told = service
            .command(&second, "test-db", &args, None, Box::new(|_| {}))
            .unwrap_err();
        assert!(
            told.contains("for terminal claude-1") && told.contains("not this one"),
            "{told}"
        );
        let vault = &service.state.vault;
        let asks = VaultControl::view(&**vault).asks.clone();
        assert_eq!(asks.len(), 1);
        let bound = service
            .state
            .store
            .password_for(&project, "p:europe-west2:i", "works")
            .unwrap();
        assert_eq!(bound.as_deref(), Some(asks[0].name.as_str()));
        vault
            .answer(
                asks[0].id,
                connecto_core::AskAnswer::Provide {
                    scope: connecto_core::ProviderScope::Project(project.clone()),
                    value: "a-long-password".into(),
                    bindings: vec![Binding::AgenticWorks],
                },
            )
            .unwrap();
        let o = Options::parse(&args, DATABASE).unwrap();
        let target = service.target(&who, &o, &mut None).unwrap().unwrap();
        assert_eq!(target.password.as_deref(), Some("a-long-password"));
        assert_eq!(target.asked.as_deref(), Some(asks[0].name.as_str()));
        // Another user on that instance gets nothing of it.
        let mut other = words(DESTINATION);
        other[5] = "someone".into();
        let o = Options::parse(&other, DATABASE).unwrap();
        // A request that cannot be made leaves no record behind.
        assert!(service.target(&who, &o, &mut None).is_err());
        assert_eq!(
            service
                .state
                .store
                .password_for(&project, "p:europe-west2:i", "someone")
                .unwrap(),
            None
        );
        // Every name Connecto gives a password is refused as a connection
        // string, recorded or not.
        for name in [
            asks[0].name.as_str(),
            "cloudsql-pw-0000",
            "cloudsql-password-1111",
        ] {
            let o = Options::parse(&words(&["--secret", name]), DATABASE).unwrap();
            let refused = service.target(&who, &o, &mut None).err().unwrap();
            assert!(
                refused.contains("is a Cloud SQL password"),
                "{name}: {refused}"
            );
        }
    }

    /// An import replaces the key kept for its account: only an agent
    /// started in Bypass mode may make one, and a shell may not.
    #[test]
    fn key_import_needs_bypass() {
        let (_temp, service) = super::super::tests::light();
        let args = words(&["key.json"]);
        for launched in [Some(AgentMode::Prompt), Some(AgentMode::Auto), None] {
            let refused = service
                .command(
                    &caller(launched),
                    "key-import",
                    &args,
                    Some("{}"),
                    Box::new(|_| {}),
                )
                .unwrap_err();
            assert!(refused.contains("Not imported"), "{launched:?}: {refused}");
        }
        // Bypass gets past the rule, to the key's own check.
        let checked = service
            .command(
                &caller(Some(AgentMode::Bypass)),
                "key-import",
                &args,
                Some("{}"),
                Box::new(|_| {}),
            )
            .unwrap_err();
        assert!(!checked.contains("Not imported"), "{checked}");
        assert!(
            service
                .state
                .vault
                .host_secrets("00000000000000ab", &Binding::GoogleServiceAccount)
                .is_empty()
        );
    }

    #[test]
    fn db_values_are_explicit() {
        let list = |args: &[&str]| Options::parse(&words(args), &["--db"]).unwrap().db_list();
        assert_eq!(list(&[]).unwrap(), None);
        assert_eq!(list(&["--db", "all"]).unwrap(), Some(None));
        assert_eq!(list(&["--db", " ALL "]).unwrap(), Some(None));
        assert_eq!(
            list(&["--db", "dev, staging"]).unwrap(),
            Some(Some(vec!["dev".to_owned(), "staging".to_owned()]))
        );
        for blank in ["", " ", "dev,", ",dev", "dev,,staging", "dev,all"] {
            assert!(list(&["--db", blank]).is_err(), "{blank:?}");
        }
        let one = |args: &[&str]| Options::parse(&words(args), &["--db"]).unwrap().db();
        assert_eq!(one(&[]).unwrap(), None);
        assert_eq!(one(&["--db", "dev"]).unwrap().as_deref(), Some("dev"));
        assert!(one(&["--db", ""]).is_err());
        assert!(one(&["--db", "all"]).is_err());
    }

    /// A definition handed to the user keeps the databases asked for, and
    /// a blank --db is refused before anything is handed over.
    #[test]
    fn a_handed_over_definition_keeps_its_databases() {
        let (_temp, service) = super::super::tests::light();
        let who = caller(Some(AgentMode::Prompt));
        let def = r#"{"instructions":"Sum {{prompt}}","schema":{"type":"object"}}"#;
        let publish = |args: &[&str]| {
            service.command(&who, "publish", &words(args), Some(def), Box::new(|_| {}))
        };
        let told = publish(&["summary", "--db", "dev"]).unwrap_err();
        assert!(told.contains("the databases dev only"), "{told}");
        assert_eq!(
            service
                .state
                .store
                .author_of(&who.project_id, "summary")
                .unwrap(),
            ("claude-1".to_owned(), Some(vec!["dev".to_owned()]))
        );
        let told = publish(&["summary"]).unwrap_err();
        assert!(told.contains("every linked database"), "{told}");
        assert!(
            publish(&["summary", "--db", ""])
                .unwrap_err()
                .contains("blank")
        );
        assert!(
            publish(&["summary", "--db", "dev,"])
                .unwrap_err()
                .contains("blank")
        );
    }

    #[test]
    fn names_and_changes() {
        assert!(kind_name("summary").is_ok());
        assert!(kind_name(" ").is_err());
        assert!(kind_name("a\nb").is_err());
        for verb in ["link", "publish", "test", "setup", "key-import"] {
            assert!(changes(verb));
        }
        assert!(!changes("show") && !changes("job"));
    }
}
