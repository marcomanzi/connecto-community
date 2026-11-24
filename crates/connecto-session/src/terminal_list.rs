//! The saved terminal list, `<data>/projects/<id>/terminals.json` (ADR 0014).
//!
//! Pure data and a small IO seam. The registry thread is the only caller, so
//! nothing on the UI thread touches the disk.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use connecto_core::{MAX_NAME_CHARS, MAX_TERMINALS};
use connecto_store::Tool;
use serde_json::{Value, json};

/// The file name inside a project's folder.
pub(crate) const FILE: &str = "terminals.json";
const VERSION: u64 = 1;

/// One saved terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub id: String,
    pub tool: Tool,
    /// Whether it starts at the next launch.
    pub running: bool,
    /// What the user or the starting agent called it.
    pub name: Option<String>,
    /// The top-level terminal whose agent started this one.
    pub parent: Option<String>,
    /// The terminal that actually asked, when that was a child.
    pub asked_by: Option<String>,
    /// The tool's own session id, for resume. Only a checked value.
    pub session: Option<String>,
    /// The model it was added with (`connecto add --model`), kept so a later
    /// start uses it too. Only a checked value.
    pub model: Option<String>,
    /// The effort it was added with (`connecto add --effort`), kept like the
    /// model. Only a value its tool takes.
    pub effort: Option<String>,
    /// Whether it was in the middle of a turn when last seen. Such a terminal
    /// starts with its project at the next launch; the others wait until
    /// they are asked for.
    pub working: bool,
    /// The app closed on it in the middle of a turn, and it has not started
    /// another since. Kept across launches until it does.
    pub interrupted: bool,
    /// The turn it was in was cut by a restart for an update (ADR 0030): the
    /// next launch resumes its conversation with "continua". Saved as
    /// `"cut_by": "update"`; taken, and dropped, by the launch that does it.
    pub cut_by_update: bool,
    /// The `--ask` replies this terminal owed when an update cut the app: the
    /// asker waits for them across the restart (ADR 0030). Saved as
    /// `"owes": [{"request": "r13", "to": "claude-31"}]`; taken, and dropped,
    /// by the launch that restores them.
    pub owes: Vec<OwedReply>,
    /// It runs an Agentic Works job, or a terminal that ran one started it
    /// (at any depth). Set when it is created, never cleared: a close, a
    /// restart or a new parent leaves it. Saved as `"job": true`.
    pub job: bool,
}

/// An `--ask` reply a terminal still owed when an update cut the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwedReply {
    /// The request id the asker was given (`r13`).
    pub request: String,
    /// The terminal that asked and waits for the reply.
    pub to: String,
}

/// A request id as the bridge gives one out: `r` and a number.
pub(crate) fn valid_request(id: &str) -> bool {
    id.strip_prefix('r')
        .is_some_and(|n| (1..=20).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()))
}

impl Entry {
    /// A terminal with nothing but an id, a tool and a state.
    pub(crate) const fn new(id: String, tool: Tool, running: bool) -> Self {
        Self {
            id,
            tool,
            running,
            name: None,
            parent: None,
            asked_by: None,
            session: None,
            model: None,
            effort: None,
            working: false,
            interrupted: false,
            cut_by_update: false,
            owes: Vec::new(),
            job: false,
        }
    }

    /// The effort it launches with: its own, or, for the app session (which
    /// the phone starts with none), [`APP_SESSION_EFFORT`] where its tool
    /// takes it, so its analyses go deeper. Never saved: an app session made
    /// before this launches with it too.
    pub(crate) fn launch_effort(&self) -> Option<String> {
        self.effort.clone().or_else(|| {
            (self.name.as_deref() == Some(connecto_core::session::APP_SESSION)
                && self.parent.is_none()
                && connecto_core::ToolKind::from_name(self.tool.name())
                    .is_some_and(|kind| valid_effort(kind, APP_SESSION_EFFORT)))
            .then(|| APP_SESSION_EFFORT.to_owned())
        })
    }
}

/// The effort an app session launches with when it was given none.
pub(crate) const APP_SESSION_EFFORT: &str = "high";

/// A name as a person or an agent gave it, or why it is refused. Blank means
/// no name. Too long is refused, never cut.
pub(crate) fn clean_name(name: &str) -> Result<Option<String>, String> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("a name is at most {MAX_NAME_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("a name is one line of plain text".to_owned());
    }
    Ok(Some(name.to_owned()))
}

/// A tool session id is passed on a command line, so only plain id characters.
pub(crate) fn valid_session(id: &str) -> bool {
    // The first character is a letter or digit, so an id can never be read as
    // an option (`--last` would reopen the newest conversation).
    (1..=128).contains(&id.len())
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// A model name is passed on a command line, so only plain name characters,
/// and never a leading `-` that could be read as an option. `/` and `:` are
/// for Pi, which names a model with its provider and a thinking level
/// (`lmstudio/google/gemma-4-e2b`, `openrouter/openai/gpt-5:high`).
pub(crate) fn valid_model(model: &str) -> bool {
    (1..=64).contains(&model.len())
        && model.as_bytes()[0].is_ascii_alphanumeric()
        && model.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'[' | b']' | b'/' | b':')
        })
}

/// Whether `tool`'s command line takes `effort` as its reasoning effort.
pub(crate) fn valid_effort(tool: connecto_core::ToolKind, effort: &str) -> bool {
    use connecto_core::ToolKind;
    let levels: &[&str] = match tool {
        ToolKind::Claude | ToolKind::Antigravity => &["low", "medium", "high", "xhigh", "max"],
        // Codex 0.167 offers `max` and `ultra` on its newest models.
        ToolKind::Codex => &[
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ],
        ToolKind::Pi => &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
        // OpenCode's TUI takes no effort: a model's variant is chosen in it.
        ToolKind::OpenCode | ToolKind::Shell => &[],
    };
    levels.contains(&effort)
}

/// The saved list. Ids are never reused: `next` only moves forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct List {
    pub next: BTreeMap<String, u32>,
    pub terminals: Vec<Entry>,
}

pub(crate) fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn tool_from(name: &str) -> Option<Tool> {
    [
        Tool::Claude,
        Tool::Codex,
        Tool::Pi,
        Tool::Antigravity,
        Tool::OpenCode,
        Tool::Shell,
    ]
    .into_iter()
    .find(|tool| tool.name() == name)
}

impl List {
    /// One terminal per tool given, all running.
    pub(crate) fn defaults(tools: &[Tool]) -> Self {
        let mut list = Self {
            next: BTreeMap::new(),
            terminals: Vec::new(),
        };
        for tool in tools {
            // Fresh counters cannot be exhausted.
            let Some(id) = list.mint(*tool) else { continue };
            list.terminals.push(Entry::new(id, *tool, true));
        }
        list
    }

    /// A new id for `tool`: the first is the tool's name, then `<tool>-<n>`.
    /// Never an id the list has used. `None` when the counter is exhausted: it
    /// never wraps, so an id is never handed out twice.
    pub(crate) fn mint(&mut self, tool: Tool) -> Option<String> {
        loop {
            let counter = self.next.entry(tool.name().to_owned()).or_insert(1);
            let number = *counter;
            *counter = number.checked_add(1)?;
            let id = if number == 1 {
                tool.name().to_owned()
            } else {
                format!("{}-{number}", tool.name())
            };
            if !self.terminals.iter().any(|entry| entry.id == id) {
                return Some(id);
            }
        }
    }

    pub(crate) fn to_json(&self) -> String {
        let terminals: Vec<Value> = self
            .terminals
            .iter()
            .map(|entry| {
                let mut value = json!({
                    "id": entry.id,
                    "tool": entry.tool.name(),
                    "desired": if entry.running { "running" } else { "stopped" },
                });
                for (field, text) in [
                    ("name", &entry.name),
                    ("parent", &entry.parent),
                    ("asked_by", &entry.asked_by),
                    ("session", &entry.session),
                    ("model", &entry.model),
                    ("effort", &entry.effort),
                ] {
                    if let Some(text) = text {
                        value[field] = json!(text);
                    }
                }
                for (field, on) in [
                    ("working", entry.working),
                    ("interrupted", entry.interrupted),
                ] {
                    if on {
                        value[field] = json!(true);
                    }
                }
                if entry.cut_by_update {
                    value["cut_by"] = json!("update");
                }
                if entry.job {
                    value["job"] = json!(true);
                }
                if !entry.owes.is_empty() {
                    value["owes"] = entry
                        .owes
                        .iter()
                        .map(|owed| json!({"request": owed.request, "to": owed.to}))
                        .collect();
                }
                value
            })
            .collect();
        let mut text = serde_json::to_string_pretty(&json!({
            "version": VERSION,
            "next": self.next,
            "terminals": terminals,
        }))
        .unwrap_or_default();
        text.push('\n');
        text
    }

    /// Parse a saved list, or say why it is unreadable: bad JSON, an unknown
    /// version or tool, more than the cap, or a bad or duplicate id. An empty
    /// list is a list: a project may have no terminals. The fields added by the
    /// project tree are optional, so a list from an older build loads as it
    /// was; a bad one of them is dropped, not a reason to lose the list.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|err| format!("bad JSON: {err}"))?;
        if value["version"].as_u64() != Some(VERSION) {
            return Err("unknown version".to_owned());
        }
        let items = value["terminals"].as_array().ok_or("no terminals list")?;
        if items.len() > MAX_TERMINALS {
            return Err(format!("more than {MAX_TERMINALS} terminals"));
        }
        let mut terminals: Vec<Entry> = Vec::new();
        for item in items {
            let id = item["id"].as_str().ok_or("a terminal has no id")?;
            if !valid_id(id) {
                return Err(format!("bad id {id:?}"));
            }
            if terminals.iter().any(|entry| entry.id == id) {
                return Err(format!("duplicate id {id}"));
            }
            let tool = item["tool"]
                .as_str()
                .and_then(tool_from)
                .ok_or_else(|| format!("unknown tool for {id}"))?;
            let running = match item["desired"].as_str() {
                Some("running") => true,
                Some("stopped") => false,
                _ => return Err(format!("bad desired state for {id}")),
            };
            let text = |field: &str| item[field].as_str().map(str::to_owned);
            let mut entry = Entry::new(id.to_owned(), tool, running);
            entry.name = text("name").and_then(|name| clean_name(&name).ok().flatten());
            entry.parent = text("parent").filter(|parent| valid_id(parent) && parent != id);
            entry.asked_by = text("asked_by").filter(|by| valid_id(by) && by != id);
            entry.session = text("session").filter(|session| valid_session(session));
            entry.model = text("model").filter(|model| valid_model(model));
            entry.effort = text("effort").filter(|effort| {
                connecto_core::ToolKind::from_name(tool.name())
                    .is_some_and(|kind| valid_effort(kind, effort))
            });
            entry.working = item["working"].as_bool().unwrap_or(false);
            entry.interrupted = item["interrupted"].as_bool().unwrap_or(false);
            entry.cut_by_update = item["cut_by"].as_str() == Some("update");
            entry.job = item["job"].as_bool() == Some(true);
            entry.owes = item["owes"]
                .as_array()
                .map(|owes| {
                    owes.iter()
                        .filter_map(|owed| {
                            let request = owed["request"].as_str()?;
                            let to = owed["to"].as_str()?;
                            (valid_request(request) && valid_id(to) && to != id).then(|| {
                                OwedReply {
                                    request: request.to_owned(),
                                    to: to.to_owned(),
                                }
                            })
                        })
                        .take(MAX_TERMINALS * 4)
                        .collect()
                })
                .unwrap_or_default();
            terminals.push(entry);
        }
        // A parent that is not in the list, or that has a parent itself, would
        // make a tree the UI does not draw: those become top level.
        let ids: Vec<String> = terminals.iter().map(|entry| entry.id.clone()).collect();
        let top: Vec<bool> = terminals
            .iter()
            .map(|entry| entry.parent.is_none())
            .collect();
        for entry in &mut terminals {
            let parent_ok = entry.parent.as_ref().is_none_or(|parent| {
                ids.iter()
                    .position(|id| id == parent)
                    .is_some_and(|at| top[at])
            });
            if !parent_ok {
                entry.parent = None;
                entry.asked_by = None;
            }
        }
        let mut next: BTreeMap<String, u32> = BTreeMap::new();
        if let Some(map) = value["next"].as_object() {
            for (name, number) in map {
                if tool_from(name).is_some()
                    && let Some(number) = number.as_u64().and_then(|n| u32::try_from(n).ok())
                {
                    next.insert(name.clone(), number.max(1));
                }
            }
        }
        // A counter behind an id in use would hand that id out again.
        for entry in &terminals {
            let name = entry.tool.name();
            let used = entry
                .id
                .strip_prefix(name)
                .map(|rest| rest.strip_prefix('-').unwrap_or(rest))
                .and_then(|rest| rest.parse::<u32>().ok())
                .unwrap_or(1);
            // An id at the top of the range leaves no counter above it.
            let above = used
                .checked_add(1)
                .ok_or_else(|| format!("the id {} leaves no number to count on from", entry.id))?;
            let counter = next.entry(name.to_owned()).or_insert(1);
            *counter = (*counter).max(above);
        }
        Ok(Self { next, terminals })
    }
}

/// The disk, behind a seam so tests can fail each step.
pub(crate) trait ListIo: Send + 'static {
    /// `Ok(None)` when there is no file.
    fn read(&self) -> io::Result<Option<String>>;
    /// Replace the file atomically.
    fn write(&self, text: &str) -> io::Result<()>;
    /// Move the file aside as `terminals.json.bad`.
    fn quarantine(&self) -> io::Result<()>;
}

/// The real file.
pub(crate) struct FsIo {
    path: PathBuf,
}

impl FsIo {
    pub(crate) const fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ListIo for FsIo {
    fn read(&self) -> io::Result<Option<String>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn write(&self, text: &str) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let staged = self.path.with_extension("json.tmp");
        fs::write(&staged, text)?;
        fs::rename(&staged, &self.path)
    }

    fn quarantine(&self) -> io::Result<()> {
        fs::rename(&self.path, self.path.with_extension("json.bad"))
    }
}

/// What loading found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Loaded {
    /// A readable file.
    Read(List),
    /// No file: the defaults, and nothing to warn about.
    Fresh(List),
    /// The content was unusable. The file moved to `.bad`, the defaults are in
    /// use, and the warning says so.
    Bad { list: List, warning: String },
    /// The file could not be read, or could not be moved aside. It may be
    /// sound, so it is never written this run: read-only list mode.
    ReadOnly { list: List, warning: String },
}

pub(crate) fn load(io: &dyn ListIo, defaults: &[Tool]) -> Loaded {
    let fallback = || List::defaults(defaults);
    let text = match io.read() {
        Ok(Some(text)) => text,
        Ok(None) => return Loaded::Fresh(fallback()),
        Err(err) => {
            return Loaded::ReadOnly {
                list: fallback(),
                warning: format!(
                    "The saved terminal list could not be read ({err}). The default terminals are \
                     shown, and the list will not be changed this run."
                ),
            };
        }
    };
    match List::parse(&text) {
        Ok(list) => Loaded::Read(list),
        Err(why) => match io.quarantine() {
            Ok(()) => Loaded::Bad {
                list: fallback(),
                warning: format!(
                    "The saved terminal list was unusable ({why}). It was kept as terminals.json.bad, \
                     and the default terminals are shown."
                ),
            },
            Err(err) => Loaded::ReadOnly {
                list: fallback(),
                warning: format!(
                    "The saved terminal list was unusable ({why}) and could not be moved aside \
                     ({err}). The default terminals are shown, and the list will not be changed \
                     this run."
                ),
            },
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_replies_a_terminal_owes_are_saved_and_bad_ones_dropped() {
        let mut list = List::defaults(&[Tool::Claude, Tool::Codex]);
        let owed = OwedReply {
            request: "r13".to_owned(),
            to: "codex".to_owned(),
        };
        list.terminals[0].owes = vec![owed.clone()];
        let back = List::parse(&list.to_json()).unwrap();
        assert_eq!(back.terminals[0].owes, vec![owed]);
        assert!(back.terminals[1].owes.is_empty());
        let text = list.to_json().replace("\"r13\"", "\"x13\"");
        assert!(List::parse(&text).unwrap().terminals[0].owes.is_empty());
        assert!(valid_request("r1") && !valid_request("r") && !valid_request("r1a"));
    }

    #[test]
    fn a_shell_is_saved_and_loaded_under_its_own_name() {
        let mut list = List::defaults(&[Tool::Claude]);
        assert_eq!(list.mint(Tool::Shell).as_deref(), Some("shell"));
        let id = list.mint(Tool::Shell).unwrap();
        assert_eq!(id, "shell-2");
        list.terminals.push(Entry::new(id, Tool::Shell, true));
        let again = List::parse(&list.to_json()).unwrap();
        let shell = again.terminals.iter().find(|e| e.id == "shell-2").unwrap();
        assert_eq!(shell.tool, Tool::Shell);
        assert!(shell.running);
    }

    fn both() -> [Tool; 2] {
        [Tool::Claude, Tool::Codex]
    }

    #[test]
    fn working_is_kept_and_is_off_when_absent() {
        let text = r#"{"version":1,"next":{},"terminals":[
            {"id":"claude","tool":"claude","desired":"running","working":true},
            {"id":"codex","tool":"codex","desired":"running","interrupted":true}]}"#;
        let list = List::parse(text).unwrap();
        assert!(list.terminals[0].working && !list.terminals[1].working);
        assert!(!list.terminals[0].interrupted && list.terminals[1].interrupted);
        let again = List::parse(&list.to_json()).unwrap();
        assert_eq!(again, list);
        assert_eq!(list.to_json().matches("working").count(), 1);
        assert_eq!(list.to_json().matches("interrupted").count(), 1);
    }

    /// The job mark is saved and read back; a list from before it has none.
    #[test]
    fn the_job_mark_is_kept_and_older_lists_have_none() {
        let text = r#"{"version":1,"terminals":[
            {"id":"claude","tool":"claude","desired":"running","job":true},
            {"id":"codex","tool":"codex","desired":"running"},
            {"id":"pi","tool":"pi","desired":"running","job":"yes"},
            {"id":"claude-2","tool":"claude","desired":"running","parent":"gone","job":true}]}"#;
        let list = List::parse(text).unwrap();
        let marks: Vec<bool> = list.terminals.iter().map(|e| e.job).collect();
        assert_eq!(marks, [true, false, false, true]);
        // A parent no longer listed is dropped; the mark is not.
        assert_eq!(list.terminals[3].parent, None);
        let saved = list.to_json();
        assert_eq!(saved.matches("\"job\"").count(), 2, "{saved}");
        let again = List::parse(&saved).unwrap();
        assert_eq!(
            again.terminals.iter().map(|e| e.job).collect::<Vec<_>>(),
            marks
        );
    }

    #[test]
    fn a_turn_cut_by_an_update_is_kept_and_nothing_else_reads_as_one() {
        let text = r#"{"version":1,"next":{},"terminals":[
            {"id":"claude","tool":"claude","desired":"running","working":true,"cut_by":"update"},
            {"id":"codex","tool":"codex","desired":"running","working":true,"cut_by":"crash"}]}"#;
        let list = List::parse(text).unwrap();
        assert!(list.terminals[0].cut_by_update);
        assert!(!list.terminals[1].cut_by_update);
        let again = List::parse(&list.to_json()).unwrap();
        assert_eq!(again, list);
        assert_eq!(list.to_json().matches("cut_by").count(), 1);
    }

    #[test]
    fn a_session_id_cannot_be_an_option() {
        for bad in ["--last", "-c", "--", "-", "_x", ""] {
            assert!(!valid_session(bad), "{bad:?}");
        }
        for good in ["0199-abc_1", "a", "0b4d1f2e-9c1a-4d6e-8f10-aa00bb11cc22"] {
            assert!(valid_session(good), "{good}");
        }
    }

    #[test]
    fn a_model_cannot_be_an_option_and_is_kept_with_its_terminal() {
        for bad in [
            "--resume",
            "-m",
            "",
            "opus 5",
            "a;b",
            "a\"b",
            &"x".repeat(65),
        ] {
            assert!(!valid_model(bad), "{bad:?}");
        }
        for good in [
            "opus",
            "haiku",
            "claude-opus-5-5",
            "gpt-6.1-sol",
            "opus[1m]",
        ] {
            assert!(valid_model(good), "{good}");
        }
        for tool in connecto_core::ToolKind::ALL {
            for suggested in tool.suggested_models() {
                assert!(valid_model(suggested), "{tool:?} offers {suggested}");
            }
        }
        let mut list = List::defaults(&both());
        list.terminals[0].model = Some("opus".to_owned());
        let back = List::parse(&list.to_json()).unwrap();
        assert_eq!(back.terminals[0].model.as_deref(), Some("opus"));
        assert_eq!(back.terminals[1].model, None);
    }

    /// A top-level app session with no effort of its own launches on high;
    /// one it was given, a child of that name and other terminals keep theirs.
    #[test]
    fn the_app_session_launches_on_high_unless_given_an_effort() {
        let app = |tool, effort: Option<&str>, parent: Option<&str>| {
            let mut entry = Entry::new("t-1".into(), tool, true);
            entry.name = Some(connecto_core::session::APP_SESSION.into());
            entry.effort = effort.map(str::to_owned);
            entry.parent = parent.map(str::to_owned);
            entry.launch_effort()
        };
        assert_eq!(app(Tool::Claude, None, None).as_deref(), Some("high"));
        assert_eq!(app(Tool::Codex, None, None).as_deref(), Some("high"));
        assert_eq!(app(Tool::Claude, Some("low"), None).as_deref(), Some("low"));
        assert_eq!(app(Tool::Claude, None, Some("claude")), None);
        let plain = Entry::new("claude".into(), Tool::Claude, true);
        assert_eq!(plain.launch_effort(), None);
    }

    /// `connecto add --effort` is kept with its terminal, so a later start,
    /// and the next launch of the app, use it too. A word its tool doesn't
    /// take is dropped on reading.
    #[test]
    fn an_effort_is_kept_with_its_terminal_and_checked_against_its_tool() {
        use connecto_core::ToolKind;
        for tool in [
            ToolKind::Claude,
            ToolKind::Codex,
            ToolKind::Antigravity,
            ToolKind::Pi,
        ] {
            assert!(valid_effort(tool, "medium"), "{tool:?}");
            assert!(!valid_effort(tool, "Medium"), "{tool:?}");
            assert!(!valid_effort(tool, "--model"), "{tool:?}");
        }
        assert!(valid_effort(ToolKind::Claude, "max"));
        assert!(valid_effort(ToolKind::Codex, "ultra"));
        assert!(!valid_effort(ToolKind::Claude, "ultra"));
        assert!(!valid_effort(ToolKind::Shell, "medium"));
        let mut list = List::defaults(&both());
        list.terminals[0].effort = Some("medium".to_owned());
        list.terminals[1].effort = Some("bogus".to_owned());
        let back = List::parse(&list.to_json()).unwrap();
        assert_eq!(back.terminals[0].effort.as_deref(), Some("medium"));
        assert_eq!(back.terminals[1].effort, None);
    }

    #[test]
    fn the_default_list_is_one_of_each_and_ids_follow_the_counters() {
        let mut list = List::defaults(&both());
        assert_eq!(
            list.terminals
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["claude", "codex"]
        );
        assert_eq!(list.mint(Tool::Claude).as_deref(), Some("claude-2"));
        assert_eq!(list.mint(Tool::Claude).as_deref(), Some("claude-3"));
        assert_eq!(list.mint(Tool::Codex).as_deref(), Some("codex-2"));
    }

    #[test]
    fn a_list_round_trips_and_ids_stay_unused_after_a_removal() {
        let mut list = List::defaults(&both());
        let id = list.mint(Tool::Claude).unwrap();
        list.terminals.push(Entry::new(id, Tool::Claude, false));
        list.terminals.remove(0);
        let again = List::parse(&list.to_json()).unwrap();
        assert_eq!(again, list);
        let mut again = again;
        // `claude` was removed, and is not handed out again.
        assert_eq!(again.mint(Tool::Claude).as_deref(), Some("claude-3"));
    }

    #[test]
    fn a_counter_behind_an_id_in_use_is_fixed_on_load() {
        let text = r#"{"version":1,"next":{"claude":1},"terminals":
            [{"id":"claude-4","tool":"claude","desired":"running"}]}"#;
        let mut list = List::parse(text).unwrap();
        assert_eq!(list.mint(Tool::Claude).as_deref(), Some("claude-5"));
    }

    #[test]
    fn every_kind_of_bad_content_is_refused() {
        let entry = |id: &str, tool: &str| {
            format!(r#"{{"id":"{id}","tool":"{tool}","desired":"running"}}"#)
        };
        let too_many = (0..=MAX_TERMINALS)
            .map(|n| entry(&format!("claude-{n}"), "claude"))
            .collect::<Vec<_>>()
            .join(",");
        let cases = [
            "not json".to_owned(),
            r#"{"version":2,"terminals":[]}"#.to_owned(),
            format!(r#"{{"version":1,"terminals":[{}]}}"#, entry("x", "gemini")),
            format!(
                r#"{{"version":1,"terminals":[{}]}}"#,
                entry("Bad Id", "claude")
            ),
            format!(
                r#"{{"version":1,"terminals":[{},{}]}}"#,
                entry("a", "claude"),
                entry("a", "codex")
            ),
            format!(r#"{{"version":1,"terminals":[{too_many}]}}"#),
            format!(
                r#"{{"version":1,"terminals":[{}]}}"#,
                entry(&"x".repeat(33), "claude")
            ),
        ];
        for case in cases {
            assert!(List::parse(&case).is_err(), "{case}");
        }
    }

    #[test]
    fn ids_follow_the_pattern() {
        assert!(valid_id("claude-2"));
        assert!(valid_id("a"));
        assert!(!valid_id(""));
        assert!(!valid_id("Claude"));
        assert!(!valid_id("a_b"));
    }

    /// An id at the top of the number range cannot be counted on from: the file
    /// is unusable, not a panic.
    #[test]
    fn an_id_at_the_top_of_the_range_makes_the_list_unusable() {
        let text = r#"{"version":1,"next":{},"terminals":
            [{"id":"claude-4294967295","tool":"claude","desired":"running"}]}"#;
        let err = List::parse(text).unwrap_err();
        assert!(err.contains("claude-4294967295"), "{err}");
        // One below is fine.
        let ok = r#"{"version":1,"terminals":
            [{"id":"claude-4294967294","tool":"claude","desired":"running"}]}"#;
        let mut list = List::parse(ok).unwrap();
        assert_eq!(list.next["claude"], u32::MAX);
        // The counter is at the top: it refuses instead of wrapping.
        assert_eq!(list.mint(Tool::Claude), None);
    }

    /// A saved counter at the top never wraps either way.
    #[test]
    fn an_exhausted_counter_refuses_instead_of_wrapping() {
        let text = r#"{"version":1,"next":{"codex":4294967295},"terminals":
            [{"id":"claude","tool":"claude","desired":"running"}]}"#;
        let mut list = List::parse(text).unwrap();
        assert_eq!(list.next["codex"], u32::MAX);
        assert_eq!(list.mint(Tool::Codex), None);
        assert_eq!(list.next["codex"], u32::MAX, "the counter did not move");
        assert_eq!(list.mint(Tool::Claude).as_deref(), Some("claude-2"));
    }

    /// R5: a list in today's format loads unchanged, and the new fields are
    /// absent from it.
    #[test]
    fn an_old_list_loads_and_the_new_fields_round_trip() {
        let old = r#"{"version":1,"next":{"claude":2},"terminals":
            [{"id":"claude","tool":"claude","desired":"running"},
             {"id":"codex","tool":"codex","desired":"stopped"}]}"#;
        let list = List::parse(old).unwrap();
        assert!(list.terminals.iter().all(|e| e.name.is_none()
            && e.parent.is_none()
            && e.asked_by.is_none()
            && e.session.is_none()));
        assert!(!list.to_json().contains("session"));

        let mut list = list;
        list.terminals[1].name = Some("review the diff".into());
        list.terminals[1].parent = Some("claude".into());
        list.terminals[1].asked_by = Some("claude".into());
        list.terminals[1].session = Some("0199-abc_1".into());
        assert_eq!(List::parse(&list.to_json()).unwrap(), list);
    }

    #[test]
    fn an_empty_list_is_a_list_and_a_broken_tree_is_flattened_on_load() {
        assert!(
            List::parse(r#"{"version":1,"terminals":[]}"#)
                .unwrap()
                .terminals
                .is_empty()
        );
        let text = r#"{"version":1,"terminals":[
            {"id":"a","tool":"claude","desired":"running"},
            {"id":"b","tool":"claude","desired":"running","parent":"a"},
            {"id":"c","tool":"claude","desired":"running","parent":"b","asked_by":"b"},
            {"id":"d","tool":"claude","desired":"running","parent":"gone","session":"bad id;"}]}"#;
        let list = List::parse(text).unwrap();
        let get = |id: &str| list.terminals.iter().find(|e| e.id == id).unwrap();
        assert_eq!(get("b").parent.as_deref(), Some("a"));
        assert_eq!(get("c").parent, None, "a grandchild is not a tree level");
        assert_eq!(get("c").asked_by, None);
        assert_eq!(get("d").parent, None);
        assert_eq!(
            get("d").session,
            None,
            "an id with a space or `;` is dropped"
        );
    }

    #[test]
    fn names_are_trimmed_refused_when_long_and_never_cut() {
        assert_eq!(
            clean_name("  fix tests ").unwrap().as_deref(),
            Some("fix tests")
        );
        assert_eq!(clean_name("   ").unwrap(), None);
        assert!(clean_name(&"x".repeat(MAX_NAME_CHARS)).is_ok());
        assert!(clean_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert!(clean_name("two\nlines").is_err());
    }
}
