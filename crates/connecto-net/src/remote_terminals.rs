//! A connected host's terminals, as registries the window can draw like a
//! project's. One registry per host project, so the tree of a host reads like
//! the local one: projects, then their terminals, with children under their
//! parent. A project's history, settings and token optimization are the
//! host's, read and changed through [`crate::project_panels`].
//!
//! Everything a host says is data from a peer: names are cleaned for display
//! here, and a parent is kept only when it is consistent with the list.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use connecto_core::{
    MAX_REMOTE_NAME_CHARS, RemoteProject, SaveState, TermActivity, TerminalEntry, TerminalSession,
    TerminalsControl, TerminalsView, ToolKind,
};

use crate::hosting::{activity_from_byte, tool_byte, tool_from_byte};
use crate::project_cache::{PanelKind, PanelValue};
use crate::project_panels::ProjectAsk;
use crate::reconnect::{HostHandle, RemoteTerminal};
use crate::wire::{Listing, ManageOp, ProjectInfo, TerminalInfo};

const NOT_YET: &str = "this is not available on a remote host yet";

/// One host's terminals, all of them or one project's. `add` goes to the
/// project when there is one, else to the host's active project.
pub struct RemoteTerminals {
    host: Arc<HostHandle>,
    project: Option<String>,
}

impl RemoteTerminals {
    /// Every terminal the host lists, in the host's order.
    #[must_use]
    pub const fn all(host: Arc<HostHandle>) -> Self {
        Self {
            host,
            project: None,
        }
    }

    /// The terminals of one of the host's projects, by the host's project id.
    #[must_use]
    pub fn for_project(host: Arc<HostHandle>, project: &str) -> Self {
        Self {
            host,
            project: Some(project.to_owned()),
        }
    }

    /// Hand a request to the host. It is refused at once when there is no
    /// connection; the host's answer shows in the view's `last_error`.
    fn send(&self, op: ManageOp) -> Result<(), String> {
        if !self.host.is_connected() {
            return Err("not connected to that host".to_owned());
        }
        self.host.manage_async(op);
        Ok(())
    }
}

fn parse_id(id: &str) -> Result<u32, String> {
    id.parse().map_err(|_| "no such terminal".to_owned())
}

/// Text from a peer, cleaned for display: control characters, line and
/// paragraph separators and bidirectional controls become spaces, the text is
/// trimmed, and it is cut to [`MAX_REMOTE_NAME_CHARS`] characters.
#[must_use]
pub fn display_text(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_control() || is_separator_or_bidi(c) {
                ' '
            } else {
                c
            }
        })
        .take(MAX_REMOTE_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Line and paragraph separators, and the Unicode `Bidi_Control` characters
/// (which reorder text on screen): the one set every peer-supplied name is
/// cleaned with.
const fn is_separator_or_bidi(c: char) -> bool {
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

/// The longest project id a host may use. Every host's ids are 16 hex digits.
pub const MAX_PROJECT_ID_LEN: usize = 64;

/// Whether `id` is in the one spelling a project id may have: 1 to
/// [`MAX_PROJECT_ID_LEN`] characters of lowercase ASCII letters, digits, `-` and
/// `_`. With one spelling allowed, no two ids can be aliases of each other.
#[must_use]
pub fn is_canonical_project_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_PROJECT_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Checks a host's whole listing before any of it is used. The listing is
/// refused as a whole, never in part, when:
/// - a project id is not canonical ([`is_canonical_project_id`]), or the same
///   id is listed twice;
/// - a project's name is empty once cleaned;
/// - a terminal id appears twice, whatever project or parent each copy names;
/// - a terminal names a project the listing does not give, or names it
///   differently from the listing.
///
/// Two projects with the same cleaned name are accepted: [`remote_projects`]
/// tells them apart. Entries are never dropped one by one: a partly kept
/// listing could still hide a project. A refused listing leaves the last
/// accepted one in place.
///
/// # Errors
/// With the reason, as the window shows it.
pub fn check_listing(listing: &Listing) -> Result<(), String> {
    // Project id to the name the host sent for it.
    let mut projects: HashMap<&str, &str> = HashMap::new();
    for project in &listing.projects {
        if !is_canonical_project_id(&project.id) {
            return Err("a project id is not in the form a host gives".to_owned());
        }
        if projects.insert(&project.id, &project.name).is_some() {
            return Err(format!("project {} is listed twice", project.id));
        }
        if display_text(&project.name).is_empty() {
            return Err(format!("project {} has no name", project.id));
        }
    }
    let mut ids: HashSet<u32> = HashSet::new();
    for info in &listing.terminals {
        if !ids.insert(info.id) {
            return Err(format!("terminal {} is listed twice", info.id));
        }
        match projects.get(info.project_id.as_str()) {
            None => {
                return Err(format!(
                    "terminal {} is in a project the list does not give",
                    info.id
                ));
            }
            Some(name) if *name != info.project_name => {
                return Err(format!("project {} has two names", info.project_id));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// The projects of a listing, in the host's order, each with its label as the
/// window shows it.
///
/// The common case is unchanged: a project whose cleaned name is unique shows
/// that name. When labels would be equal, each project in the clash gets its
/// canonical id in brackets after its name, and the check repeats until no two
/// labels are equal. An id contains no space or bracket, so a suffixed label
/// can never equal another suffixed label (the id is read back from its end),
/// and a name a peer chose cannot pass as a suffixed one: whatever it is, it is
/// renamed with its id if it collides. A label stays within
/// [`MAX_REMOTE_NAME_CHARS`].
#[must_use]
pub fn remote_projects(projects: &[ProjectInfo]) -> Vec<RemoteProject> {
    let bases: Vec<String> = projects.iter().map(|p| display_text(&p.name)).collect();
    let mut labels = bases.clone();
    let mut suffixed = vec![false; projects.len()];
    loop {
        let mut count: HashMap<&str, usize> = HashMap::new();
        for label in &labels {
            *count.entry(label.as_str()).or_insert(0) += 1;
        }
        let clashing: Vec<usize> = (0..labels.len())
            .filter(|&i| !suffixed[i] && count.get(labels[i].as_str()).copied().unwrap_or(0) > 1)
            .collect();
        if clashing.is_empty() {
            break;
        }
        for i in clashing {
            labels[i] = disambiguated(&bases[i], &projects[i].id);
            suffixed[i] = true;
        }
    }
    projects
        .iter()
        .zip(labels)
        .map(|(project, name)| RemoteProject {
            id: project.id.clone(),
            name,
        })
        .collect()
}

/// `base` followed by ` (id)`, with the base cut so the whole fits in
/// [`MAX_REMOTE_NAME_CHARS`].
fn disambiguated(base: &str, id: &str) -> String {
    let suffix = format!(" ({id})");
    let keep = MAX_REMOTE_NAME_CHARS.saturating_sub(suffix.chars().count());
    let head: String = base.chars().take(keep).collect();
    format!("{}{suffix}", head.trim_end())
}

/// The parent each listed terminal keeps, by position in `infos`. A parent is
/// kept only when the list has it, in the same project, when it is not the
/// terminal itself, and when it has no parent of its own. So a missing parent,
/// a self-parent and a cycle all come out as no parent, and nesting is one
/// level deep, as the local tree draws it.
#[must_use]
pub fn kept_parents(infos: &[TerminalInfo]) -> Vec<Option<u32>> {
    let mut first: HashMap<u32, &TerminalInfo> = HashMap::new();
    for info in infos {
        first.entry(info.id).or_insert(info);
    }
    infos
        .iter()
        .map(|info| {
            let parent = info.parent?;
            let up = first.get(&parent)?;
            (parent != info.id && up.project_id == info.project_id && up.parent.is_none())
                .then_some(parent)
        })
        .collect()
}

/// The owner the host named for each of `infos` ([`TerminalInfo::owner`]),
/// kept only when it is another terminal of the same project in the list.
#[must_use]
pub fn kept_owners(infos: &[TerminalInfo]) -> Vec<Option<u32>> {
    let projects: HashMap<u32, &str> = infos
        .iter()
        .map(|info| (info.id, info.project_id.as_str()))
        .collect();
    infos
        .iter()
        .map(|info| {
            let owner = info.owner?;
            (owner != info.id && projects.get(&owner) == Some(&info.project_id.as_str()))
                .then_some(owner)
        })
        .collect()
}

/// A terminal's background work as the host listed it, cleaned for display,
/// each task's start reckoned from when the list arrived.
fn background_work(
    tasks: &[crate::wire::BackgroundInfo],
    listed_at: std::time::Instant,
) -> Vec<connecto_core::BackgroundWork> {
    tasks
        .iter()
        .map(|task| connecto_core::BackgroundWork {
            id: task.id.clone(),
            kind: display_text(&task.kind),
            label: display_text(&task.label),
            description: task
                .description
                .as_deref()
                .map(display_text)
                .filter(|text| !text.is_empty()),
            since: listed_at
                .checked_sub(std::time::Duration::from_secs(task.seconds))
                .unwrap_or(listed_at),
        })
        .collect()
}

/// The id a terminal is shown by and its name: the host's own id
/// (`claude-12`), as the host shows it, when the list carries it; a terminal
/// the host lists by its id (no name of its own) has no name here either.
fn shown(info: &TerminalInfo) -> (Option<String>, Option<String>) {
    let name = display_text(&info.name);
    let shown_id = info
        .entry
        .as_deref()
        .map(display_text)
        .filter(|id| !id.is_empty());
    let named = shown_id.as_deref() != Some(name.as_str()) && !name.is_empty();
    (shown_id, named.then_some(name))
}

/// One of a project's terminals as the host last listed it, for the
/// project's roster: every terminal the person can see on that host, also
/// those a chat-only client does not attach to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterTerminal {
    /// The host's own id (`claude-12`), else the wire's number.
    pub id: String,
    pub name: Option<String>,
    pub tool: ToolKind,
    /// The host's status dot.
    pub state: TermActivity,
    /// A turn runs now (working with no background work listed).
    pub in_turn: bool,
    /// The id of the terminal it belongs to (the agent that asked for it,
    /// else the one that started it); `None`: the person opened it.
    pub owner: Option<String>,
    /// The project's app session, the chat a call talks to.
    pub app_session: bool,
    pub background: Vec<connecto_core::BackgroundWork>,
}

/// The roster of `project` from a host's whole list, made `listed_at`.
#[must_use]
pub fn roster(
    infos: &[TerminalInfo],
    listed_at: std::time::Instant,
    project: &str,
) -> Vec<RosterTerminal> {
    let parents = kept_parents(infos);
    let owners = kept_owners(infos);
    let id_of = |wire: u32| {
        infos.iter().find(|t| t.id == wire).map_or_else(
            || wire.to_string(),
            |t| shown(t).0.unwrap_or_else(|| wire.to_string()),
        )
    };
    infos
        .iter()
        .zip(parents.into_iter().zip(owners))
        .filter(|(info, _)| info.project_id == project)
        .map(|(info, (parent, owner))| {
            let (shown_id, name) = shown(info);
            let state = activity_from_byte(info.activity);
            RosterTerminal {
                id: shown_id.unwrap_or_else(|| info.id.to_string()),
                app_session: info.name == connecto_core::session::APP_SESSION && parent.is_none(),
                name,
                tool: tool_from_byte(info.tool),
                state,
                in_turn: state == TermActivity::Working && info.background.is_empty(),
                owner: owner.or(parent).map(id_of),
                background: background_work(&info.background, listed_at),
            }
        })
        .collect()
}

/// The entries of one project, or of every project when `project` is `None`.
fn entries(list: &[RemoteTerminal], project: Option<&str>) -> Vec<TerminalEntry> {
    let infos: Vec<TerminalInfo> = list.iter().map(|t| t.info.clone()).collect();
    let parents = kept_parents(&infos);
    let owners = kept_owners(&infos);
    list.iter()
        .zip(parents.into_iter().zip(owners))
        .filter(|(t, _)| match project {
            Some(project) => t.info.project_id == project,
            None => true,
        })
        .map(|(t, (parent, owner))| {
            let host_activity = activity_from_byte(t.info.activity);
            let activity = match t.session.status() {
                connecto_core::SessionStatus::Exited { .. } => TermActivity::Exited,
                // A terminal the host has not started is "not started" as it
                // is there, attached or not; it is not starting.
                connecto_core::SessionStatus::Unknown { .. }
                    if !t.info.running
                        && (t.session.is_linked() || host_activity == TermActivity::NotStarted) =>
                {
                    host_activity
                }
                connecto_core::SessionStatus::Unknown { .. } => TermActivity::Starting,
                connecto_core::SessionStatus::Running => activity_from_byte(t.info.activity),
            };
            let (shown_id, name) = shown(&t.info);
            TerminalEntry {
                id: t.info.id.to_string(),
                name,
                parent: parent.map(|p| p.to_string()),
                // Only when it is not the parent, as on the host.
                asked_by: owner.filter(|o| Some(*o) != parent).map(|o| o.to_string()),
                tool: tool_from_byte(t.info.tool),
                state: activity,
                // Background work is listed only at the prompt, between turns.
                in_turn: activity == TermActivity::Working && t.info.background.is_empty(),
                background: background_work(&t.info.background, t.listed_at),
                finished_turns: 0,
                session: Arc::clone(&t.session) as Arc<dyn TerminalSession>,
                control: None,
                desired_running: t.info.running,
                shown_id,
                activity: 0,
            }
        })
        .collect()
}

impl TerminalsControl for RemoteTerminals {
    fn view(&self) -> Arc<TerminalsView> {
        let terminals = entries(&self.host.terminals(), self.project.as_deref());
        let (history, transcript) = self.project.as_deref().map_or_else(
            || (Arc::default(), None),
            |project| {
                let history = match self.host.peek_panel(project, PanelKind::History).0 {
                    Some(PanelValue::History(history)) => history,
                    _ => Arc::default(),
                };
                (history, self.host.transcript(project))
            },
        );
        Arc::new(TerminalsView {
            terminals,
            history,
            transcript,
            // The host's terminals can be added to and closed while connected.
            save: if self.host.is_connected() {
                SaveState::Saved
            } else {
                SaveState::ReadOnly
            },
            last_error: self.host.last_error(),
            warning: None,
        })
    }

    fn add(&self, tool: connecto_core::ToolKind, name: Option<String>) -> Result<(), String> {
        self.add_on_model(tool, name, None)
    }

    fn fetch_file(&self, id: &str, path: &str) -> Result<(String, Vec<u8>), String> {
        self.host.read_file(parse_id(id)?, path)
    }

    fn upload_file(&self, id: &str, name: &str, bytes: &[u8]) -> Result<String, String> {
        self.host.upload_file(parse_id(id)?, name, bytes)
    }

    /// Not OpenCode on a host from before it (see [`crate::wire::FEATURE_OPENCODE`]),
    /// nor an agent the project's Tools setting on the host leaves out. The
    /// host's settings are read from its settings view, fetched in the
    /// background: until it is here, or from a host that has no such view,
    /// every agent is offered and the host refuses what it does not take. A
    /// host from before sets of tools says one tool or all, which is what it
    /// allows.
    /// Until the host's settings view is here, or the host has said why it
    /// cannot give it (one from before it, say: then every agent is offered).
    fn agents_loading(&self) -> bool {
        let Some(project) = self.project.as_deref() else {
            return false;
        };
        if !self.host.is_connected() {
            return false;
        }
        let (value, error) = self.host.panel(project, PanelKind::Settings, SETTINGS_FRESH);
        value.is_none() && error.is_none()
    }

    fn offers(&self, tool: connecto_core::ToolKind) -> bool {
        if tool == connecto_core::ToolKind::OpenCode && !self.host.knows(tool) {
            return false;
        }
        let Some(project) = self.project.as_deref() else {
            return true;
        };
        if !tool.is_agent() {
            return true;
        }
        match self.host.panel(project, PanelKind::Settings, SETTINGS_FRESH).0 {
            Some(PanelValue::Settings(answer)) => answer
                .project
                .as_ref()
                .is_none_or(|p| p.settings.tools.allows(tool)),
            _ => true,
        }
    }

    /// The host's own models: its providers decide, not this machine's.
    fn remote_models(&self, tool: connecto_core::ToolKind) -> Option<connecto_core::RemoteModels> {
        let offered = self
            .project
            .as_deref()
            .map(|project| self.host.offered_models(tool, project))
            .unwrap_or_default();
        Some(connecto_core::RemoteModels {
            default: offered.default,
            models: offered.models,
        })
    }

    /// The model goes to a host that reads it; one from before models starts
    /// the tool on its default (see `HostHandle::manage`).
    fn add_on_model(
        &self,
        tool: connecto_core::ToolKind,
        name: Option<String>,
        model: Option<String>,
    ) -> Result<(), String> {
        self.send(ManageOp::Add {
            tool: tool_byte(tool),
            name,
            project: self.project.clone(),
            model,
        })
    }

    fn close(&self, id: &str) -> Result<(), String> {
        self.send(ManageOp::Close {
            terminal: parse_id(id)?,
        })
    }

    fn rename(&self, id: &str, name: Option<String>) -> Result<(), String> {
        self.send(ManageOp::Rename {
            terminal: parse_id(id)?,
            name,
        })
    }

    fn restart(&self, id: &str) -> Result<(), String> {
        self.send(ManageOp::Restart {
            terminal: parse_id(id)?,
        })
    }

    fn stop_background(&self, id: &str, task: Option<&str>) -> Result<String, String> {
        if !self.host.has_background() {
            return Err(
                "Update Connecto on that machine to stop background work from here.".to_owned(),
            );
        }
        self.send(ManageOp::StopBackground {
            terminal: parse_id(id)?,
            task: task.map(str::to_owned),
        })?;
        Ok(match task {
            Some(_) => "Stopping the background shell.".to_owned(),
            None => "Stopping the background shells.".to_owned(),
        })
    }

    /// Opens the closed terminal again on the host, which resumes its
    /// session there; it shows here once the host lists it.
    fn continue_from(&self, history_id: &str) -> Result<(), String> {
        let project = self.project_or_refuse()?;
        self.host.project_command(
            project,
            ProjectAsk::Continue {
                history: history_id.to_owned(),
            },
        );
        Ok(())
    }

    fn request_transcript(&self, history_id: &str) -> Result<(), String> {
        let project = self.project_or_refuse()?;
        self.host.request_transcript(project, history_id);
        Ok(())
    }

    fn watch_history(&self) {
        if let Some(project) = &self.project {
            let _ = self.host.panel(project, PanelKind::History, HISTORY_FRESH);
        }
    }

    fn history_error(&self) -> Option<String> {
        let project = self.project.as_deref()?;
        self.host
            .transcript_error(project)
            .or_else(|| self.host.peek_panel(project, PanelKind::History).1)
    }

    fn remote_packages(&self, shown_as: &str) -> Option<Arc<dyn connecto_core::PackagesControl>> {
        let project = self.project.clone()?;
        Some(Arc::new(crate::remote_settings::RemotePackages::new(
            Arc::clone(&self.host),
            project,
            shown_as.to_owned(),
        )))
    }

    fn remote_tokens(&self, shown_as: &str) -> Option<Arc<dyn connecto_core::TokenControl>> {
        let project = self.project.clone()?;
        Some(Arc::new(crate::remote_settings::RemoteTokens::new(
            Arc::clone(&self.host),
            project,
            shown_as.to_owned(),
        )))
    }

    fn remote_ops(&self) -> Option<Arc<dyn connecto_core::ops::OpsControl>> {
        let project = self.project.clone()?;
        Some(Arc::new(crate::remote_settings::RemoteOps::new(
            Arc::clone(&self.host),
            project,
        )))
    }

    fn remote_functional_memory(
        &self,
    ) -> Option<Arc<dyn connecto_core::functional_memory::FunctionalMemoryControl>> {
        let project = self.project.clone()?;
        Some(Arc::new(
            crate::remote_settings::RemoteFunctionalMemory::new(Arc::clone(&self.host), project),
        ))
    }
}

/// How long a host project's history is shown before it is asked for again,
/// while the history sheet is open.
const HISTORY_FRESH: std::time::Duration = std::time::Duration::from_secs(3);
/// How old the host's settings may be for the agents offered.
const SETTINGS_FRESH: std::time::Duration = std::time::Duration::from_secs(10);

impl RemoteTerminals {
    /// The host project this list is, or why there is none: a host that lists
    /// no projects (one from before projects) keeps no history to read.
    fn project_or_refuse(&self) -> Result<&str, String> {
        if !self.host.is_connected() {
            return Err("not connected to that host".to_owned());
        }
        self.project.as_deref().ok_or_else(|| NOT_YET.to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use connecto_core::GridSize;

    use crate::client::RemoteSession;
    use crate::wire::TerminalAddr;

    fn info(id: u32, project: &str, parent: Option<u32>) -> TerminalInfo {
        TerminalInfo {
            entry: None,
            background: Vec::new(),
            id,
            name: format!("t{id}"),
            tool: 1,
            running: true,
            activity: 0,
            cols: 80,
            rows: 24,
            project_id: project.to_owned(),
            project_name: format!("name of {project}"),
            parent,
            owner: None,
        }
    }

    fn listed(infos: Vec<TerminalInfo>) -> Vec<RemoteTerminal> {
        infos
            .into_iter()
            .map(|info| RemoteTerminal {
                listed_at: std::time::Instant::now(),
                session: RemoteSession::new(
                    TerminalAddr {
                        host: "h".into(),
                        terminal: info.id,
                    },
                    GridSize { cols: 80, rows: 24 },
                    Arc::new(|| {}),
                ),
                info,
            })
            .collect()
    }

    #[test]
    fn a_missing_self_or_cyclic_parent_degrades_to_none() {
        let infos = vec![
            info(1, "p", Some(9)), // missing
            info(2, "p", Some(2)), // self
            info(3, "p", Some(4)), // cycle 3 <-> 4
            info(4, "p", Some(3)), // cycle 3 <-> 4
            info(5, "p", Some(6)), // a chain: 6 has a parent, so 5 keeps none
            info(6, "p", Some(7)), // 7 has none, so 6 keeps it
            info(7, "p", None),
            info(8, "p", Some(7)), // a real child
        ];
        assert_eq!(
            kept_parents(&infos),
            vec![None, None, None, None, None, Some(7), None, Some(7)]
        );
    }

    /// The host's tasks reach the sidebar cleaned, aged from when the list
    /// arrived, and with no empty description.
    #[test]
    fn background_work_is_cleaned_and_aged_from_the_list() {
        let at = std::time::Instant::now() + std::time::Duration::from_secs(100);
        let work = background_work(
            &[
                crate::wire::BackgroundInfo {
                    id: "b1".into(),
                    kind: "shell".into(),
                    label: "npm\u{202e} run dev".into(),
                    description: Some("Start the dev server".into()),
                    seconds: 60,
                },
                crate::wire::BackgroundInfo {
                    id: "a2".into(),
                    kind: "subagent".into(),
                    label: "Review".into(),
                    description: Some("\u{200f}".into()),
                    seconds: 0,
                },
            ],
            at,
        );
        assert_eq!(work[0].label, "npm  run dev");
        assert_eq!(work[0].description.as_deref(), Some("Start the dev server"));
        assert_eq!(work[0].age(at), std::time::Duration::from_secs(60));
        assert!(work[0].is_shell());
        assert_eq!(work[1].description, None);
    }

    #[test]
    fn every_bidi_control_is_cleaned_from_names() {
        for c in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{2028}', '\u{2029}',
        ] {
            let raw = format!("a{c}b");
            assert_eq!(display_text(&raw), "a b", "U+{:04X}", c as u32);
        }
        assert_eq!(
            display_text("plain \u{e9}t\u{e9} \u{1f600}"),
            "plain \u{e9}t\u{e9} \u{1f600}"
        );
    }

    fn named(id: u32, project: &str, name: &str) -> TerminalInfo {
        let mut t = info(id, project, None);
        t.project_name = name.into();
        t
    }

    /// A listing with the given projects (id, name) and terminals.
    fn listing(projects: &[(&str, &str)], terminals: Vec<TerminalInfo>) -> Listing {
        Listing {
            projects: projects
                .iter()
                .map(|(id, name)| ProjectInfo {
                    id: (*id).to_owned(),
                    name: (*name).to_owned(),
                })
                .collect(),
            terminals,
        }
    }

    #[test]
    fn a_plain_listing_passes_the_check_and_an_empty_project_is_allowed() {
        let good = listing(
            &[
                ("0123abcd", "Alpha"),
                ("fedc-9_1", "Beta"),
                ("empty", "Gamma"),
            ],
            vec![
                named(1, "0123abcd", "Alpha"),
                named(2, "0123abcd", "Alpha"),
                named(3, "fedc-9_1", "Beta"),
            ],
        );
        assert_eq!(check_listing(&good), Ok(()));
        assert_eq!(check_listing(&Listing::default()), Ok(()));
    }

    #[test]
    fn duplicate_terminal_ids_refuse_the_whole_listing() {
        // Conflicting parents and projects under one id.
        let mut a = info(1, "a", None);
        let mut b = info(2, "a", None);
        let mut c = info(1, "b", Some(2));
        let d = info(2, "a", Some(1));
        a.name = "one".into();
        b.name = "two".into();
        c.name = "one again".into();
        let projects = [("a", "A"), ("b", "B")];
        assert!(
            check_listing(&listing(
                &projects,
                vec![a.clone(), b.clone(), c.clone(), d]
            ))
            .is_err()
        );
        assert!(check_listing(&listing(&projects, vec![a, c])).is_err());
    }

    #[test]
    fn the_same_project_id_with_two_names_refuses_the_whole_listing() {
        // Listed twice with two names.
        let twice = listing(&[("same", "Trusted"), ("same", "Other")], Vec::new());
        assert!(check_listing(&twice).unwrap_err().contains("listed twice"));
        // Listed once, but a terminal names it differently.
        let skew = listing(&[("same", "Trusted")], vec![named(1, "same", "Other")]);
        assert!(check_listing(&skew).unwrap_err().contains("two names"));
    }

    #[test]
    fn a_terminal_in_a_project_the_listing_does_not_give_refuses_it() {
        let stray = listing(&[("a", "A")], vec![named(1, "b", "A")]);
        assert!(check_listing(&stray).is_err());
    }

    #[test]
    fn non_canonical_project_ids_refuse_the_whole_listing() {
        let long = "x".repeat(MAX_PROJECT_ID_LEN + 1);
        for bad in [
            "",
            "Same",
            "SAME",
            "same ",
            "sa\u{200b}me",
            "same\u{202e}",
            "same\nx",
            "s/me",
            "\u{0}",
            long.as_str(),
        ] {
            let infos = listing(&[(bad, "Name")], Vec::new());
            assert!(check_listing(&infos).is_err(), "{bad:?} must be refused");
        }
        assert!(is_canonical_project_id(&"x".repeat(MAX_PROJECT_ID_LEN)));
    }

    #[test]
    fn aliased_projects_cannot_pass_as_two_or_merge() {
        // Case variants are not one spelling, so one of them is refused.
        let alias = listing(&[("same", "A"), ("Same", "B")], Vec::new());
        assert!(check_listing(&alias).is_err());
    }

    #[test]
    fn a_project_name_that_cleans_to_nothing_refuses_the_listing() {
        let blank = listing(&[("p", " \u{2028}\t ")], Vec::new());
        assert!(check_listing(&blank).is_err());
    }

    #[test]
    fn projects_with_the_same_name_are_accepted_and_told_apart_by_id() {
        let both = listing(
            &[("aaaa1111", "backend"), ("bbbb2222", "backend")],
            vec![
                named(1, "aaaa1111", "backend"),
                named(2, "bbbb2222", "backend"),
            ],
        );
        assert_eq!(check_listing(&both), Ok(()));
        let names: Vec<String> = remote_projects(&both.projects)
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["backend (aaaa1111)", "backend (bbbb2222)"]);
    }

    fn labels(projects: &[(&str, &str)]) -> Vec<String> {
        let infos: Vec<ProjectInfo> = projects
            .iter()
            .map(|(id, name)| ProjectInfo {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
            })
            .collect();
        let got: Vec<String> = remote_projects(&infos)
            .into_iter()
            .map(|p| p.name)
            .collect();
        let unique: HashSet<&String> = got.iter().collect();
        assert_eq!(unique.len(), got.len(), "labels must be unique: {got:?}");
        got
    }

    #[test]
    fn a_name_cannot_forge_another_projects_generated_label() {
        // The reviewer's case: "x (abcd)" is a name a peer can choose.
        let got = labels(&[("abcd", "x"), ("efgh", "x"), ("evil", "x (abcd)")]);
        assert_eq!(got[0], "x (abcd)");
        assert_eq!(got[1], "x (efgh)");
        assert_eq!(got[2], "x (abcd) (evil)");
    }

    #[test]
    fn a_three_way_collision_is_suffixed_with_each_id() {
        let got = labels(&[("aaaa", "x"), ("bbbb", "x"), ("cccc", "x")]);
        assert_eq!(got, ["x (aaaa)", "x (bbbb)", "x (cccc)"]);
    }

    #[test]
    fn unique_names_keep_no_suffix() {
        let got = labels(&[("aaaa", "x"), ("bbbb", "y (cccc)")]);
        assert_eq!(got, ["x", "y (cccc)"]);
    }

    #[test]
    fn a_suffixed_label_is_read_back_from_its_end_and_never_equals_another() {
        // Names that already contain brackets and ids.
        let got = labels(&[
            ("aaaa", "p (bbbb)"),
            ("bbbb", "p"),
            ("cccc", "p (bbbb) (aaaa)"),
        ]);
        let unique: HashSet<&String> = got.iter().collect();
        assert_eq!(unique.len(), 3, "{got:?}");
    }

    #[test]
    fn names_past_the_bound_that_match_are_kept_within_the_bound() {
        let base = "x".repeat(MAX_REMOTE_NAME_CHARS + 10);
        let first = format!("{base}a");
        let second = format!("{base}b");
        let infos = [("p1", first.as_str()), ("p2", second.as_str())];
        let got = labels(&infos);
        for (label, (id, _)) in got.iter().zip(infos) {
            assert!(label.chars().count() <= MAX_REMOTE_NAME_CHARS, "{label}");
            assert!(label.ends_with(&format!("({id})")), "{label}");
        }
    }

    #[test]
    fn remote_projects_keep_the_host_order_and_include_empty_projects() {
        let projects = vec![
            ProjectInfo {
                id: "p".into(),
                name: "one\u{2028}name\u{202e}".into(),
            },
            ProjectInfo {
                id: "q".into(),
                name: "empty".into(),
            },
        ];
        assert_eq!(
            remote_projects(&projects),
            vec![
                RemoteProject {
                    id: "p".into(),
                    name: "one name".into()
                },
                RemoteProject {
                    id: "q".into(),
                    name: "empty".into()
                },
            ]
        );
    }

    #[test]
    fn a_parent_in_another_project_is_not_kept() {
        let infos = vec![info(1, "a", None), info(2, "b", Some(1))];
        assert_eq!(kept_parents(&infos), vec![None, None]);
    }

    #[test]
    fn display_text_is_bounded_and_has_no_line_breaks() {
        let long = "x".repeat(10_000);
        assert_eq!(display_text(&long).chars().count(), MAX_REMOTE_NAME_CHARS);
        let cleaned = display_text("  a\r\nb\u{85}c\u{2029}d\u{7}  ");
        assert_eq!(cleaned, "a  b c d");
        assert!(!cleaned.chars().any(char::is_control));
        assert_eq!(display_text("\n\t "), "");
    }

    #[test]
    fn a_project_view_holds_its_own_terminals_with_their_parents() {
        let list = listed(vec![
            info(1, "a", None),
            info(2, "b", None),
            info(3, "a", Some(1)),
            info(4, "b", Some(1)),
        ]);
        let a = entries(&list, Some("a"));
        let ids: Vec<(&str, Option<&str>)> = a
            .iter()
            .map(|e| (e.id.as_str(), e.parent.as_deref()))
            .collect();
        assert_eq!(ids, vec![("1", None), ("3", Some("1"))]);
        let all = entries(&list, None);
        assert_eq!(all.len(), 4);
        assert_eq!(all[3].parent, None, "its parent is in another project");
    }

    #[test]
    fn a_cut_or_missing_parent_leaves_the_child_listed() {
        let list = listed(vec![info(3, "a", Some(1))]);
        let view = entries(&list, Some("a"));
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].parent, None);
    }

    /// A host terminal shows its host id (`claude-12`), not the wire's
    /// number, and one the host has not started reads "not started" before
    /// any link, as on the host, rather than "starting".
    #[test]
    fn a_terminal_shows_its_host_id_and_a_stopped_one_is_not_starting() {
        let list = listed(vec![
            TerminalInfo {
                entry: Some("claude-12".into()),
                name: "app-session".into(),
                activity: crate::hosting::activity_byte(TermActivity::Idle),
                ..info(17, "a", None)
            },
            TerminalInfo {
                entry: Some("claude".into()),
                name: "claude".into(),
                running: false,
                activity: crate::hosting::activity_byte(TermActivity::NotStarted),
                ..info(19, "a", None)
            },
            // A host from before the ids: the number, as before.
            info(20, "a", None),
        ]);
        let view = entries(&list, Some("a"));
        let shown: Vec<(&str, Option<&str>)> = view
            .iter()
            .map(|e| (e.display_id(), e.name.as_deref()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("claude-12", Some("app-session")),
                // Listed by its id on the host, so unnamed here as there.
                ("claude", None),
                ("20", Some("t20")),
            ]
        );
        assert_eq!(view[0].id, "17", "operations still name the wire id");
        assert_eq!(view[1].state, TermActivity::NotStarted);
    }
}
