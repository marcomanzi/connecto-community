# 0032: CLI improvements: a coding CLI improves its own set-up

Status: Accepted and implemented. `connecto-core/src/cli_improvements.rs` holds
the task, model and terminal name. `TerminalsControl::add_prompted` (in
`connecto-session/src/terminals.rs`) opens the terminal. The UI is Settings >
CLI improvements (`connecto-ui/src/settings.rs`, started in `app.rs`). The
site page is `site/docs/cli-improvements.html`.

## Context

Claude Code's `/insights` reads the person's past sessions and writes a
report (`~/.claude/usage-data/report.html`) with suggestions for their
set-up: settings, hooks, CLAUDE.md, skills. Applying them is a chore the
CLI itself can do, with the person watching.

## Decision

1. **Where.** Settings has a section, **CLI improvements**, for tasks a
   coding CLI does on itself. It is in Settings, not the menu bar: the menu
   bar is built once (it is the macOS menu) and cannot show an item only
   while a CLI is installed. Settings is drawn from the agents' state every
   frame.
2. **Only with its CLI.** "Claude auto apply insights" is shown only when
   Claude Code is installed (Settings > Agents says so). Without it the item
   is not there at all, and the section says to install Claude Code.
3. **The terminal.** It is a new Claude Code terminal named
   `claude-insights`, on Opus (`--model opus`), opened with its task as the
   first message through `TerminalsControl::add_prompted`. That method is
   `add_with` with a model, the same path `connecto add --model` and Agentic
   Works take. The terminal is then shown, and Settings closes. It is an
   ordinary terminal: Pause, the audit and the bridge apply.
4. **The project.** It opens in this machine's project on show. While a
   paired machine's project is shown it is not offered, and says why:
   that machine's Claude set-up is its own, and this machine's last
   project is never taken instead. It is not offered either where Claude
   may not start: deactivated in Settings > Agents, or turned off in the
   project's settings (the checks every new terminal passes). Once
   started, the window shows the new terminal over whatever was selected.
   It opens in this machine's active project. `/insights`
   spans all projects and most suggestions are user-level (`~/.claude`),
   but a Connecto terminal belongs to a project, and the one the person is
   working in is the one whose own set-up (CLAUDE.md, `.claude/`) the
   suggestions improve. With no project open the button is disabled and
   says so. Starting it in the home folder would need a project there,
   which Connecto does not make by itself.
5. **The config folder.** Connecto runs Claude with an isolated
   `CLAUDE_CONFIG_DIR` when the project's Claude config is Isolated. So the
   task never assumes `~/.claude`. It has the terminal read
   `${CLAUDE_CONFIG_DIR:-$HOME/.claude}`, use the report path `/insights`
   prints (else `usage-data/report.html` there), and say before changing
   an isolated folder that the change applies to that project's terminals
   only. The nested `claude -p` inherits the terminal's environment, so its
   report is in that folder.
6. **Running `/insights`.** The report is made only when the command is
   invoked directly, not by the model through a tool. So the task tells
   Claude to run `claude -p "/insights"` in its shell, which is a direct
   invocation in print mode. If that fails or the report does not change,
   Claude asks the person to type `/insights`, ends its turn, and goes on
   when told. One first message is enough: there is no second message to
   send when the command ends.
7. **The task.** Read the report, list each suggestion, and apply it where
   it belongs (user level or this project), reading a file before changing
   it and keeping what is there. Ask first, and wait, when in doubt and
   before anything risky: more permissions, hooks that run commands,
   deleting or rewriting, another project, installing, the network. Habits
   are told, not applied. End with a summary.

## Consequences

- The quality of what is applied is the model's. The person sees every step
  in the terminal and answers its questions.
- `claude -p "/insights"` reads many sessions and spends the person's usage.
  That is the cost of `/insights` itself.
- Further CLI improvements (Codex, Pi) go in the same section, each shown
  only with its CLI.
