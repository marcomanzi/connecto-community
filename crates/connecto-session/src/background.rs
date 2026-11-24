//! Background work a Claude terminal left running: what it is, and stopping
//! its background shells.
//!
//! Claude Code runs a background shell as a direct child of the `claude`
//! process: a shell started with `-c`, whose script sources one of Claude's
//! shell snapshots and then runs the command as `eval '<command>'`. Only such
//! children are ever signalled, and only those running a listed shell task's
//! command. MCP servers and other children never match. Subagents, monitors
//! and workflows are not processes of their own and are never stopped.

use std::time::Instant;

use connecto_core::{BackgroundWork, LONG_RUNNING, short_age};
use serde_json::{Value, json};

use crate::history;
use crate::process_tree::Known;
use crate::relay::Slot;
use crate::turn::Running;

/// How long a stopped shell has after SIGTERM before it is killed.
#[cfg(unix)]
const GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// One running task as a slot keeps it: what is shown, the raw command a
/// shell runs, which only the matching below reads, and, for a shell, the
/// process it was tied to when it was first seen running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Task {
    pub work: BackgroundWork,
    pub command: Option<String>,
    /// The shell's process, by pid and start time: the one shell of the
    /// terminal running this command when the task was first seen, if there
    /// was exactly one. A single stop signals only this process.
    pub process: Option<Known>,
}

/// The tracker's running tasks, labelled for display: redacted, on one line,
/// and cut, as history fields are.
pub(crate) fn tasks(running: &[Running]) -> Vec<Task> {
    running
        .iter()
        .map(|task| {
            let label = history::field(task.label()).unwrap_or_else(|| task.kind().to_owned());
            let description = task
                .report
                .description
                .as_ref()
                .and_then(|text| history::field(text.expose()))
                .filter(|text| *text != label);
            let command = (task.kind() == "shell")
                .then(|| task.report.command.as_ref().map(|c| c.expose().clone()))
                .flatten();
            Task {
                work: BackgroundWork {
                    id: task.report.id.expose().clone(),
                    kind: task.kind().to_owned(),
                    label,
                    description,
                    since: task.since,
                },
                command,
                process: None,
            }
        })
        .collect()
}

/// [`tasks`] with each shell tied to its process: a task seen before keeps
/// the process `previous` gave it; a new one is tied to the one shell of
/// `root` that runs its command now, among the shells no other task holds
/// (see [`bind`]). The process list is read only when there is a new shell.
pub(crate) fn tasks_bound(running: &[Running], previous: &[Task], root: Option<i32>) -> Vec<Task> {
    let mut list = tasks(running);
    let mut new = Vec::new();
    for (at, task) in list.iter_mut().enumerate() {
        let seen = previous
            .iter()
            .find(|old| old.work.id == task.work.id && old.work.since == task.work.since);
        match seen {
            Some(old) => task.process = old.process,
            None if task.work.is_shell() && task.command.is_some() => new.push(at),
            None => {}
        }
    }
    let Some(root) = root.filter(|_| !new.is_empty()) else {
        return list;
    };
    let Ok(procs) = read_ps() else {
        return list;
    };
    let children = crate::process_tree::descendants(root);
    let shells = shells_of(&list, &new);
    let pids = assign(&procs, root, &shells);
    let mut pids = pids.into_iter();
    for at in list
        .iter()
        .enumerate()
        .filter(|(_, task)| task.work.is_shell())
        .map(|(at, _)| at)
        .collect::<Vec<_>>()
    {
        if let Some(Some(pid)) = pids.next() {
            list[at].process = children.iter().copied().find(|known| known.pid == pid);
        }
    }
    list
}

/// The shell tasks of `list`, in order, for [`assign`]: every one, with or
/// without a command, as one whose command is not known could be running in
/// any shell. `new` lists the positions first seen now.
fn shells_of<'a>(list: &'a [Task], new: &[usize]) -> Vec<Shell<'a>> {
    list.iter()
        .enumerate()
        .filter(|(_, task)| task.work.is_shell())
        .map(|(at, task)| Shell {
            command: task
                .command
                .as_deref()
                .filter(|command| !command.trim().is_empty()),
            bound: task.process.map(|known| known.pid),
            new: new.contains(&at),
        })
        .collect()
}

/// A reported shell task, for [`assign`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shell<'a> {
    /// What it runs; `None` when Claude did not say (or said nothing but
    /// spaces): it is never tied itself, and could be in any shell.
    pub command: Option<&'a str>,
    /// The pid it was tied to before, if any.
    pub bound: Option<i32>,
    /// First seen now: only these are tied here.
    pub new: bool,
}

/// The pid each new shell task is tied to, in order (`None` for the others).
/// A tie must be unique both ways: the task runs in exactly one shell of
/// `root` that no task holds (see [`bind`]), and that shell runs the command
/// of exactly one task not tied yet, a task whose command is not known
/// counting for every shell. Two `cargo test` tasks with one shell left (the
/// other ended before the list was read) tie neither, whichever comes first:
/// that shell could be either's; so does a `cargo test` task beside a shell
/// task that did not say what it runs. Every choice is made from the
/// same starting point, so the order of the list does not matter.
pub(crate) fn assign(procs: &[Proc], root: i32, shells: &[Shell<'_>]) -> Vec<Option<i32>> {
    let taken: Vec<i32> = shells.iter().filter_map(|shell| shell.bound).collect();
    shells
        .iter()
        .map(|shell| {
            let command = shell
                .command
                .filter(|_| shell.new && shell.bound.is_none())?;
            let pid = bind(procs, root, command, &taken)?;
            let line = &procs.iter().find(|proc| proc.pid == pid)?.command;
            let claimants = shells
                .iter()
                .filter(|other| other.bound.is_none())
                .filter(|other| {
                    other
                        .command
                        .is_none_or(|command| runs_as_task(line, command))
                })
                .count();
            (claimants == 1).then_some(pid)
        })
        .collect()
}

/// The pid of the one snapshot shell of `root`, not in `taken`, that runs
/// `command` whole (or, for a command Claude clipped, by its start). `None`
/// when there is none, or more than one: two shells running the same
/// command cannot be told apart.
pub(crate) fn bind(procs: &[Proc], root: i32, command: &str, taken: &[i32]) -> Option<i32> {
    let found: Vec<i32> = snapshot_shells(procs, root)
        .filter(|proc| !taken.contains(&proc.pid))
        .filter(|proc| runs_as_task(&proc.command, command))
        .map(|proc| proc.pid)
        .collect();
    match found.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// Whether a shell's command line runs `command` as one task: whole, or, for
/// a command Claude clipped (which cannot match whole), by its start.
fn runs_as_task(command_line: &str, command: &str) -> bool {
    let clipped = without_clip_marker(command).len() != command.len();
    fit(command_line, command).is_some_and(|how| clipped || how == Fit::Exact)
}

/// The process list, as [`parse_ps`] reads it.
fn read_ps() -> Result<Vec<Proc>, String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=,ppid=,command="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|err| format!("The process list could not be read: {err}"))?;
    Ok(parse_ps(&String::from_utf8_lossy(&output.stdout)))
}

/// The tasks as `list` and `tasks` give them: id, type, label, and how long
/// each has run, in seconds and in words.
pub(crate) fn json(tasks: &[Task], now: Instant) -> Value {
    Value::Array(
        tasks
            .iter()
            .map(|task| {
                let age = task.work.age(now);
                json!({
                    "id": task.work.id,
                    "type": task.work.kind,
                    "label": task.work.label,
                    "description": task.work.description,
                    "seconds": age.as_secs(),
                    "age": short_age(age),
                    "long": age >= LONG_RUNNING,
                })
            })
            .collect(),
    )
}

/// Tasks named for a sentence: `python3 grind.py (shell, 2 h 20 min)`, the
/// first three and how many more.
pub(crate) fn summary(tasks: &[Task], now: Instant) -> String {
    const NAMED: usize = 3;
    let mut parts: Vec<String> = tasks
        .iter()
        .take(NAMED)
        .map(|task| {
            format!(
                "{} ({}, {})",
                task.work.label,
                task.work.kind,
                short_age(task.work.age(now))
            )
        })
        .collect();
    if tasks.len() > NAMED {
        parts.push(format!("{} more", tasks.len() - NAMED));
    }
    parts.join("; ")
}

/// Whether `old` and `new` list the same tasks, first seen at the same times.
pub(crate) fn same(old: &[Task], new: &[Running]) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|(task, running)| {
            task.work.id == *running.report.id.expose() && task.work.since == running.since
        })
}

/// One process, as the process list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// The command line, its arguments joined by spaces.
    pub command: String,
}

/// Read `ps -o pid=,ppid=,command=` output.
pub(crate) fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.trim_start().splitn(2, char::is_whitespace);
            let pid = words.next()?.parse().ok()?;
            let rest = words.next()?.trim_start();
            let mut words = rest.splitn(2, char::is_whitespace);
            let ppid = words.next()?.parse().ok()?;
            let command = words.next().unwrap_or("").trim_start().to_owned();
            Some(Proc { pid, ppid, command })
        })
        .collect()
}

const SHELLS: [&str; 6] = ["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Whether `command` is a shell Claude started from one of its snapshots.
fn is_snapshot_shell(command: &str) -> bool {
    let program = command.split_whitespace().next().unwrap_or("");
    let name = program.rsplit('/').next().unwrap_or(program);
    SHELLS.contains(&name)
        && command.contains(" -c ")
        && command.contains("/shell-snapshots/snapshot-")
}

/// How well a snapshot shell's command line holds a task's command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fit {
    /// Only by its start: the command was cut short, or the shell runs a
    /// longer one (`sleep 60` for `sleep 6`).
    Start,
    /// The whole command, quoted and closed.
    Exact,
}

/// How a snapshot shell's command line runs `task`, if it does: Claude
/// writes it as `eval '<task>'`, each `'` in it as `'"'"'`. A command the hook
/// cut short still matches by its start.
fn fit(command_line: &str, task: &str) -> Option<Fit> {
    let whole = task;
    let task = without_clip_marker(task);
    // A command Claude clipped is longer than what is left of it, so a shell
    // running exactly that much runs something else.
    let clipped = task.len() != whole.len();
    if task.trim().is_empty() {
        return None;
    }
    let quoted = task.replace('\'', r#"'"'"'"#);
    let needle = format!("eval '{quoted}");
    let mut best = None;
    for (at, _) in command_line.match_indices(&needle) {
        let rest = &command_line[at + needle.len()..];
        // Closed: the quote ends here, and is not the start of a `'"'"'`.
        let closed = rest
            .strip_prefix('\'')
            .is_some_and(|after| !after.starts_with('"'));
        let this = match (closed, clipped) {
            (true, true) => continue,
            (true, false) => Fit::Exact,
            (false, _) => Fit::Start,
        };
        best = best.max(Some(this));
    }
    best
}

fn runs(command_line: &str, task: &str) -> bool {
    fit(command_line, task).is_some()
}

/// `task` without the `… [+N chars]` marker Claude appends to a command it
/// clipped (at 1000 characters), so the start that is left can match.
fn without_clip_marker(task: &str) -> &str {
    let Some(at) = task.rfind("\u{2026} [+") else {
        return task;
    };
    let rest = &task[at + "\u{2026} [+".len()..];
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && &rest[digits..] == " chars]" {
        &task[..at]
    } else {
        task
    }
}

/// `root`'s direct children that are snapshot shells.
fn snapshot_shells(procs: &[Proc], root: i32) -> impl Iterator<Item = &Proc> {
    procs
        .iter()
        .filter(move |proc| proc.ppid == root && proc.pid > 1 && proc.pid != root)
        .filter(|proc| is_snapshot_shell(&proc.command))
}

/// The pids of `root`'s direct children that are snapshot shells running one
/// of `commands`.
pub(crate) fn matching_shells(procs: &[Proc], root: i32, commands: &[String]) -> Vec<i32> {
    snapshot_shells(procs, root)
        .filter(|proc| commands.iter().any(|task| runs(&proc.command, task)))
        .map(|proc| proc.pid)
        .collect()
}

/// What a stop covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// Every running shell task: the shells running any of these commands.
    All(Vec<String>),
    /// One task: only the process it was tied to when first seen, while it
    /// is still that process (same pid and start time) and runs `command`.
    /// Never a shell picked by its command at stop time.
    One {
        command: String,
        process: Option<Known>,
    },
}

/// What a single stop says when the task was never tied to one process.
pub(crate) const NOT_TOLD_APART: &str = "This background shell cannot be told apart: when it \
     started, no single process could be tied to it (another runs the same command). Stop all \
     background shells instead.";

/// What a single stop says when its process is no longer the one it was tied to.
pub(crate) const ENDED: &str = "That background shell has already ended: the process it \
     started as is gone, so nothing was stopped.";

/// The pids a stop signals. For all: every snapshot shell running one of the
/// commands. For one task: only the pid it was tied to, while that pid is
/// still a shell of `root` running its command; whether it is still the same
/// process (its start time) is checked where it is signalled. A shell running
/// the same command again, or a longer one, is never taken in its place.
pub(crate) fn chosen_shells(
    procs: &[Proc],
    root: i32,
    target: &Target,
) -> Result<Vec<i32>, String> {
    let gone =
        || "No process was found for the background shells: they may have just ended.".to_owned();
    let (command, process) = match target {
        Target::All(commands) => {
            let found = matching_shells(procs, root, commands);
            return if found.is_empty() {
                Err(gone())
            } else {
                Ok(found)
            };
        }
        Target::One { command, process } => (command, process),
    };
    let known = process.ok_or_else(|| NOT_TOLD_APART.to_owned())?;
    let still = snapshot_shells(procs, root)
        .any(|proc| proc.pid == known.pid && runs_as_task(&proc.command, command));
    if still {
        Ok(vec![known.pid])
    } else {
        Err(ENDED.to_owned())
    }
}

/// What a stop request is told when no shell task is running.
pub(crate) fn nothing_to_stop(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        "That terminal has no background work running.".to_owned()
    } else {
        "Only background shells can be stopped. The work still running is a subagent, a \
         monitor or a workflow: it ends on its own, or stop it from inside the terminal."
            .to_owned()
    }
}

/// What a stop covers: every running shell task, or only `task` and the
/// process it was tied to. `Err` says why nothing would be stopped.
pub(crate) fn stop_target(tasks: &[Task], task: Option<&str>) -> Result<Target, String> {
    let commands = shell_commands(tasks, task)?;
    let Some(id) = task else {
        return Ok(Target::All(commands));
    };
    let process = tasks
        .iter()
        .find(|each| each.work.id == id)
        .and_then(|each| each.process);
    let command = commands.into_iter().next().unwrap_or_default();
    Ok(Target::One { command, process })
}

/// The commands of the shell tasks a stop covers: every running shell task,
/// or only `task`. `Err` says why nothing would be stopped.
pub(crate) fn shell_commands(tasks: &[Task], task: Option<&str>) -> Result<Vec<String>, String> {
    let chosen: Vec<&Task> = match task {
        Some(id) => {
            let found = tasks
                .iter()
                .find(|each| each.work.id == id)
                .ok_or_else(|| format!("No background task {id:?} is running there."))?;
            if !found.work.is_shell() {
                return Err(format!(
                    "{id} is a {}: only background shells can be stopped.",
                    found.work.kind
                ));
            }
            vec![found]
        }
        None => tasks.iter().filter(|each| each.work.is_shell()).collect(),
    };
    let commands: Vec<String> = chosen
        .iter()
        .filter_map(|each| each.command.clone())
        .filter(|command| !command.trim().is_empty())
        .collect();
    if commands.is_empty() {
        if chosen.is_empty() {
            return Err(nothing_to_stop(tasks));
        }
        return Err(
            "Claude did not say which command that background shell runs, so it cannot be found."
                .to_owned(),
        );
    }
    Ok(commands)
}

/// What a stop that found `count` shells says.
pub(crate) fn stopping(count: usize) -> String {
    let what = if count == 1 {
        "1 background shell".to_owned()
    } else {
        format!("{count} background shells")
    };
    format!(
        "Stopping {what}. Claude reports each as failed, tells the agent, and the terminal's turn ends."
    )
}

/// Stop the background shells of `slot`'s terminal, or only `task`. With
/// `wait`, the process list is read now and the reply says how many were
/// found; without, the checks that need no process list run now and the rest
/// on a thread, so a UI never waits, and a refusal found there (no shell, or
/// one that cannot be told apart) goes to `refused`.
pub(crate) fn stop(
    slot: &Slot,
    task: Option<&str>,
    wait: bool,
    refused: impl FnOnce(String) + Send + 'static,
) -> Result<String, String> {
    let target = stop_target(&slot.background_shown(), task)?;
    let root = slot
        .pane
        .session()
        .and_then(|session| session.pid())
        .ok_or_else(|| "That terminal is not running.".to_owned())?;
    // Windows refuses at once, with its reason.
    if wait || cfg!(not(unix)) {
        return stop_shells(root, &target).map(stopping);
    }
    stop_later(root, target, refused)
}

/// [`stop_shells`] on a thread; its refusal goes to `refused`. Returns at once
/// with what the stop is about to do.
pub(crate) fn stop_later(
    root: i32,
    target: Target,
    refused: impl FnOnce(String) + Send + 'static,
) -> Result<String, String> {
    let count = match &target {
        Target::All(commands) => commands.len(),
        Target::One { .. } => 1,
    };
    std::thread::Builder::new()
        .name("background-stop".into())
        .spawn(move || {
            if let Err(why) = stop_shells(root, &target) {
                refused(why);
            }
        })
        .map_err(|err| format!("Could not start the stop: {err}"))?;
    Ok(stopping(count))
}

/// Stop the background shells of the `claude` process `root` that `target`
/// covers (see [`chosen_shells`]): SIGTERM to each shell's process group (when it leads one of
/// its own), the shell and everything under it, and SIGKILL to what is left
/// after a short grace. Returns how many shells were found. Never blocks for
/// the grace.
#[cfg(unix)]
pub(crate) fn stop_shells(root: i32, target: &Target) -> Result<usize, String> {
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::{Pid, getpgid};

    use crate::process_tree::{alive, descendants, kill as sigkill};

    let procs = read_ps()?;
    let shells = chosen_shells(&procs, root, target)?;
    let root_group = getpgid(Some(Pid::from_raw(root))).ok();
    let children = descendants(root);
    // One task: still the very process it was tied to, not a new one that
    // got its pid.
    if let Target::One {
        process: Some(bound),
        ..
    } = target
        && !children.contains(bound)
    {
        return Err(ENDED.to_owned());
    }
    let mut listed = Vec::new();
    for shell in &shells {
        let Some(known) = children.iter().copied().find(|known| known.pid == *shell) else {
            continue;
        };
        let under = descendants(*shell);
        let pid = Pid::from_raw(*shell);
        let group = getpgid(Some(pid)).ok();
        if group == Some(pid) && group != root_group {
            let _ = killpg(pid, Signal::SIGTERM);
        }
        for each in std::iter::once(known).chain(under.iter().copied()) {
            if alive(each) {
                let _ = kill(Pid::from_raw(each.pid), Signal::SIGTERM);
            }
            listed.push(each);
        }
    }
    let found = shells.len();
    let _ = std::thread::Builder::new()
        .name("background-stop".into())
        .spawn(move || {
            let deadline = Instant::now() + GRACE;
            while Instant::now() < deadline && listed.iter().copied().any(alive) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            for known in listed.into_iter().filter(|known| alive(*known)) {
                sigkill(known);
            }
        });
    Ok(found)
}

/// Windows: not built yet.
#[cfg(not(unix))]
pub(crate) fn stop_shells(_root: i32, _target: &Target) -> Result<usize, String> {
    Err("Stopping background shells is not supported on Windows yet.".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use connecto_core::{BackgroundReport, EventText};

    use super::*;

    const SNAPSHOT: &str = "/bin/zsh -c source '/Users/u/.claude/shell-snapshots/snapshot-zsh-1795041369345-sbsol9.sh' 2>/dev/null || true && setopt NO_EXTENDED_GLOB 2>/dev/null || true && eval ";

    fn shell(pid: i32, ppid: i32, command: &str) -> Proc {
        let quoted = command.replace('\'', r#"'"'"'"#);
        Proc {
            pid,
            ppid,
            command: format!(
                "{SNAPSHOT}'{quoted}' < /dev/null && pwd -P >| '/tmp/claude-1a2b-cwd'"
            ),
        }
    }

    fn proc(pid: i32, ppid: i32, command: &str) -> Proc {
        Proc {
            pid,
            ppid,
            command: command.to_owned(),
        }
    }

    #[test]
    fn the_process_list_is_read_with_spaces_kept_in_the_command() {
        let text =
            "    1     0 /sbin/launchd\n  95790 89006 /bin/zsh -c eval 'a  b'\nnot a line\n 7 1\n";
        assert_eq!(
            parse_ps(text),
            [
                proc(1, 0, "/sbin/launchd"),
                proc(95790, 89006, "/bin/zsh -c eval 'a  b'"),
                proc(7, 1, ""),
            ]
        );
    }

    /// Only the claude process's own snapshot shells running a listed command
    /// match: never an MCP server, a shell of another process, a grandchild, or
    /// a shell running something else.
    #[test]
    fn only_claudes_snapshot_shells_running_a_listed_command_match() {
        let claude = 500;
        let procs = [
            shell(601, claude, "python3 grind.py big.log"),
            shell(602, claude, "npm run dev"),
            shell(603, 999, "python3 grind.py big.log"),
            shell(604, 601, "python3 grind.py big.log"),
            proc(
                605,
                claude,
                "node /opt/mcp/server.js python3 grind.py big.log",
            ),
            proc(606, claude, "/bin/zsh -c eval 'python3 grind.py big.log'"),
            shell(607, claude, "grep -E 'it''s' log | wc -l"),
            proc(608, 601, "python3 grind.py big.log"),
        ];
        let commands = |list: &[&str]| list.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            matching_shells(&procs, claude, &commands(&["python3 grind.py big.log"])),
            [601]
        );
        assert_eq!(
            matching_shells(
                &procs,
                claude,
                &commands(&["npm run dev", "grep -E 'it''s' log | wc -l"])
            ),
            [602, 607]
        );
        // A command the hook cut short matches by its start.
        assert_eq!(
            matching_shells(&procs, claude, &commands(&["python3 grind.py"])),
            [601]
        );
        // Claude's clip marker is not part of the command line.
        assert_eq!(
            matching_shells(
                &procs,
                claude,
                &commands(&["python3 grind.py\u{2026} [+1234 chars]"])
            ),
            [601]
        );
        assert!(matching_shells(&procs, claude, &commands(&["\u{2026} [+12 chars]"])).is_empty());
        assert!(matching_shells(&procs, claude, &commands(&["", "  "])).is_empty());
        assert!(matching_shells(&procs, claude, &commands(&["cargo test"])).is_empty());
    }

    fn one(command: &str, process: Option<i32>) -> Target {
        Target::One {
            command: command.to_owned(),
            process: process.map(|pid| Known::for_tests(pid, 1)),
        }
    }

    /// Two shells running the same command cannot be told apart: neither
    /// task is tied to a process, so stopping one is refused, while stopping
    /// all takes both. A shell another task already holds is not a candidate.
    #[test]
    fn two_shells_running_the_same_command_are_tied_to_neither_task() {
        let claude = 500;
        let procs = [
            shell(601, claude, "sleep 60"),
            shell(602, claude, "sleep 60"),
        ];
        assert_eq!(bind(&procs, claude, "sleep 60", &[]), None);
        assert_eq!(bind(&procs, claude, "sleep 60", &[601]), Some(602));
        assert_eq!(
            chosen_shells(&procs, claude, &one("sleep 60", None)),
            Err(NOT_TOLD_APART.to_owned())
        );
        assert_eq!(
            chosen_shells(&procs, claude, &Target::All(vec!["sleep 60".to_owned()])),
            Ok(vec![601, 602])
        );
        // The task list carries what each task was tied to.
        let now = Instant::now();
        let mut list = tasks(&[
            running("b1", Some("shell"), Some("sleep 60"), now),
            running("b2", Some("shell"), Some("sleep 60"), now),
        ]);
        assert_eq!(
            stop_target(&list, Some("b2")).unwrap(),
            one("sleep 60", None)
        );
        list[1].process = Some(Known::for_tests(602, 1));
        assert_eq!(
            stop_target(&list, Some("b2")).unwrap(),
            one("sleep 60", Some(602))
        );
        assert_eq!(
            stop_target(&list, None).unwrap(),
            Target::All(vec!["sleep 60".to_owned(), "sleep 60".to_owned()])
        );
        // A task seen before keeps its process; without a process list to
        // read (no root), a new one is tied to nothing.
        let again = tasks_bound(
            &[
                running("b1", Some("shell"), Some("sleep 60"), now),
                running("b2", Some("shell"), Some("sleep 60"), now),
                running("b3", Some("shell"), Some("sleep 60"), now),
            ],
            &list,
            None,
        );
        assert_eq!(again[1].process, Some(Known::for_tests(602, 1)));
        assert_eq!(again[2].process, None);
    }

    /// Two `cargo test` tasks are reported, and one of their shells ended
    /// before the process list was read: the shell left could be either's, so
    /// neither task is tied to it, in either order of the list. A task whose
    /// command only one task runs is still tied.
    #[test]
    fn a_shell_two_reported_tasks_could_be_is_tied_to_neither() {
        let claude = 500;
        let procs = [
            shell(602, claude, "cargo test"),
            shell(603, claude, "npm run dev"),
        ];
        let task = |command| Shell {
            command: Some(command),
            bound: None,
            new: true,
        };
        let twins = [task("cargo test"), task("cargo test")];
        assert_eq!(assign(&procs, claude, &twins), [None, None]);
        let mixed = [task("cargo test"), task("npm run dev"), task("cargo test")];
        assert_eq!(assign(&procs, claude, &mixed), [None, Some(603), None]);
        let reversed = [task("npm run dev"), task("cargo test"), task("cargo test")];
        assert_eq!(assign(&procs, claude, &reversed), [Some(603), None, None]);
        // An older task still not tied counts as a claimant too.
        let old = Shell {
            new: false,
            ..task("cargo test")
        };
        assert_eq!(
            assign(&procs, claude, &[old, task("cargo test")]),
            [None, None]
        );
        assert_eq!(
            assign(&procs, claude, &[task("cargo test"), old]),
            [None, None]
        );
        // One tied before holds its shell; a new one takes the shell left.
        let procs = [
            shell(601, claude, "cargo test"),
            shell(602, claude, "cargo test"),
        ];
        let held = Shell {
            bound: Some(601),
            new: false,
            ..task("cargo test")
        };
        assert_eq!(
            assign(&procs, claude, &[held, task("cargo test")]),
            [None, Some(602)]
        );
        assert_eq!(
            assign(&procs, claude, &[task("cargo test"), held]),
            [Some(602), None]
        );
        // Alone, a task is tied to its shell.
        assert_eq!(
            assign(&procs[1..], claude, &[task("cargo test")]),
            [Some(602)]
        );
    }

    /// A `cargo test` task beside a shell task that did not say what it runs
    /// (no command, or only spaces), with one `cargo test` shell left: it could
    /// be the other task's, so neither is tied, in either order; a binding
    /// made before stays.
    #[test]
    fn a_shell_task_with_no_command_could_own_any_shell() {
        let claude = 500;
        let procs = [shell(602, claude, "cargo test")];
        let task = |command| Shell {
            command,
            bound: None,
            new: true,
        };
        let known = task(Some("cargo test"));
        let unknown = task(None);
        assert_eq!(assign(&procs, claude, &[known, unknown]), [None, None]);
        assert_eq!(assign(&procs, claude, &[unknown, known]), [None, None]);
        let older = Shell {
            new: false,
            ..unknown
        };
        assert_eq!(assign(&procs, claude, &[known, older]), [None, None]);
        // Tied before, it keeps its shell; the unknown one claims nothing then.
        let held = Shell {
            bound: Some(602),
            new: false,
            ..known
        };
        assert_eq!(assign(&procs, claude, &[held, unknown]), [None, None]);
        // Through the task list: no command, or only spaces.
        let now = Instant::now();
        for missing in [None, Some("   ")] {
            let list = tasks(&[
                running("b1", Some("shell"), Some("cargo test"), now),
                running("b2", Some("shell"), missing, now),
            ]);
            let shells = shells_of(&list, &[0, 1]);
            assert_eq!(assign(&procs, claude, &shells), [None, None], "{missing:?}");
        }
    }

    /// `sleep 6` is not `sleep 60`: a task is tied to the shell running its
    /// whole command, never to one that only starts the same way.
    #[test]
    fn a_task_is_tied_only_to_the_shell_running_its_whole_command() {
        let claude = 500;
        let procs = [
            shell(601, claude, "sleep 6"),
            shell(602, claude, "sleep 60"),
            shell(603, claude, "echo 'it'"),
            shell(604, claude, "echo 'it''s'"),
            shell(605, 999, "cargo test"),
        ];
        assert_eq!(bind(&procs, claude, "sleep 6", &[]), Some(601));
        assert_eq!(bind(&procs, claude, "sleep 60", &[]), Some(602));
        assert_eq!(bind(&procs, claude, "echo 'it'", &[]), Some(603));
        assert_eq!(bind(&procs[1..], claude, "sleep 6", &[]), None);
        assert_eq!(bind(&procs, claude, "cargo test", &[]), None);
        // A command cut short is tied by its start, never to a shell running
        // exactly the part left.
        let clipped = "sleep 6\u{2026} [+1 chars]";
        assert_eq!(bind(&procs, claude, clipped, &[]), Some(602));
        assert_eq!(bind(&procs[..1], claude, clipped, &[]), None);
        // The tied shell, while it runs the command, is what one stop takes.
        assert_eq!(
            chosen_shells(&procs, claude, &one("sleep 6", Some(601))),
            Ok(vec![601])
        );
    }

    /// The task's own shell has ended and another runs the same command (a
    /// rerun in the foreground), or a longer one: one stop takes neither.
    #[test]
    fn one_stop_never_takes_another_shell_once_its_own_has_ended() {
        let claude = 500;
        let rerun = [shell(602, claude, "cargo test")];
        assert_eq!(
            chosen_shells(&rerun, claude, &one("cargo test", Some(601))),
            Err(ENDED.to_owned())
        );
        let longer = [shell(602, claude, "sleep 60")];
        assert_eq!(
            chosen_shells(&longer, claude, &one("sleep 6", Some(601))),
            Err(ENDED.to_owned())
        );
        // Its pid now runs something else.
        let reused = [shell(601, claude, "npm run dev")];
        assert_eq!(
            chosen_shells(&reused, claude, &one("cargo test", Some(601))),
            Err(ENDED.to_owned())
        );
    }

    fn running(id: &str, kind: Option<&str>, command: Option<&str>, since: Instant) -> Running {
        let text = |t: &str| EventText::new(t.to_owned());
        Running {
            report: BackgroundReport {
                id: text(id),
                kind: kind.map(text),
                description: Some(text("Grind the log")),
                command: command.map(text),
            },
            since,
        }
    }

    #[test]
    fn a_shell_keeps_its_raw_command_and_shows_a_redacted_one_line_label() {
        let now = Instant::now();
        let secret = "curl -H 'Authorization: Bearer sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdef'\n  https://x";
        let list = [
            running("b1", Some("shell"), Some(secret), now),
            running(
                "a2",
                Some("subagent"),
                Some("ignored"),
                now + Duration::from_secs(1),
            ),
        ];
        let tasks = tasks(&list);
        assert_eq!(tasks[0].command.as_deref(), Some(secret));
        assert!(
            !tasks[0].work.label.contains("sk-ant-api03"),
            "{:?}",
            tasks[0].work.label
        );
        assert!(!tasks[0].work.label.contains('\n'));
        assert!(tasks[0].work.is_shell());
        assert_eq!(tasks[1].command, None);
        assert_eq!(tasks[1].work.label, "Grind the log");
        assert_eq!(tasks[1].work.kind, "subagent");
        assert!(same(&tasks, &list));
        assert!(!same(&tasks, &list[..1]));
    }

    #[test]
    fn a_stop_covers_the_running_shells_or_the_one_named() {
        let now = Instant::now();
        let list = tasks(&[
            running("b1", Some("shell"), Some("python3 grind.py"), now),
            running("a2", Some("subagent"), None, now),
            running("b3", Some("shell"), Some("npm run dev"), now),
            running("b4", Some("shell"), None, now),
        ]);
        assert_eq!(
            shell_commands(&list, None).unwrap(),
            ["python3 grind.py", "npm run dev"]
        );
        assert_eq!(shell_commands(&list, Some("b3")).unwrap(), ["npm run dev"]);
        assert!(
            shell_commands(&list, Some("a2"))
                .unwrap_err()
                .contains("subagent")
        );
        assert!(
            shell_commands(&list, Some("zz"))
                .unwrap_err()
                .contains("No background task")
        );
        assert!(
            shell_commands(&list, Some("b4"))
                .unwrap_err()
                .contains("did not say")
        );
        assert!(
            shell_commands(&list[1..2], None)
                .unwrap_err()
                .contains("Only background shells")
        );
        assert!(stopping(1).starts_with("Stopping 1 background shell."));
        assert!(stopping(2).starts_with("Stopping 2 background shells."));
    }

    #[test]
    fn tasks_are_listed_with_their_age_and_summed_up_for_a_sentence() {
        let start = Instant::now();
        let list = tasks(&[
            running("b1", Some("shell"), Some("python3 grind.py"), start),
            running(
                "a2",
                Some("subagent"),
                None,
                start + Duration::from_secs(8000),
            ),
        ]);
        let now = start + Duration::from_secs(8400);
        let value = json(&list, now);
        assert_eq!(value[0]["id"], "b1");
        assert_eq!(value[0]["type"], "shell");
        assert_eq!(value[0]["label"], "python3 grind.py");
        assert_eq!(value[0]["seconds"], 8400);
        assert_eq!(value[0]["age"], "2 h 20 min");
        assert_eq!(value[0]["long"], true);
        assert_eq!(value[1]["long"], false);
        assert_eq!(
            summary(&list, now),
            "python3 grind.py (shell, 2 h 20 min); Grind the log (subagent, 6 min)"
        );
        let many: Vec<Task> = (0..5).map(|_| list[1].clone()).collect();
        assert!(summary(&many, now).ends_with("; 2 more"));
    }

    #[test]
    fn nothing_to_stop_says_why() {
        assert!(nothing_to_stop(&[]).contains("no background work"));
        let other = tasks(&[running("a", Some("subagent"), None, Instant::now())]);
        assert!(nothing_to_stop(&other).contains("Only background shells"));
    }

    /// The real stop, on throwaway shells this test starts: `zsh -c`
    /// children of a stand-in `claude` (this test process cannot be
    /// signalled, so the stand-in is a `sh` that waits), shaped as Claude runs
    /// a background command. The task is tied to its shell when first seen;
    /// one stop ends that shell and its `sleep`. The stand-in then runs the
    /// same command again in a new shell: the old task's stop is refused and
    /// the new shell runs on, until all are stopped.
    #[cfg(unix)]
    #[test]
    fn a_single_stop_takes_its_own_shell_and_never_a_rerun_of_the_same_command() {
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!("connecto-bg-{}", std::process::id()));
        let snapshots = dir.join("shell-snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        let snapshot = snapshots.join("snapshot-sh-1-test.sh");
        std::fs::write(&snapshot, "true\n").unwrap();
        let task = "sleep 31 ; echo 'done'";
        let quoted = task.replace('\'', r#"'"'"'"#);
        let script = format!(
            "source '{}' 2>/dev/null || true && eval '{quoted}' < /dev/null",
            snapshot.display()
        );
        // The stand-in claude runs the shell in the background, waits, and
        // then runs the same command again.
        let shell = format!("/bin/sh -c {} & wait", sh_quote(&script));
        let inner = format!("{shell}; {shell}");
        let mut claude = Command::new("/bin/sh")
            .args(["-c", &inner])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let root = i32::try_from(claude.id()).unwrap();
        let wait_for = |what: &str, mut done: Box<dyn FnMut() -> bool + '_>| {
            let deadline = Instant::now() + Duration::from_secs(8);
            while !done() {
                assert!(Instant::now() < deadline, "waiting for {what}");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        let seen = |previous: &[Task]| {
            tasks_bound(
                &[running("b1", Some("shell"), Some(task), Instant::now())],
                previous,
                Some(root),
            )
        };
        let mut first = None;
        wait_for(
            "the shell to be tied to its task",
            Box::new(|| {
                first = seen(&[])[0].process;
                first.is_some()
            }),
        );
        let first = first.unwrap();
        let own = Target::One {
            command: task.to_owned(),
            process: Some(first),
        };
        assert_eq!(stop_shells(root, &own), Ok(1));
        // The same command again, in a new shell.
        let mut second = None;
        wait_for(
            "the rerun",
            Box::new(|| {
                second = seen(&[])[0].process.filter(|known| *known != first);
                second.is_some()
            }),
        );
        let second = second.unwrap();
        assert_eq!(stop_shells(root, &own), Err(ENDED.to_owned()));
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            crate::process_tree::alive(second),
            "the rerun was not stopped"
        );
        assert_eq!(
            stop_shells(root, &Target::All(vec![task.to_owned()])),
            Ok(1)
        );
        // The shell ends, so the stand-in's last `wait` returns and it exits.
        let mut status = None;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            status = claude.try_wait().unwrap();
            if status.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if status.is_none() {
            let _ = claude.kill();
        }
        let _ = claude.wait();
        let _ = std::fs::remove_file(&snapshot);
        let _ = std::fs::remove_dir(&snapshots);
        let _ = std::fs::remove_dir(&dir);
        assert!(status.is_some(), "the background shells were not stopped");
        assert!(stop_shells(root, &Target::All(vec!["sleep 31".to_owned()])).is_err());
    }

    #[cfg(unix)]
    fn sh_quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', r#"'"'"'"#))
    }
}
