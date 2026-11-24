//! `connecto agents [--roles]`: which coding CLIs a project can start now,
//! and which one, on which model, takes each role of a team (ADR 0045).
//!
//! The host says, per CLI, whether the project's Tools allow it and whether
//! it is ready (installed, offered, signed in, Pi with a provider that
//! works). [`picks`] then chooses the coder, the reviewer and the second
//! opinion from the ready ones, and [`render`] writes what the command
//! prints. The models and the order of preference are here and only here,
//! so a model change is one edit and every team skill follows it.

use std::path::Path;

use crate::ToolKind;

/// Whether a CLI can be started in the project now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    /// Installed, offered and, as far as Connecto can check it, signed in.
    Ready,
    /// Installed and offered, but whether it can work could not be checked
    /// without starting it (Antigravity has no sign-in status; Pi's
    /// providers for a project are read when its first Pi terminal starts):
    /// why. Chosen only after every ready CLI.
    Unverified(String),
    /// It cannot: why, in plain words.
    Unusable(String),
    /// Not known yet: what is still being found out.
    Checking(String),
}

/// One coding CLI as the roster sees it for a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterAgent {
    pub tool: ToolKind,
    /// The project's Tools setting allows it.
    pub allowed: bool,
    /// Ready, or why not. A CLI the project leaves out is never ready.
    pub state: AgentState,
    /// What its `--version` printed, when known.
    pub version: Option<String>,
    /// The provider it reaches its model through when that replaces the
    /// role's model, such as "Together AI" for Claude through TogetherLink,
    /// which refuses `--model`: it then runs that provider's own model.
    pub via: Option<String>,
}

impl RosterAgent {
    /// How good a candidate it is: 0 ready on the role's model, 1 ready on
    /// a provider's own model, 2 unverified; `None` when it cannot start.
    fn tier(&self) -> Option<u8> {
        if !self.allowed {
            return None;
        }
        match self.state {
            AgentState::Ready if self.via.is_none() => Some(0),
            AgentState::Ready => Some(1),
            AgentState::Unverified(_) => Some(2),
            AgentState::Unusable(_) | AgentState::Checking(_) => None,
        }
    }
}

/// What the bridge asks the app: every coding CLI, in
/// [`crate::packages::ProjectTools::CHOICES`] order, for one project.
pub trait RosterBridge: Send + Sync {
    /// The CLIs for the project `project_id` in `folder`. May be slow (it
    /// may run each CLI's sign-in check), so it is called off the UI thread
    /// and returns within the bridge's wait, saying `Checking` for a CLI
    /// whose answer is not in yet. It reads only: it never changes a
    /// setting nor starts anything that could.
    fn agents(&self, project_id: &str, folder: &Path) -> Vec<RosterAgent>;
}

/// A role a team skill starts a terminal for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Writes the code.
    Coder,
    /// Checks what the coder wrote, on another CLI when there is one.
    Reviewer,
    /// Answers one question or checks one change for the agent that asks,
    /// on another CLI than the asker's when there is one.
    SecondOpinion,
}

impl Role {
    pub const ALL: [Self; 3] = [Self::Coder, Self::Reviewer, Self::SecondOpinion];

    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Coder => "coder",
            Self::Reviewer => "reviewer",
            Self::SecondOpinion => "second-opinion",
        }
    }

    /// The CLIs this role prefers, best first.
    const fn order(self) -> [ToolKind; 5] {
        use ToolKind::{Antigravity, Claude, Codex, OpenCode, Pi};
        match self {
            Self::Coder => [Claude, Codex, Antigravity, Pi, OpenCode],
            Self::Reviewer => [Codex, Antigravity, Claude, Pi, OpenCode],
            Self::SecondOpinion => [Claude, Codex, Antigravity, Pi, OpenCode],
        }
    }
}

/// The efforts a coordinator chooses from, by how hard the task looks.
pub const EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// What [`Pick::command`] puts where the coordinator's effort goes.
pub const EFFORT_SLOT: &str = "<effort>";

/// How a CLI is started in every role: its model, and how it takes the
/// effort the coordinator chose. Claude runs Opus and Codex Astra, each with
/// `--effort`; Antigravity runs the Gemini Neon model of that effort
/// (`gemini-4-neon-<effort>`: the effort is part of the model's name); Pi
/// runs the model the user configured, with `--effort`; OpenCode runs its
/// configured model and takes no effort. With `via` (a provider whose own
/// model replaces the role's, such as TogetherLink for Claude) no model is
/// given, since the launch would drop it.
#[must_use]
pub fn start_args(tool: ToolKind, effort: &str, via: bool) -> Vec<String> {
    let flag = |model: &str| {
        let mut args = Vec::new();
        if !via {
            args.extend(["--model".to_owned(), model.to_owned()]);
        }
        args.extend(["--effort".to_owned(), effort.to_owned()]);
        args
    };
    match tool {
        ToolKind::Claude => flag("claude-opus-5-5"),
        ToolKind::Codex => flag("gpt-6.2-astra"),
        ToolKind::Antigravity => vec!["--model".to_owned(), format!("gemini-4-neon-{effort}")],
        ToolKind::Pi => vec!["--effort".to_owned(), effort.to_owned()],
        ToolKind::OpenCode | ToolKind::Shell => Vec::new(),
    }
}

/// The CLI a role runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub role: Role,
    pub tool: ToolKind,
    /// No other CLI was ready, so it runs on the same CLI as the one it
    /// should be independent of (the coder, or the asker): a fresh session,
    /// but the same CLI, and it must be said so.
    pub same_cli: bool,
    /// The provider whose model it runs instead of the role's model.
    pub via: Option<String>,
    /// Its sign-in could not be checked before it starts.
    pub unverified: bool,
}

impl Pick {
    fn of(role: Role, agent: &RosterAgent, same_cli: bool) -> Self {
        Self {
            role,
            tool: agent.tool,
            same_cli,
            via: agent.via.clone(),
            unverified: matches!(agent.state, AgentState::Unverified(_)),
        }
    }

    /// The exact command that starts it at `effort` (one of [`EFFORTS`], or
    /// [`EFFORT_SLOT`] for the coordinator to fill in).
    #[must_use]
    pub fn command(&self, effort: &str) -> String {
        let mut out = format!("connecto add {}", self.tool.name());
        for arg in start_args(self.tool, effort, self.via.is_some()) {
            out.push(' ');
            out.push_str(&arg);
        }
        out
    }

    /// The agent as the role line shows it: its CLI, the provider it runs
    /// through, and the marks a skill must pass on.
    fn shown(&self) -> String {
        let mut out = self.tool.name().to_owned();
        if let Some(via) = &self.via {
            out.push_str(&format!(" via {via}"));
        }
        if self.same_cli {
            out.push_str(" (same CLI)");
        }
        if self.unverified {
            out.push_str(" (unverified)");
        }
        out
    }
}

/// Each role's CLI: the best candidate that is not `other` (the coder's CLI
/// for the reviewer, the asker's for the second opinion), ready on the
/// role's model before ready on a provider's own model before unverified,
/// then by the role's order; else `other` itself, marked as the same CLI.
/// `None` when no CLI can start.
#[must_use]
pub fn picks(agents: &[RosterAgent], asker: Option<ToolKind>) -> Vec<(Role, Option<Pick>)> {
    let agent = |tool: ToolKind| agents.iter().find(|a| a.tool == tool);
    let choose = |role: Role, other: Option<ToolKind>| -> Option<Pick> {
        role.order()
            .into_iter()
            .enumerate()
            .filter(|&(_, tool)| Some(tool) != other)
            .filter_map(|(rank, tool)| {
                let found = agent(tool)?;
                Some(((found.tier()?, rank), found))
            })
            .min_by_key(|(key, _)| *key)
            .map(|(_, found)| Pick::of(role, found, false))
            .or_else(|| {
                other
                    .and_then(agent)
                    .filter(|found| found.tier().is_some())
                    .map(|found| Pick::of(role, found, true))
            })
    };
    let coder = choose(Role::Coder, None);
    let reviewer = choose(Role::Reviewer, coder.as_ref().map(|p| p.tool));
    let second = choose(Role::SecondOpinion, asker);
    vec![
        (Role::Coder, coder),
        (Role::Reviewer, reviewer),
        (Role::SecondOpinion, second),
    ]
}

fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// What `connecto agents` prints; with `roles`, also each role's CLI and
/// the command that starts it.
#[must_use]
pub fn render(agents: &[RosterAgent], asker: Option<ToolKind>, roles: bool) -> String {
    let mut rows = vec![vec![
        "AGENT".to_owned(),
        "STATUS".to_owned(),
        "VERSION".to_owned(),
        "START WITH, OR WHY NOT".to_owned(),
    ]];
    for agent in agents {
        let start = || Pick::of(Role::Coder, agent, false).command(EFFORT_SLOT);
        let (status, note) = if !agent.allowed {
            (
                "not allowed",
                "left out by the project's Tools setting".to_owned(),
            )
        } else {
            match &agent.state {
                AgentState::Ready => ("ready", start()),
                AgentState::Unverified(why) => ("unverified", format!("{}  ({why})", start())),
                AgentState::Unusable(why) => ("unusable", why.clone()),
                AgentState::Checking(what) => ("checking", what.clone()),
            }
        };
        let mut name = agent.tool.name().to_owned();
        if let Some(via) = &agent.via {
            name.push_str(&format!(" via {via}"));
        }
        rows.push(vec![
            name,
            status.to_owned(),
            agent.version.clone().unwrap_or_else(|| "-".to_owned()),
            note,
        ]);
    }
    let mut out = table(&rows);
    if let Some(via) = agents
        .iter()
        .find_map(|a| a.via.as_ref().map(|v| (a.tool, v)))
    {
        out.push_str(&format!(
            "{} runs through {}: on that provider's own model, not the role's, so name it that way.\n",
            via.0.name(),
            via.1
        ));
    }
    if !roles {
        return out;
    }
    out.push('\n');
    let mut rows = vec![vec![
        "ROLE".to_owned(),
        "AGENT".to_owned(),
        "START WITH".to_owned(),
    ]];
    let mut notes = Vec::new();
    for (role, pick) in picks(agents, asker) {
        match pick {
            Some(pick) => {
                if pick.same_cli {
                    let of = if role == Role::Reviewer {
                        "the coder"
                    } else {
                        "you"
                    };
                    notes.push(format!(
                        "{}: no other coding CLI is ready, so it runs on the same CLI as {of}, in a fresh session. Say so in its result: \"same CLI ({})\".",
                        role.word(),
                        pick.tool.name()
                    ));
                }
                if pick.unverified {
                    notes.push(format!(
                        "{}: {}'s sign-in could not be checked before it starts. If its terminal stops at a sign-in screen, close it and use another ready CLI.",
                        role.word(),
                        pick.tool.name()
                    ));
                }
                rows.push(vec![
                    role.word().to_owned(),
                    pick.shown(),
                    pick.command(EFFORT_SLOT),
                ]);
            }
            None => rows.push(vec![
                role.word().to_owned(),
                "none".to_owned(),
                "-".to_owned(),
            ]),
        }
    }
    out.push_str(&table(&rows));
    out.push_str("Replace <effort> with low, medium, high or xhigh, by how hard the task looks.\n");
    for note in notes {
        out.push_str(&note);
        out.push('\n');
    }
    if agents
        .iter()
        .any(|a| a.allowed && matches!(a.state, AgentState::Checking(_)))
    {
        out.push_str(
            "Some agents are still being checked: run `connecto agents --roles` again in a moment for the full choice.\n",
        );
    }
    if agents.iter().all(|a| a.tier().is_none()) {
        out.push_str(
            "No coding CLI is ready in this project: tell the user which ones are unusable and why.\n",
        );
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ToolKind::{Antigravity, Claude, Codex, OpenCode, Pi};

    fn roster(ready: &[ToolKind]) -> Vec<RosterAgent> {
        [Claude, Codex, Pi, Antigravity, OpenCode]
            .into_iter()
            .map(|tool| RosterAgent {
                tool,
                allowed: true,
                state: if ready.contains(&tool) {
                    AgentState::Ready
                } else {
                    AgentState::Unusable("not signed in".into())
                },
                version: None,
                via: None,
            })
            .collect()
    }

    fn set(agents: &mut [RosterAgent], tool: ToolKind, state: AgentState) {
        agents.iter_mut().find(|a| a.tool == tool).unwrap().state = state;
    }

    fn chosen(agents: &[RosterAgent], asker: Option<ToolKind>) -> Vec<Option<(ToolKind, bool)>> {
        picks(agents, asker)
            .into_iter()
            .map(|(_, p)| p.map(|p| (p.tool, p.same_cli)))
            .collect()
    }

    #[test]
    fn every_cli_ready_gives_claude_codex_and_another_cli_for_the_asker() {
        let all = roster(&[Claude, Codex, Pi, Antigravity, OpenCode]);
        assert_eq!(
            chosen(&all, Some(Claude)),
            [
                Some((Claude, false)),
                Some((Codex, false)),
                Some((Codex, false))
            ]
        );
        assert_eq!(chosen(&all, Some(Codex))[2], Some((Claude, false)));
    }

    #[test]
    fn the_reviewer_avoids_the_coders_cli_while_another_is_ready() {
        assert_eq!(
            chosen(&roster(&[Claude, Antigravity]), Some(Claude)),
            [
                Some((Claude, false)),
                Some((Antigravity, false)),
                Some((Antigravity, false))
            ]
        );
        assert_eq!(
            chosen(&roster(&[Pi, OpenCode]), Some(Pi)),
            [
                Some((Pi, false)),
                Some((OpenCode, false)),
                Some((OpenCode, false))
            ]
        );
    }

    #[test]
    fn one_cli_alone_takes_every_role_marked_as_the_same_cli() {
        assert_eq!(
            chosen(&roster(&[Codex]), Some(Codex)),
            [
                Some((Codex, false)),
                Some((Codex, true)),
                Some((Codex, true))
            ]
        );
        let text = render(&roster(&[Codex]), Some(Codex), true);
        assert!(text.contains("codex (same CLI)"), "{text}");
        assert!(text.contains("same CLI (codex)"), "{text}");
    }

    #[test]
    fn a_cli_the_project_leaves_out_or_not_signed_in_is_never_chosen() {
        let mut agents = roster(&[Claude, Codex]);
        agents[1].allowed = false;
        assert_eq!(
            chosen(&agents, Some(Claude)),
            [
                Some((Claude, false)),
                Some((Claude, true)),
                Some((Claude, true))
            ]
        );
        let text = render(&agents, Some(Claude), false);
        assert!(text.contains("not allowed"), "{text}");
        assert!(text.contains("not signed in"), "{text}");
        assert!(!text.contains("ROLE"), "{text}");
        // A ready CLI says how to start it, for a role moved off another.
        assert!(
            text.contains("connecto add claude --model claude-opus-5-5 --effort <effort>"),
            "{text}"
        );
        assert!(!text.contains("connecto add codex"), "{text}");
    }

    #[test]
    fn nothing_ready_picks_no_one_and_says_so() {
        let text = render(&roster(&[]), None, true);
        assert_eq!(chosen(&roster(&[]), None), [None, None, None]);
        assert!(text.contains("No coding CLI is ready"), "{text}");
    }

    /// An Antigravity whose sign-in cannot be checked loses the reviewer
    /// slot to a Pi that is ready; alone with the coder it still takes it,
    /// marked, before the coder's own CLI would.
    #[test]
    fn an_unverified_cli_comes_after_every_ready_one() {
        let mut agents = roster(&[Claude, Pi]);
        set(
            &mut agents,
            Antigravity,
            AgentState::Unverified("no sign-in status".into()),
        );
        assert_eq!(chosen(&agents, Some(Claude))[1], Some((Pi, false)));
        let text = render(&agents, Some(Claude), true);
        assert!(text.contains("unverified"), "{text}");
        set(&mut agents, Pi, AgentState::Unusable("no provider".into()));
        let picks = picks(&agents, Some(Claude));
        let reviewer = picks[1].1.clone().unwrap();
        assert_eq!((reviewer.tool, reviewer.same_cli), (Antigravity, false));
        assert!(reviewer.unverified);
        let text = render(&agents, Some(Claude), true);
        assert!(text.contains("antigravity (unverified)"), "{text}");
        assert!(text.contains("could not be checked"), "{text}");
    }

    /// Claude through TogetherLink runs Together's model, not Opus: it is
    /// started without `--model`, shown with its provider, and a CLI on the
    /// role's own model codes first.
    #[test]
    fn a_cli_on_a_providers_own_model_says_so_and_comes_after_one_on_the_roles_model() {
        let mut agents = roster(&[Claude, Codex]);
        agents[0].via = Some("Together AI".into());
        let picks = picks(&agents, Some(Codex));
        let coder = picks[0].1.clone().unwrap();
        assert_eq!(coder.tool, Codex);
        let second = picks[2].1.clone().unwrap();
        assert_eq!(second.tool, Claude);
        assert_eq!(second.command("high"), "connecto add claude --effort high");
        let text = render(&agents, Some(Codex), true);
        assert!(text.contains("claude via Together AI"), "{text}");
        assert!(!text.contains("claude-opus-5-5"), "{text}");
        // Alone, it still codes.
        agents[1].state = AgentState::Unusable("not signed in".into());
        assert_eq!(chosen(&agents, None)[0], Some((Claude, false)));
    }

    #[test]
    fn each_pick_starts_with_its_cli_model_and_effort() {
        let picks = picks(&roster(&[Claude, Codex, Antigravity, Pi]), Some(Pi));
        let commands: Vec<String> = picks
            .iter()
            .map(|(_, p)| p.as_ref().unwrap().command("high"))
            .collect();
        assert_eq!(
            commands,
            [
                "connecto add claude --model claude-opus-5-5 --effort high",
                "connecto add codex --model gpt-6.2-astra --effort high",
                "connecto add claude --model claude-opus-5-5 --effort high",
            ]
        );
        assert_eq!(
            start_args(Antigravity, "xhigh", false),
            ["--model", "gemini-4-neon-xhigh"]
        );
        assert_eq!(start_args(Pi, "low", false), ["--effort", "low"]);
        assert!(start_args(OpenCode, "low", false).is_empty());
        let text = render(&roster(&[Claude, Codex]), Some(Claude), true);
        assert!(
            text.contains("connecto add codex --model gpt-6.2-astra --effort <effort>"),
            "{text}"
        );
        assert!(text.contains("Replace <effort>"), "{text}");
    }
}
