# 0035: An agent reaches another project's terminals only with `--project`, and only to talk and read

Status: Accepted (2026-11-23), built in
`crates/connecto-session/src/projects.rs` (the project directory),
`crates/connecto-session/src/bridge/server.rs` (`projects`, `in_project`),
`crates/connecto-session/src/bridge/state.rs` (seats, forwards, asks held
across projects), `crates/connecto-hook/src/bridge.rs` and `client.rs` (the
wire op and the flag), `crates/connecto-workspace/src/lib.rs` (filling the
directory) and `crates/connecto-gate/src/lib.rs` (the audit record). Tested
with two relays in one process (`tests/bridge.rs`); not yet tried by hand in
the app.

## Context

The bridge (ADR 0016) belongs to one project: each open project has its own
relay, socket, bridge state and terminal ids, and `connecto` reaches only the
caller's own project. A terminal in project `connecto` cannot see or ask
`claude-12` in project `harbor_app`, even when the user wants exactly
that.

The user decided: it must be possible, but only when asked for explicitly,
and only for talking and reading.

## Decision

1. **A directory of projects.** The workspace keeps one `ProjectDirectory`
   for all relays: every sidebar project with the name the sidebar shows, its
   folder, its id and a weak link to its bridge while it is open, then every
   recent project not in the sidebar, closed, with the id of its real folder
   as an open project's is (a shown path such as `C:\work\app` and the
   canonical `\\?\C:\work\app` are one project). The directory and the
   bridge's instance number live in the relay's shared state, read by the
   handlers and the bridge state alike, so neither can see one without the
   other. It is refreshed on every
   publish of the workspace view, so a project closed or renamed is seen at
   once. A link is weak and a closing relay (`stop` set) counts as closed: the
   directory never keeps a project alive.
2. **`connecto projects`** lists what the directory knows: name, path, this
   project / open / closed, and the terminals of each open project with their
   state and owner (a closed one: how many its saved list holds). It only
   reads, so it works under Pause.
3. **`--project <name|path>`**, explicit every time, on `list`, `read`,
   `send` (with `--ask`), `result`, `watch` and `unwatch`. It names a project
   by its sidebar name, its folder name, its path or its id; a name several
   projects share is refused with their paths. Without the flag nothing
   changes. A bare name is matched only against names and ids, never taken
   as a folder relative to where the app runs; only what reads as a path
   (with a `/` or starting with `~`) is matched as one. `connecto` makes a
   relative path absolute from the folder it runs in before sending it (joined
   as written and resolved by the filesystem, so `link/../dest` goes where
   the link leads), and
   the app refuses one still relative, so `./src` names the caller's `src`,
   never one near the app. The flag naming the
   caller's own project is the same as no flag.
   On the wire it is an op of its own, `in_project`, carrying the inner op,
   so an older app refuses it as unknown instead of ignoring the project and
   acting on its own.
4. **Only talking and reading cross.** `keys`, `add`, `start`, `close`,
   `tasks`, secrets, operations, works, memory and `send-file` are refused
   with `--project` anywhere among their words (up to a `--`, after which the
   words are the payload's own), by the command and again by the app
   (`not_across_projects`, recorded in both projects). A send across projects may not type into a shell.
   Targets must be open to everyone: a terminal an agent started
   ("belongs to") is refused across projects, for read, send, watch and
   unwatch alike, even to that agent's siblings, since ownership is a
   project's own matter. It is decided on the registry's own entry: a
   terminal the registry does not list yet is refused too.
5. **A stranger in the target project.** The caller keeps its identity from
   the process tree (ADR 0016); in the target's bridge state it gets a *seat*,
   a slot number far above any terminal's, so it is never taken for a
   terminal there even when the ids match. A seat is keyed by the caller's
   project, its *bridge instance* (a number unique in the app run: a project
   closed and opened again is a new instance whose terminal numbers start
   again) and its attempt there, so a later conversation never inherits an
   earlier one's seat, watches or results. Its asks and watches are the
   seat's; an ask keeps its own copy of where its answer goes. Notices for it
   (a watched event, the end of its ask, with the reply) are forwarded to its
   own project's bridge state, which puts them in its inbox or wakes it
   exactly as for its own project. A watched event's notice is keyed by the
   project's id, its bridge instance and the terminal, never by its name, so
   an unwatch withdraws exactly its own notices across a rename or two
   projects of one name. A forward is a debt on that state (the
   same lossless list a handler's timed-out send uses), never a message on
   its bounded queue: no thread waits, and nothing is lost while that bridge
   instance runs. One for another instance is dropped. `result` across
   projects reads only the caller's own requests, decided on the identity
   the request saved, not on a seat that may since have been dropped.
6. **What the target sees.** `[connecto from claude-55 in project connecto,
   request rN. Sent by another agent, not the user.]`. The notice an asker
   gets shows the target project's name but names it by its id in the
   command (`connecto result rN --project <id>`), which never changes and
   needs no quoting. The project name in the
   header keeps only letters, digits, space, `.`, `_` and `-`, so a folder
   name cannot end the header or forge a line; the turn tracker binds the
   request id from that form too.
7. **An ask across projects holds the asker's turns** as one at home does: the
   asker's state records it, as a debt owed before the send is reported,
   until the answer is forwarded (an answer that overtakes the record leaves
   a tombstone, so nothing stays held). If the target bridge instance ends
   first, even if the project is opened again, the asker is told so and
   released.
8. **Pause and audit.** A Pause in either project refuses every
   cross-project op, `unwatch` included. Every one, allowed or refused, is
   recorded in both projects' records of the one audit log
   (`cross_project_<verb>`, A5): the caller's names the target as
   `<project id>/<id>`, the target's names the caller as `<project id>/<id>`.
   An allowed one acts only once its intent is durable in both (the Gate's
   200 ms wait); a log that cannot take it refuses it (`audit_failed`). The
   intent gives each project's Pause epoch, and the op checks both again
   right before it acts and before it hands back anything read (a send: on
   the epoch the target's state admitted it in), so a Pause while the log
   was writing still stops it; a watch or unwatch is checked by the target's
   state right before it changes the watch, so one a Pause overtook in the
   queue changes nothing. A refusal is recorded durably too, a closed
   project's under its own id by the caller's gate (one log holds every
   project's record); one the logs cannot take is answered `audit_failed`,
   never lost in silence, keeping all the refusal said (its code as
   `refused_code`, a send's request id and whether text went out). A send
   also passes the target's Gate as any agent's does.
9. **Agents are told it is the exception.** The launch text, `connecto`'s
   usage and the built-in team skills (package version 6) say that the
   commands work in this project, and that another project's terminals are
   used only when the user or the task explicitly asks. A request to check,
   fix or find out something in another project counts as asking (the user's
   decision, 2026-11-23, after an app session answered "no command allows
   it"). The launch text says how such work gets done: ask a terminal open
   to everyone there with `send --ask`, or start a terminal in one's own
   project that does. If no terminal there is open to everyone, the agent
   says so plainly.
10. **Any terminal of a project may be the caller.** Only the target must be
   open to everyone. The caller can be a terminal an agent started for its
   own work, such as a helper an app session starts for a check. Such a
   helper still gets only the verbs of point 4 (`tests/bridge.rs`,
   `an_agent_talks_to_another_project_only_when_it_names_it`).

## Alternatives

- **One bridge for all projects, ids qualified by project.** Simpler routing,
  but every in-project command and text would change, and an agent could
  reach another project by mistake. Refused: the user wants the default to
  stay exactly as it is.
- **A field on the existing ops.** An older app ignores unknown fields, so it
  would act on its own project; refused for the same reason `add_effort` is
  an op of its own.
- **Cross-project `keys`, `add`, `close`.** More than talking; the user kept
  them to each project.
- **Keeping cross-project asks across an update (ADR 0030).** Not done: the
  asker's seat lives in another project's state. An update ends them with no
  answer delivered; the asker can still `read --project` the target.

## Consequences

- An agent can find and ask an agent of another open project, and gets the
  answer in its context as at home. A closed project cannot be reached; only
  the user opens projects.
- A cross-project unwatch withdraws the wanted flags in the caller's own
  inbox through a forward; until it is taken (at the next tick), one more
  wake can come.
- Watches and seats of a conversation that has ended are dropped when a
  forward finds it gone, and at most 1024 seats are kept per project.
- Debts between states are not bounded by a queue: a project flooding
  another with watched events grows that list until the next tick (250 ms).
