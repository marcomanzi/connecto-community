# 0019. Plain shell terminals, and the terminal-kind model

Status: Accepted.

## Decision

A terminal can be the user's login shell. The kind is `ToolKind::Shell` in
`connecto-core` (and `Tool::Shell` in `connecto-store`, which the registry and
launcher use), beside Claude and Codex. A kind is now a value with a name
(`ToolKind::name`, `from_name`, `ALL`) and capabilities (`is_agent`). Gemini CLI,
Cursor CLI and opencode are meant to be further variants; the compiler then points
at every place that has to decide what they do. Not built yet.

## What a shell is

- **Launch** (`launch::start_shell`): `$SHELL` if it is an executable file, else zsh,
  bash or sh, run as `-l -i` in the project folder, with the same small environment
  the agents get (`USER`, `LOGNAME`, `PATH`, `LANG`) plus `HOME`, `SHELL`, `TERM`; its
  own startup files do the rest. No store copy, no signature check, no login, no
  profile, no launch mode, no resume. The bridge reaches it: `CONNECTO_BRIDGE_SOCK`,
  `CONNECTO_TERMINAL` and the `connecto` command on `PATH`. It gets no hook socket
  variables, no hook token and no instructions pasted in. A hook channel is served
  for it anyway and carries nothing: "has a hook channel" is how the relay tells a
  terminal from a login.
- **State**: a running shell has the dot `TermActivity::Plain` (a grey square, "shell").
  It is never working, finished or waiting for the user, because it sends no events.
- **Bridge**: `connecto send <id> '<command>'` types the line and presses Enter, with
  no paste frame and no "from another agent" header; one line per send, no control
  characters (`connecto keys` carries Ctrl-C and the like). `connecto read` is as for
  any terminal. `send --ask`, `watch` are refused (`no_turns`) with a message that says
  to use send and read; there is no `result` to ask for. Multi-line text is refused
  rather than sent as typed lines, so a send is one command with one outcome.
- **Gate and audit**: unchanged. A write to a shell is an agent write on the same
  path: admitted by the gate, audited (`type`, `keys`, `add`, `start`, `close`),
  refused while paused (a pause before Enter leaves no Enter). A remote peer's
  input, resize and add/restart go through the same remote guard. A shell is at least
  as powerful as a bypass-permissions agent, and is treated as one.
- **Automations** read and type into agents only; shells are not listed to them.
- **Remote**: the wire already numbered tools (3 = shell) and states (10 = plain), so
  a peer can add and restart shells and watch their output.

## Two-way assumptions that remain

Per-project tool choice (`ProjectTools`: both, Claude, Codex) and launch modes
(`LaunchModes`: one field per agent) are agent settings and ignore a shell. The
automation store names a terminal by `claude` or `codex`. Packages install
`.claude/`, `.codex/` and `.agents/` per agent. The history and Settings sheets list
two agents. `Tool::team_id/entry` exist only for stored CLIs. A new agent kind has
to touch each of these deliberately.

## Follow-up (not built)

Knowing when a command has finished, via shell-integration markers (OSC 133), would
let `send --ask` work on a shell.
