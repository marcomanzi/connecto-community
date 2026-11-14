# Project tree UI: specification

Status: approved by the user on 2026-11-08 (design "B · Tree", proposal v2). Baseline: branch `project-tree-ui` from `main` at `b795ff2`.

The shared contract is code, not prose: `crates/connecto-core/src/terminals.rs` and `crates/connecto-core/src/workspace.rs`. Where this document and those files disagree, the files win. Change them only together with both sides.

## 1. Summary

Connecto runs one project at a time today, and switching means Stop all, Choose…, then starting every terminal again. This work makes Connecto a multi-project workspace:

- A left sidebar shows a **tree**: open projects, each with its terminals. A terminal that an agent starts with `connecto add` sits **under the terminal that started it**.
- Every project and terminal has a **status dot**: working, waiting for you, done, finished-unseen, idle, exited, failed.
- All open projects **keep running** in the background. Clicking a project or terminal only changes what is shown.
- The **top bar is removed**. A native macOS **menu bar** holds File (open project, recents), Terminal, Agents and View.
- **No Stop/Start buttons.** Opening a project starts its terminals at once; a first login starts by itself.
- Closed terminals go to a searchable **history** (the clock icon), from which they can be read or **continued** with the tool's own resume.
- Terminals can be **renamed**. New terminals come from one chooser: Claude or Codex.
- The **relay panel is hidden**, not deleted. Its engine stays; the user will bring it back later.
- Relaunching the app restores every open project, its tree, and the terminal shown, and **resumes each terminal's conversation**.

## 2. Requirements

| Id | Requirement |
| - | - |
| R1 | The workspace keeps an ordered list of open projects, the active project, each project's active terminal and expanded flag, custom project names, sidebar collapsed flag, and up to 10 recents. It is saved in `<data>/workspace.json` with atomic writes; an unreadable file is kept as `workspace.json.bad`, a warning shows, and the app opens with an empty workspace. |
| R2 | Each open project has its own relay and terminal registry, alive for the whole time it is open. Switching projects never stops, starts, or types into anything. `choose_project` and its "Stop all first" rule are removed from the UI path (the trait method may stay for tests, unused by the window). |
| R3 | Opening a project (File > Open, a recent, or launch restore) starts every terminal in its list at once. A project with no saved list starts the default tools (Claude, Codex), as today. |
| R4 | A terminal whose tool is logged out (`AuthState::LoggedOut`) goes straight into the tool's login with no confirm. The confirm stays when a login exists or its state is `Unknown`. |
| R5 | `terminals.json` entries gain `name`, `parent`, `asked_by` and `session` (the tool session id). Lists in today's format load unchanged. |
| R6 | An agent's `connecto add` records the caller as parent, flattened to the top-level ancestor (one nesting level); `asked_by` keeps the real caller when it differs. `connecto add <tool> [--name <text>]` sets the name. Without `--name` the name is set from the first `--ask` body the child receives: its first words, at most 40 characters. A name is at most `MAX_NAME_CHARS` and is refused, never cut, when longer. |
| R7 | `TerminalEntry::state` follows R7a–R7c, from signals the registry already has (turn phase, `needs-user`, pane stage, exit), never from screen text. R7a `NeedsUser` is set by the bridge's `needs-user` event and cleared by the next prompt, turn start, or user keystroke. R7b `Done` applies only to a child whose last `--ask` request ended and which is idle. R7c `finished_turns` increments on each accepted turn end. |
| R8 | Close (user × , ⌘W, or agent `connecto close`) stops the terminal if running, saves a history entry and transcript, and removes it from the list. Its children are closed with it the same way (stopped, saved to the history, removed); an agent add queued by a terminal that has since closed is refused (changed 2026-11-13: children used to move to top level and keep running). The "last terminal is only stopped" rule is gone: a project may have zero terminals. |
| R9 | History lives in `<data>/projects/<id>/history/`: `index.json` (entries, newest first, at most 500; older ones are dropped with their transcripts) and one `<history-id>.txt` per transcript. The transcript is the scrollback text at close, passed through `connecto-redact`, at most 2 MiB (tail kept). `task` and `last_reply` are redacted and cut to 300 characters. |
| R10 | Continue from history opens a new top-level terminal of the same tool and name that resumes the session: Claude `--resume <session-id>` in place of `--session-id`, Codex `resume <session-id>`. An entry with no session id is not resumable and shows transcript only. |
| R11 | Relaunch restores every open project and starts its terminals; a terminal with a saved session id resumes it (as R10). If resume fails at launch, that terminal starts a new conversation and its pane says so once. |
| R12 | One `PauseSwitch` covers every project's agents. Stop Everything pauses, then stops every terminal of every project; nothing is closed. |
| R13 | The window has no top bar or title strip. The sidebar header is the window drag area (double-click zooms); with the sidebar collapsed the terminal header is. Traffic lights sit over the sidebar header. |
| R14 | Native macOS menu bar (muda or equivalent; verify the crate against eframe 0.37 / winit before use): **File** Open Project… ⌘O, Open Recent ▸ (recents, Clear Recent), Rename Project…, Close Project ⌘⇧W. **Terminal** New Terminal… ⌘T, New Claude ⌘⌥1, New Codex ⌘⌥2, Rename Terminal ⌘R, Close Terminal ⌘W, Close Finished Children ⌘⇧K, History… ⌘Y, Restart Terminal. **Agents** Pause All Agents ⌘. (checkmark when paused), Stop Everything. **View** Toggle Sidebar ⌘\, Next Terminal ⌃Tab, Previous Terminal ⌃⇧Tab, Next Needing Me ⌘J, Project 1…9 ⌘1…9. A menu shortcut is consumed by the menu and never reaches the PTY. |
| R15 | The sidebar tree: project rows (twisty, dot, name, history clock on hover/active, terminal count when collapsed), top-level terminal rows (dot, name or id, second line `id · state`), children indented under their parent with a connector line, and a "＋ New terminal ⌘T" row per expanded project. A project dot shows its most urgent terminal: NeedsUser > NeedsLogin > Failed > Working > finished-unseen > Idle/Done > Starting > Exited/NotStarted. The footer holds the Pause switch. Collapsed (⌘\), the sidebar is a 60 px rail of project monograms with dots. |
| R16 | The terminal header shows `project / name  id` and a state chip. Double-click on the name or ⌘R renames inline (Enter saves, Esc cancels, empty clears). Project rows rename the same way. |
| R17 | New terminal chooser (⌘T or the row): Claude / Codex with each tool's login state for the project, an optional name field, Enter picks the highlighted tool. |
| R18 | History sheet (⌘Y or clock): a search field filtering on name, terminal id, task and last reply (case-insensitive substring); filters This project / All projects / Claude / Codex; grouped Today / Yesterday / Earlier; each row offers Continue (if resumable) and View transcript (read-only, selectable, Cmd+C copies). Esc closes. |
| R19 | Status dots: Working = cyan with a slow pulse; NeedsUser and NeedsLogin = amber; Done = green check; finished-unseen = green with a ring (the UI keeps the `finished_turns` it last showed per terminal; the ring clears when the terminal is shown); Idle = green; Exited/NotStarted = hollow grey; Failed = coral. The window repaints for the pulse only while some dot is Working, and honours the existing frame watchdog. |
| R20 | The relay panel and its composer are not drawn. `relay_panel.rs` and the relay engine stay compiled and tested. |
| R21 | Static shell mode (`--pane`, the `frame-capture` seam) still runs, without the sidebar. |
| R22 | Empty states: no open project shows a centred "Open a project ⌘O" with the recents; a project with no terminals shows the chooser inline. |

## 3. Assumptions

- A1 Grandchildren are allowed and drawn at the child level with "asked by <id>" (user, 2026-11-08).
- A2 The open-terminal cap per project rises from 8 to 16 (`MAX_TERMINALS`); closed terminals do not count.
- A3 Resume is supported by both CLIs given the session id (user, 2026-11-08). Claude's id is the one Connecto passes with `--session-id`; Codex's comes from its `SessionStart` hook (`tool_session`).
- A4 History search is over names, ids, task and last reply only. Full-text transcript search is later work.
- A5 The audit log stays one file per run; each record gains the project id.

## 4. Ownership

| Area | Owner | Files |
| - | - | - |
| Contract | coordinator | `connecto-core/src/terminals.rs`, `workspace.rs`, `lib.rs` |
| Backend | backend agent | `connecto-session`, `connecto-workspace` (new), `connecto-adapter`, `connecto-hook`, `connecto-app/src/main.rs` |
| Frontend | frontend agent | `connecto-ui` (all), including `Diagnostics` and `run` |

The app hands the UI `Diagnostics { workspace: Option<Arc<dyn WorkspaceControl>>, .. }` in place of `relay` and `terminals`. Static shell mode passes `workspace: None` and its `PaneConfig`s as today.

## 5. Tests (in addition to keeping today's suites green)

Backend: workspace.json round trip and `.bad` fallback; two projects open at once with independent registries; switching does nothing to either; agent add records parent, flattening, `asked_by`, `--name`, auto-name from the first ask; rename keeps the id and `send` still works; close writes a redacted transcript and index entry and closes its children with it; history cap of 500; continue builds the right resume args for both tools; relaunch resumes with the saved id and falls back once; NeedsUser set and cleared; Done only for children; global pause blocks agent writes in both projects; logged-out tool starts its login without a confirm; old `terminals.json` loads.

Frontend (egui harness with fakes): tree order with children under parents; clicking a terminal in another project shows it and sends no input anywhere; project dot priority; unseen ring appears in a background project and clears when shown; chooser adds the chosen tool with its name; inline rename Enter/Esc/empty; close on a child calls `close`; history filter and Continue; transcript view copies; collapsed rail; drag area starts a window drag only off controls; menu shortcuts never reach the PTY; pulse repaints only while working; empty states.

All tests use the fake tools in `connecto-session/tests/fixtures`; no real CLI, credential or Keychain.
