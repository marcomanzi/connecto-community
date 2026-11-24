# 0044: A project uses any set of coding CLIs, checked where every terminal starts

Status: Accepted (2026-11-24), built in `crates/connecto-core/src/packages.rs`
(`ProjectTools`), `crates/connecto-session/src/project_settings.rs`
(`tool_list`, `tool_refusal`), `crates/connecto-session/src/terminals.rs`
(the registry's add and start), `crates/connecto-session/src/pane.rs`
(`start_with`), the bridge (`connecto add` / `start`), `crates/connecto-net`
(settings on the wire, the host's add and restart, the remote chooser),
`crates/connecto-ui` (Tools checkboxes, the choosers, the Agentic Works
authoring row), `crates/connecto-app/src/ops_host.rs`, the packages editor and
Connecto Chat (`lib/main.dart`, `lib/board.dart`). Tested with unit and bridge
tests, among them
`the_tools_setting_is_checked_wherever_a_terminal_starts`,
`a_set_of_tools_is_saved_so_older_builds_still_read_it`,
`an_older_peers_queued_write_keeps_the_set_of_tools_written_before_it` and
`a_peer_add_of_a_cli_the_project_leaves_out_is_refused`. Not yet tried by hand.

## Context

A project's Tools setting allowed one coding CLI or all of them. The user
wants any set: Codex and Pi, say, and nothing else offered, installed or
started.

## Decision

- **The setting is a set of one or more CLIs** (`ProjectTools`, never empty).
  The project settings sheet shows a checkbox per CLI and All; the last one
  checked cannot be unchecked.
- **Saved so older builds still work.** `settings.json` keeps `tools`, a word
  an older build reads (the CLI's own for a set of one, `all` for any other
  set: a superset, so such a build still offers everything chosen), and adds
  `tool_list`, the exact set. A reader takes `tool_list` while `tools` is
  still what that set writes; when an older build has changed `tools` since,
  that change wins. Known limit: an older build that changes Codex + Pi to
  Claude and back to all before a newer build reads the file leaves a file
  that looks unchanged, so Codex + Pi comes back.
- **On the wire** settings carry the same two fields. A write from a peer
  from before sets says one CLI or all; it is resolved on the packages
  worker, in turn with the other writes, against the project's set (kept when
  the peer's word is what the set shows it). A host from before sets is
  edited with one-or-all choices only.
- **Enforced at one boundary.** Every agent terminal of a project is added by
  the registry and started by its pane: the registry's add (the window, every
  app helper, a paired client, Agentic Works) refuses a CLI the saved setting
  leaves out with "This project does not use …: its Tools setting allows only
  …", making no entry; a pane's start (a user's start or restart, an agent's
  `connecto start`, a saved terminal brought back at launch) ends failed with
  the same reason and launches nothing. The setting is read from the file each
  time, and again right before the process is spawned (after any wait for
  the project's package install), so a change made while a start waits
  counts. A pane's login, which runs the CLI too, is refused the same way. A
  refused terminal is no longer brought back by itself: its saved
  "running", "working" and "interrupted" are cleared (its conversation and
  the refusal shown stay). A terminal already running keeps its turn. The bridge and the host
  refuse earlier with the same words, so an agent or a peer is told at once.
- **Callers choose sensibly.** Operations (Fix, Discover, a recipe's Run)
  open the first CLI the project allows that opens; the Agentic Works
  authoring row offers only the allowed authoring CLIs (Claude, Codex) and
  says why when neither is; the package editor, a Claude session, says that
  Connecto Home leaves Claude out instead of starting; automation sessions
  run their automation's own agent and report the refusal; the choosers on
  the desktop and in Connecto Chat show only the host project's allowed
  agents once its settings are known, and none while they load.
- **Exempt: hidden utility sessions.** Titles and Functional Memory run a
  hidden utility session (`LaunchExtras::utility`) chosen by what is
  installed and signed in. They are Connecto's own, never a terminal of the
  project, and are not checked.

## Consequences

- A project of Codex and Pi gets only `.codex` and `.agents` from its
  packages, and nothing can start Claude in it.
- The built-in team skills still name Claude, Codex and Antigravity; a team
  in a project that leaves those out is told by `connecto add` which CLIs it
  may use. Teams that adapt to any set are separate work.
