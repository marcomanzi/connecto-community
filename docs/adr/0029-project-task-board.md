# 0029: A project's task board is a file its app session agent keeps, read by Connecto Chat

Status: Accepted, built in `connecto-flutter-app/lib/board.dart`, the
`board` peer op (`connecto-flutter-app/native/src/lib.rs`) and the hand-off
in `lib/call.dart`. Tested with a fake host and a fake agent. It has not
yet been tried with a real agent keeping the board.

## Context

Voice sessions (ADR 0028) each agree one task and hand it to the project's
app session agent. People agree several tasks in a row and want them run one
at a time, in priority order, even with the phone closed. In the app they
want to see each task's state and move one to the front. Each task is run by
a coder and a reviewer. The person sets how that is done per project, in one
prompt.

The app can only read the app session's chat and send it messages and keys.
The desktop app and the host must not change. The host already serves one
more thing to a paired peer: a file under a terminal's project folder
(`FileRead`, commit cc2a27b), checked against the peer's listing, refused
while paused, and audited.

## Decision

1. **The board is a JSON file in the project, `.connecto/board.json`.**
   The app session agent creates it and keeps it current. It holds
   `howToRun` and `tasks`, in priority order. Each task has an id, title,
   goal, scope, done, a status (queued, running, blocked, done or failed)
   and a one-line note. The agent writes the whole file to a `.tmp` file
   and then moves it over the board, so the app never reads half a file.
   In a git repository it keeps both files out of git: it appends the
   project-scoped pattern (`/<git rev-parse --show-prefix>.connecto/board.json*`)
   to the file `git rev-parse --git-path info/exclude` names, which also
   holds in a linked worktree. The file is on the computer, so the
   queue goes on with the phone closed. It also lasts beyond the agent's
   own context: the agent reads it again before each change.
2. **The app reads it with the host's file read.** The new `board` op in
   the app's Rust peer calls `fetch_file` as the app session terminal (or
   the project's first terminal) for that one fixed path. It returns the
   text, or `null` while there is no file. A project with no terminal to
   read as, or a file that is not UTF-8, is an error, never an empty board.
   This is the app's own peer. The
   host and the desktop app are unchanged. A host from before file reads
   says it is too old. Each read is audited on the host, so the board is
   read every 4 s and only while the board or the project settings are
   open. The chat, which the peer holds locally, is read every second.
3. **The app changes the board only through the agent.** Moving a task to
   the front and saving the project's instructions are chat messages
   (`moveToFrontPrompt`, `setHowToRunPrompt`). One `BoardSync` per project
   lives as long as the app: it keeps the messages not yet sent and the
   changes not yet on the board, so they survive leaving the page, and it
   polls while a page shows the board or a change is outstanding. It sends
   them once the agent is between turns, one at a time; a failed send is
   retried, and its warning goes once one gets through. Until the board shows
   a change it is shown as pending (the moved task first, "Moving to
   front…"; the settings field keeps the saved text). Three minutes after a
   change went out (waiting for the agent does not count), the app stops
   waiting, says so, and lets it be sent again. While the person has not
   edited the settings field, it follows the board. Every message about the board repeats
   the board's rules (`boardRules`), so nothing depends on what the agent
   remembers.
4. **The agent runs the queue.** `taskPrompt` gives each task an id from
   the app. The agent first sets the default `howToRun` if the board has
   none. An id already on the board is not added again: the agent answers
   with its status. Otherwise the task is added as queued. If nothing is
   running or blocked, the first queued task starts, which may be an older
   one. Unless this task started, the reply begins `Queued [id].`. A task
   runs as the board's `howToRun` says at its start. Because the agent's last
   message ends its turn, it is told to change the board and start the
   next coder (`connecto send --ask`) first, then end with one message:
   `Task complete [id].` or `Task failed [id].`, with a line on the task
   that started next. A blocked task waits for the person's answer and holds
   the queue.
5. **Ids in the markers, read apart from reply attribution.** A voice
   session ends on its own `Task complete [id].`, `Task failed [id].` (said
   as a failure, never as done) or `Queued [id].`. It looks for these in
   every agent message added since the hand-off, so a message typed on
   screen, a board change or another task's report in the same poll does
   not hide them. The agent's last message is taken only once its turn is
   over, so it is never said half-written. Another task's tagged report is
   neither said nor ends the call. An untagged "Task complete." still
   counts as before, among the replies to this call. "Queued" and "Task
   failed" count only with this task's id. Ids are removed before speech,
   and the markers are said in the call language.
6. **How tasks run is one prompt per project, kept on the board.** The
   project settings page (from the board) edits `howToRun`. The agent reads
   it from the board for every task it starts, so it applies without the
   phone. Every task prompt carries the default, and the agent writes it to
   the board when there is none. The default is a coder, a Claude terminal
   on Opus, and a reviewer, a Codex terminal on `gpt-6.2-astra` at medium
   effort. They iterate until the review has nothing left that matters.
   `gpt-6.2-astra` is the Astra model in Codex's own list (`models_cache.json`,
   which the `models` op reads), and the one this computer's Codex config
   uses by default.

## Consequences

- No host or desktop change. A plain file can be read and fixed by hand,
  and `git` never sees it.
- The board is only as exact as the agent keeps it. A malformed write
  keeps the last good board on screen, with a warning (and says so when
  there is none yet). Unknown statuses read as queued. A task listed twice
  counts once, where it first appears as a valid task. A task without an id
  is shown but cannot be moved.
- `connecto add` takes `--model` but no effort. The reviewer's medium
  effort comes from the host's Codex config (`model_reasoning_effort`),
  which is medium on this computer. To pin the effort per terminal,
  `connecto add` would need an `--effort` option, which is a host change.
  Update 2026-11-21: `connecto add` now takes `--effort <level>`, kept
  with the terminal like `--model`; the built-in teams start the Codex
  reviewer with `--model gpt-6.2-astra --effort medium`.
- Moving a task never preempts the running one. It changes what runs
  next.
- Each board read adds a line to the host's audit log, every 4 s while
  the board is open.
- Update 2026-11-23: a voice session no longer ends on `Queued [id].`. It
  can start several tasks in a row, each with its own id, and follows the
  reports of all of them (ADR 0028, "several tasks in one call").
