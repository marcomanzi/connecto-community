# Agent terminal bridge: specification

Status: first release, ready to build. Baseline: branch `main` at commit `e0c380c`. Later fixes are expected: the user's direction is "we should work in steps, let's try the first release and then we fix things as we go."

> **Superseded: version pinning.** Connecto no longer pins CLI versions (A32, R20, the "manual mode" and "unverified version" wording below). The bridge is on for every Claude and Codex version, and only a project's `disableAllHooks` or an unreadable Codex hook hash turns a terminal's bridge off.

All numbers in this document (timeouts, queue sizes, caps) are first-release defaults unless a requirement fixes them.

**How ids work.** `R` is a requirement (section 4), `AC` and `T` are acceptance criteria and tests (section 7), `A` is an assumption, `L` a known limit and `O` an open point (section 9), `Q` a recorded decision (section 10). `Check` is a short real-tool run (section 8.2).

**Read these first.** The user has not reviewed all of them:

| Item | Where |
| - | - |
| Assumptions A33 to A47, and the changed A25 | Section 9.1 |
| **A32 and A7, corrected after the user accepted them.** Send to a terminal on an unverified version stays denied | Section 9.3 |
| Known limits L5 to L12. L8, L10, L11 and L12 have not been shown to the user | Section 9.2 |
| Check 2, the one point that returns to the user if it fails | Section 8.2 |
| Open points: none remain, and the closed ones show where each was settled | Section 9.6 |

---

## 1. Summary

Connecto is a macOS desktop app that runs Claude Code and Codex side by side in real terminals, with a manual relay between them. Today an LLM in one terminal has no idea the other terminals exist. To get work from another tool, it has to run `claude -p` or `codex exec`, a separate one-shot process that shares nothing with the terminal the user can see.

This work lets an LLM in a Connecto terminal see what the other terminals are doing, read them, use them without approval, ask another agent something and read just the answer, and be told about events there. It does this through a `connecto` shell command that every terminal gets. Connecto also loses its sandbox, so agents can use the whole Mac, and gains up to 8 terminals per project.

---

## 2. Scope

### 2.1 Included

Three steps, in this order. Each step leaves Connecto usable and testable.

| Step | Contents | Requirements |
| - | - | - |
| 1 | Remove the sandbox, and switch off Claude's own cross-session messaging in the same change | R9, R10, R11, R15 |
| 2 | Any number of terminals up to 8, tabs, a saved terminal list | R12, R13 (user side), R16, R17 |
| 3 | The bridge, in five slices 3a to 3e (section 8.1) | R1 to R8, R13 (agent side), R14, R18, R19, R20 |

R15 sits in step 1 because the sandbox is what blocks Claude's messaging route today. No build may have the sandbox gone and that route open.

The bridge covers every terminal Connecto runs (A3), and only agents inside Connecto terminals get it (A4). Reach is the agent's own project, and Connecto runs one project at a time.

### 2.2 Non-goals

- CLIs other than Claude Code and Codex. No extension point for them is built (A9).
- Anything from the failed `automations-attempt` branch: the offline test block, draft guards, shared profiles, several projects, automations, Jev. No code from that branch is copied.
- Supervised mode.
- Resuming a conversation. Every start is a new conversation.
- Removing `codex exec`, `claude -p` or the scripts that use them. They stay (A6).
- Limiting Claude to its project folder. That stays future work (A13).
- Metering usage (A46).

---

## 3. Terms

### People and programs

| Term | Meaning |
| - | - |
| Agent | The LLM running in a Connecto terminal, through Claude Code or Codex |
| Tool | Claude Code or Codex, the CLI an agent runs in |
| Terminal | One Claude or Codex process in a PTY inside Connecto, with a stable id such as `claude`, `codex` or `claude-2` |
| PTY | The pseudo-terminal a tool runs in. Connecto owns it, so Connecto can type into it and read its screen |
| Target | The terminal an operation acts on |
| Caller | The terminal whose agent ran a `connecto` command |
| Bridge | Everything in step 3: the `connecto` command, the socket it talks to, and the state behind it |
| Hook | A command a tool runs at a set point, such as when a prompt is submitted. Hooks are how Connecto learns what a tool is doing |
| `notify` | Codex's setting that runs a program when a turn completes, passing one JSON argument |
| Hook helper | The `connecto-hook` binary. Tools run it as their hook. It forwards the payload to Connecto over the hook socket |
| Hook socket | The existing per-terminal Unix socket for hook frames. It is one-way: the server answers only with an acknowledgement byte |
| Bridge socket | The new single Unix socket for `connecto` commands. It answers requests |
| Session actor | The per-terminal thread (`SessionActor`) that handles that terminal's events and writes to its PTY |
| Gate | `connecto-gate`, the policy check every action passes through, in the order pause, target, policy, limits, audit |
| Audit | Connecto's hash-chained log of deliveries and control actions |
| Relay, manual Send | The existing panel where a user click sends text from one terminal to another |
| Pause | The existing button that blocks deliveries. In this work it also blocks agent actions (R14) |
| Stop all | The existing button that pauses, then stops every terminal |
| Store | `<data>/cli`, Connecto's private copies of the tools and the hook helper |
| `<data>` | Connecto's data folder: `~/Library/Application Support/connecto/`, or `CONNECTO_DATA_DIR` if set |
| Profile | A tool's login and settings folder for one project, under `<data>/projects/<id>/profiles/<tool>/`. Each new login makes a new numbered generation, such as `g2` |
| Verified version | Claude Code 2.1.318 and Codex 0.166.0, the only versions the bridge works on |
| Manual mode | How Connecto runs a tool on an unverified version: typing in the terminal works, and the relay is denied to it |
| Bridge off | A terminal has no bridge, because its version is unverified or its Codex hook hash cannot be read (A32). Section 6.2 lists what that refuses |
| Bypass flags | `--dangerously-skip-permissions` (Claude) and `--dangerously-bypass-approvals-and-sandbox` (Codex). They turn off each tool's own approval checks. `--yolo` is an alias of the Codex flag, and the launch keeps the long form (A11) |
| ADR | An architecture decision record in `docs/adr/` |

### Turns and states

| Term | Meaning |
| - | - |
| Prompt | Text the tool accepts as input, reported by its `UserPromptSubmit` hook |
| Turn | One run of the tool, from a prompt to the end of its reply |
| Open prompt | A prompt the tool has accepted and whose turn has not ended |
| Self-started turn | A turn Claude starts by itself when background work reports back. Its prompt starts with `<task-notification>` |
| Background work | Subagents and shells Claude left running after its turn ended, listed in the `Stop` hook's `background_tasks` field |
| Main thread | The Codex conversation the user sees, as opposed to the threads of its subagents or its hidden title thread |
| Turn log | The tool's own record of a session on disk: Claude's transcript or Codex's rollout file |
| Phase | What the turn tracker believes a running terminal is doing: `Working` (a turn is running), `Background` (the tool is at its prompt, and background work continues), or `Idle` |
| Busy | The terminal's phase is `Working`. `Background` and `Idle` are both "not busy", because the tool is at its prompt |
| Attempt | One start of a terminal. Starting again makes a new attempt |
| Preflight | The checks a terminal runs before spawning: version, login and project folder |
| Slot | An in-memory position for a terminal, used by the relay. A slot can be reused after a terminal is removed. Terminal ids never are |

### Events

These five are the only events the bridge reports. Each is a claim by the terminal that reported it, with no extra proof (A28).

| Event | Meaning | Claude Code signal | Codex signal |
| - | - | - | - |
| `began` | The phase became `Working` | `UserPromptSubmit` hook | `UserPromptSubmit` hook, new in this work |
| `finished` | The turn ended and no background work is left. On Codex, only the main thread's turn ended (L8) | `Stop` hook with an empty `background_tasks` | `notify` of type `agent-turn-complete` for the main thread |
| `interrupted` | The turn was interrupted | A `user` entry in the transcript, such as `[Request interrupted by user for tool use]`. No hook fires | A `turn_aborted` record in the rollout, best effort (A44) |
| `needs-user` | The tool is asking for the user | `Notification` hook, except types `idle_prompt` and `auth_success`. Unknown types count | Not produced (A15) |
| `exited` | The terminal's process ended | Process exit | Process exit |

### The bridge

| Term | Meaning |
| - | - |
| Type | To paste text into a target's PTY with bracketed paste, wait a settle time, then press Enter |
| Watch | A standing request by one terminal, the watcher, to be woken on every event in another terminal. It is made with `connecto watch` |
| Watcher | A terminal that holds a watch on another terminal |
| Notice | A short line generated by Connecto, telling an agent that an event happened or a request ended |
| Wake | Typing a notice into a not-busy terminal, which starts a turn there |
| Inbox | Connecto's in-memory list of notices for one terminal, not yet delivered |
| Context | Text a hook prints for the tool, which the tool adds to the model's input alongside a prompt. Both tools' docs call the field `additionalContext` |
| Drain | The hook helper fetching a terminal's inbox at `UserPromptSubmit` and printing it as context |
| Wanted notice | A notice from a watched terminal, or the end of an `--ask` request, that has not yet been the subject of a wake. A wake that was never typed gives the flag back |
| Request | One `connecto send` that was not refused before any paste. It has a request id, and records a result |
| Request id | The id Connecto generates for a request: `r` and a number, such as `r12`. It travels in the header of the typed text, and the target's hook sees it at prompt submission |
| `--ask` | The `send` option that tells the sender when its request ends |
| Result | The target's final reply for the turn that handled a request, and nothing else |
| Turn tracker | The code in each session actor that turns tool events into phases, the five events and request endings |
| Turn record | The tracker's entry for one closed turn: the prompts it closed, their requests, and the latest reply |
| Roster | The list of terminals and their states, as `connecto list` shows it |
| `LeftWorking` | An internal signal sent whenever a terminal's phase leaves `Working`. It is not an event and is never reported. Going to `Background` sends it, and emits no `finished` |

---

## 4. Requirements

| Id | Requirement | Step | Decided by |
| - | - | - | - |
| R1 | An agent in a Connecto terminal knows the other terminals exist, what is happening in them, and that it can use them | 3 | Original request |
| R2 | It can choose to read everything in another terminal: visible screen and scrollback, as text | 3 | Q28, A33 |
| R3 | It can use another terminal without approval: prompts, any keys and menus. Always on, on verified versions. Its text is typed at once, even if the target is busy or not ready | 3 | Q2, Q3, Q23 |
| R4 | It gets the five events: `began`, `finished`, `interrupted`, `needs-user`, `exited` | 3 | Q5, Q27, A15 |
| R5 | A not-busy agent is woken only if it asked to watch that terminal, and the wake is typed into its terminal. Every R4 event wakes a not-busy watcher, `began` included. Busy agents and non-watchers get notices as context at their next prompt. An agent that stops being busy while holding a wanted notice gets one wake, so a watcher isn't left waiting for a prompt that may never come, apart from the cases in L12. Non-watchers are never woken | 3 | Q4, Q20, A25, A40 |
| R6 | R1 to R5 hold in both directions, and between terminals of the same tool | 3 | Q19 |
| R7 | No limit stops agents waking each other | 3 | Q6, A16 |
| R8 | The result goes beyond what `codex exec` and `claude -p` give today | 3 | Q28 |
| R9 | The sandbox is removed from `main`. Both tools always run with their bypass flags. There is no Supervised mode | 1 | Q15, Q16 |
| R10 | The refusal of the home folder as a project survives that removal. The rule moves out of `connecto-contain` | 1 | Q8b |
| R11 | `features.unified_exec_tty=false` is dropped once the sandbox is gone, and not before | 1 | Q8, Q15 |
| R12 | Connecto runs any number of Claude and Codex terminals, up to a cap of 8 | 2 | Q17, Q24 |
| R13 | An agent can add a terminal, and can close any terminal at any time, except under Pause (R14). Closing is Stop followed by removal, as the UI's remove button does (changed after the first manual run). The last terminal, and any terminal while the list cannot be written, is only stopped and stays listed, and the reply says why. A stopped conversation cannot be continued from Connecto | 2, 3 | Q17, Q18, Q22, A22 |
| R14 | Pause is a full brake on agent writes, keys, add, start, stop and wakes. Stop all still ends terminals | 3 | Q19, A21, A34 |
| R15 | Other Claude sessions cannot message a Connecto terminal (`crossSessionInbound: refuse`). **Changed after the first manual run:** Claude's own outbound `SendMessage` and `ListAgents` are no longer denied, so a skill can message its subagents. Those sends bypass the bridge's audit and Pause, and L3's extra turns may come back. Claude also loads the project's skills, agents and settings (`--setting-sources user,project,local`; project hooks run alongside Connecto's, PoC threat T11, accepted) | 1 | Q19 |
| R16 | The terminal list is saved. A terminal stopped by the user or by exit stays listed as stopped until the user removes it. An agent's close removes it (R13) | 2 | Q22 |
| R17 | Terminals are shown as tabs, one visible at a time | 2 | Q25 |
| R18 | A lean channel lets LLMs on different CLIs talk: an agent asks another agent something, and can read just the result of that request, without reading the whole terminal and without `claude -p` or `codex exec`. "Lean" means the smallest set of operations that meets the answers (A43) | 3 | Q28 |
| R19 | `finished` means the turn ended and no background work is left. Interrupts are detected separately and reported as their own event. On Codex the first release can honour this only for the main thread's turn (L8) | 3 | Q27 |
| R20 | A new Connecto build can put its hook helper into the store without changing the stored Claude or Codex copies, so the bridge stays on the verified versions | 3 | A32 |

---

## 5. Architecture

### 5.1 Overview

An agent runs `connecto <operation>` through its normal shell tool. The command talks to one Unix socket in the app. Connecto works out which terminal called from the process tree, passes the action through the Gate, and either answers from memory or types into the target's PTY.

```
 Terminal A (Claude)                     connecto-app                     Terminal B (Codex)
 +-----------------+                                                     +-----------------+
 | Bash tool       |  bridge socket   +---------------+  session actor   |  PTY            |
 |  `connecto ...` -+----------------->| bridge server |--> FIFO typing --+->  paste, Enter  |
 |                 |<-- reply --------| bridge state  |    Gate, audit   |                 |
 |  hooks          |                  +-------^-------+                  |  hooks, notify  |
 |  connecto-hook --+--hook socket---->  session actor  <---hook socket---+-- connecto-hook  |
 |  (prints inbox) |<--bridge socket--  turn tracker                     |  rollout tail   |
 +-----------------+                                                     +-----------------+
```

Three rules hold the design together:

- **Session actors stay the only writers to a PTY.** Every byte an agent causes passes through the target's actor.
- **The hook socket stays one-way.** Anything that needs an answer goes over the bridge socket.
- **The bridge never decides on untrusted content.** Text from terminals is stored and handed over, and wake text contains only ids that Connecto generated.

### 5.2 Project rules that continue to apply

- macOS only. Rust 1.95 or later, edition 2024. `unsafe_code` is denied, and clippy denies `unwrap_used`, `expect_used` and `panic`.
- No async runtime: threads and bounded `crossbeam-channel` queues (ADR 0002).
- UI methods return promptly and never block. No session work, relay step or Stop may depend on a UI pass, because a hidden window runs none (ADR 0011).
- `crates/connecto-session/tests/dependency_rules.rs` still holds: only `connecto-session` depends on `connecto-gate` and `connecto-pty`, `connecto-adapter` and `connecto-hook` stay below session and gate, and the UI sees only `connecto-core`.
- `--dangerously-bypass-hook-trust` is never used.
- Only `USER`, `LOGNAME`, `PATH` and `LANG` are inherited into a terminal. MCP stays off: Claude runs with `--strict-mcp-config` and no `--mcp-config`.
- Any policy change bumps `POLICY_REVISION`, which every audit record carries.

### 5.3 Step 1: remove the sandbox, switch off Claude messaging

| Area | Change |
| - | - |
| Workspace | Delete the untracked `crates/connecto-workspace/` first (A17). It holds only build output and has no manifest, so it stops the `main` checkout loading, because `Cargo.toml` has `members = ["crates/*"]`. Remove `connecto-contain` from `Cargo.toml`, `crates/connecto-session/Cargo.toml` and `Cargo.lock` |
| Home folder rule | New `connecto-session/src/project_folder.rs` holds `validate_project`, `ForbiddenRoots` and `PathError`, moved as `main` has them, without `sbpl_string` (R10, A19) |
| Launch | `contained.rs` becomes `launch.rs`. Remove `check_links`, `prepare_profile`, `run_canary`, `sandboxed`, `LaunchError::Canary` and `LaunchError::ProjectLinks`. The `PtySpec` runs the store binary directly |
| Login status | `auth.rs` runs the status command directly |
| Log in | `new_generation` stops deleting older generations (A14). `has_leftover_profile` and `LEFTOVER_TEXT` go, because a kept generation is now normal and the warning would always show |
| Codex | Drop `features.unified_exec_tty=false` (R11), and invert the test that pins it |
| Claude (R15) | `settings()` adds `"crossSessionInbound":"refuse"` and nothing else on permissions. Launch uses `--setting-sources user,project,local`, and adds no `--disallowedTools` (changed after the first manual run) |
| Text | Remove the UI badge "Writes contained. Reads and network open.", "Starting in the sandbox", the sandbox comments and the README section "What is and is not contained". Rewrite the login notes |
| Tests | Delete or adapt `tests/contained.rs`, `tester_attacks.rs`, `tester_real_attacks.rs` and `crates/connecto-contain/tests`. `default_build.rs` needs a new anchor, because it uses "Writes contained" to prove it read the right binary |
| ADRs | New ADR 0013 supersedes 0003, 0004, 0008, 0009 and 0012 |

The bypass flags are already passed on `main`. It is the outer sandbox that still limits writes, new PTYs and Unix sockets.

On Claude 2.1.318 with the bypass flags, measured outside Connecto: `crossSessionInbound: refuse` blocked inbound messages, and both the `permissions.deny` setting and `--disallowedTools` removed the tools. Limits of this are in L1 and L3.

### 5.4 Step 2: any number of terminals

#### Components

| Component | Where | Responsibility |
| - | - | - |
| Terminal registry | `connecto-session/src/terminals.rs`, one thread | Owns the 8 slots, ids and the saved list. All add, remove, start and close requests pass through it, so nothing on the UI thread touches the disk |
| Relay | `connecto-session/src/relay.rs` | One `SessionActor` per occupied slot. `SessionActor` holds its own `Arc<ManagedPane>` rather than indexing a fixed `Vec` |
| `TerminalsControl` | `connecto-core/src/terminals.rs` | The trait the UI sees: `view()`, `add(tool)`, `remove(id)`. Every method returns promptly |
| Tab strip | `connecto-ui/src/app.rs` | Reconciles `App.panes` with `view()` by id each frame, and draws one pane |

`SessionRef.session` stays a `usize` and now means a slot. Audit records already name targets by string. A late `end_delivery` on a reused slot does nothing, because it matches on the delivery id.

#### Data model

`<data>/projects/<id>/terminals.json`, written by temp file and rename:

```json
{
  "version": 1,
  "next": { "claude": 3, "codex": 2 },
  "terminals": [
    { "id": "claude", "tool": "claude", "desired": "running" },
    { "id": "codex", "tool": "codex", "desired": "running" },
    { "id": "claude-2", "tool": "claude", "desired": "stopped" }
  ]
}
```

- Ids match `^[a-z0-9-]{1,32}$`, are never reused, and are the display name.
- `desired` becomes `stopped` when the user stops a terminal, an agent closes it, or it exits by itself. App shutdown does not touch it. Today `shut_down` calls `relay.stop_all()`, so it needs a separate internal path, or every quit would save "all stopped" and break A37.
- On load, a file counts as unreadable if it has bad JSON, an unknown version or tool, more than 8 entries, a bad or duplicate id, or no entries. It is renamed to `terminals.json.bad`, the default two terminals are used, and a visible warning shows.
- A file that exists but cannot be read, an I/O error rather than bad content, or a `.bad` rename that fails, puts the project in **read-only list mode** for this run. The file may be sound, so Connecto never writes over it. The default two are used, and a warning shows. Add and remove are refused with `save_failed`. Starts and stops work and are held in memory only. A relaunch tries to read the file again.
- With no file, a project starts with one Claude and one Codex terminal.

#### Behaviour

- **Choosing a project.** `choose_project` still needs nothing live. It loads that project's list and replaces the slots. Terminals with `desired: running` start with new conversations, and stopped ones stay stopped. Section 5.12 says what happens to bridge state.
- **Add.** The registry writes the new list first, with the new entry as `running` and `next` advanced. Only if that write succeeds does it fill a slot and start the terminal, at once and at the current grid size. Running terminals are not touched or restarted (A22). It gets a new conversation, in the project folder, on the project's login (A12). If the write fails, nothing changes: no slot, no start, `next` unchanged, so the unused id was never given out. An agent gets `save_failed` with the reason, and the user sees it in the tab strip.
- **Cap.** 8 listed terminals, stopped ones included. The ninth is refused with a reason (A35).
- **Remove.** The user's button, or an agent's close once its process has ended (the registry removes it on a tick; a close that cannot remove it says why). Stopped terminals only, and the last one cannot go (A37). The new list is written first, and the slot is freed only if that succeeds. Otherwise the terminal stays listed and the tab strip shows why.
- **Changes of `desired`** (stop, close, exit, start, a cancelled start) never wait on the disk, because Stop must always work (R14). The process change happens, `desired` changes in memory, and then the list is written. If that write fails, the list is marked unsaved and a warning shows in the tab strip until a save succeeds. Every later change writes the whole list, and while unsaved the registry retries every 30 s. The agent's reply still succeeds, and adds `"saved": false`. What survives a relaunch is the last list that was written: until a retry succeeds, a terminal may come back running when it was stopped, or the reverse, and the warning says so.
- **What the user sees.** `TerminalsView` carries the save state and the last error. The tab strip shows a warning, the UI's add and remove report their failures there, and the methods still return at once.
- **Tabs.** Each tab shows id, tool, state and an activity mark. Agent activity never changes the visible tab. The user can add, close and remove terminals there. Only the visible tab polls snapshots. Sessions keep running with no UI pass.
- **Shell seam.** `--pane` shells under `frame-capture` keep a static `PaneConfig` list and appear as tabs.
- **Serialised changes.** Add, remove, start, close and `choose_project` all pass through the registry thread, so no check-then-act spans two of them. `choose_project` also refuses while any pane is in preflight, which today's `is_live` test misses. Remove calls `pane.close()`, which cancels any start in progress.

Update the README text "two real terminals". New ADR 0014 records the slots, the saved list and the reservation below.

#### Profile reservation

Terminals of one tool share a profile, each with its own conversation (A23). Today nothing makes login and starts exclusive. `start_login` checks `LIVE_PROFILES`, rotates the generation and launches, without claiming anything, and `session_dirs` reads the current generation with no lock. So two logins could run at once, or a start could land on a generation a login is about to replace. A `ProfileClaim` also lasts until its `LiveSession` is dropped, so a stopped terminal kept for reading would block login for the others.

`LIVE_PROFILES` becomes a reservation table keyed by tool root, which is one entry per project and tool. One mutex covers every decision below.

```rust
struct ProfileUse { sessions: Vec<SessionClaim>, login: bool }
// SessionClaim: unbound while launching, then bound to the session's PtySession.
```

| Action | Under the lock | Refused with |
| - | - | - |
| Reserve a session | `login` must be false. Read the current generation, add an unbound claim | "Another terminal is logging in" |
| Reserve a login | `login` must be false, and no claim unbound or `Running`. Rotate the generation, set `login` | The same, or `LaunchError::SessionLive` |
| Session ends | The claim stays in the table but stops counting once its status is not `Running`, the test `login` uses for its own session. It is removed on drop | |
| Login ends | The login claim clears `login` when dropped. It is held by the login's `LiveSession`, which the pane drops as soon as login exits | |

- Generation selection is inside the lock, so a session always launches on the generation it reserved, and a login rotates only with nothing launching.
- `Shared::login` no longer drops its own stopped session first. A refused login therefore keeps that screen, and a successful one replaces it.
- A start refused by a running login ends in `Failed` with that reason, and can be started again.
- The reservation is per app process, like `LIVE_PROFILES` today. Two copies of the app on one project are not covered.

### 5.5 Step 3: components

| Component | Where | Responsibility |
| - | - | - |
| `connecto` client | `connecto-hook` binary, chosen by program name | Parses the command, sends one request, prints the reply. No logic of its own |
| Hook helper | `connecto-hook` binary, as today | Forwards hook frames. With `--context` it also drains the inbox and prints it |
| Wire format | `connecto-hook/src/bridge.rs` | Request and reply types, shared with the server. `connecto-hook` gains `serde_json` |
| Bridge server | `connecto-session/src/bridge/server.rs` | Owns the socket, finds the caller, runs a handler thread per connection |
| Bridge state | `connecto-session/src/bridge/state.rs`, one thread | Owns the roster, watches, inboxes and requests. Decides wake or inbox |
| Turn tracker | `connecto-session/src/turn.rs`, in `SessionActor` | Turns tool events into phases, events and request endings |
| Turn log tail | `connecto-session/src/turn.rs` | Reads new lines of the turn log while a turn runs, to spot interrupts and activity |
| Agent typing | `SessionActor` | Runs paste, settle and Enter for agent text, keys and wakes, one at a time |
| Codex hook trust | `connecto-session/src/codex_trust.rs` | Reads hook keys and hashes during preflight |
| Gate agent path | `connecto-gate` | `Actor::Agent`, `decide_agent`, `agent_submit`, `agent_note` |
| Scrollback export | `connecto-term/src/emulator.rs` | `text_all()` |
| Helper refresh | `connecto-store` | `refresh_hook` and the `connecto` link |

### 5.6 Gate agent path

`Actor::Session` becomes `Actor::Agent`. `decide_deliver` still denies it with `NotUser`, so an agent can never use the manual path or its confirmation logic.

```rust
pub enum AgentAction { Type, Keys, Wake, Add, Start, Close }

pub struct AgentRequest {
    pub action: AgentAction,
    pub source: String,            // calling terminal id, or "connecto" for a wake
    pub target: SessionRef,
    pub target_name: String,
    pub message: String,           // r12, or a key/wake id
    pub digest: String,            // of the text as typed
    pub audit_body: Option<Redacted<String>>,
    pub admitted: Admitted,
}

/// Captured when the bridge accepts an action, before any queue.
pub struct Admitted { pub pause_epoch: u64, pub attempt: u64 }

// Pure, like decide_deliver. Add, Start and Close have no target state to check.
pub const fn decide_agent(action: AgentAction, state: &TargetState) -> Decision;
```

The order in `agent_submit` is: paused now, pause epoch equal to the admitted one, target attempt equal to the admitted one, `decide_agent`, durable audit intent. There is no call to `Limits`, so the limits of 6 hops, 4 messages a minute, duplicates and one open delivery do not apply to agents (R3, R7, A16). User Sends keep them. For `Type`, `Keys` and `Wake`, `decide_agent` refuses a target that is not running, and a target in manual mode. `Add`, `Start` and `Close` are process control, so they pass: section 6.2 allows them on a bridge-off target, and `close` on a stopped terminal succeeds. Readiness and busy are not consulted (R3).

- `still_valid` becomes generic over the action. An agent `Authorized` carries the admitted epoch, not the epoch at dequeue.
- `agent_note` records a read: actor `agent`, source, target, action `read`, no body (A33). It never waits, and is allowed under Pause.
- Agent text is `sanitize`d, never redacted. `audit_body` holds `redact()` of the same text (A47). If redaction fails, the audit keeps the hash only.
- `POLICY_REVISION` goes from 3 to 4. New ADR 0015 supersedes ADR 0006, and the README line that nothing moves unless you click Send changes. A21 holds because no operation touches Pause or Stop all.

### 5.7 Agent typing in `SessionActor`

A new bounded channel of depth 16 carries `AgentCommand`:

```rust
enum AgentCommand {
    Type { id: String, source: String, text: String, admitted: Admitted, reply: Sender<WriteOutcome> },
    Keys { id: String, source: String, keys: Vec<AgentKey>, admitted: Admitted, reply: Sender<WriteOutcome> },
    Wake { id: String, text: String, admitted: Admitted, reply: Sender<WriteOutcome> },
}
```

**Admission.** Bridge state reads the pause epoch, then `is_paused`, then the target's attempt, before anything is queued. It refuses at once if paused. At dequeue the Gate refuses a command whose epoch or attempt has moved, so nothing queued before a Pause runs after Resume, and nothing queued before a restart lands in the new conversation. Those give `paused` and `restarted`, with nothing typed. Bridge state allocates the request id at admission, and drops the request if nothing was pasted and the id was never given out. The same admission applies to `add`, `start` and `close`, through the registry.

This needs one change to `PauseSwitch::pause`: set `paused` first, then bump the epoch. In today's order, a reader can see the new epoch with `paused` still false, and hold a valid-looking admission through a later Resume. With `paused` first, a reader that sees `paused` false took the epoch before the bump, so the mismatch is caught.

**Typing.**

- The actor holds one `typing` slot beside the existing `open`. It takes a command only when `typing` is empty and no manual paste is waiting for its Enter. While `typing` is set it does not take `Command::Send`, so two pastes can never share one Enter. That wait is at most 400 ms.
- For `Type`: refresh the snapshot, call `agent_submit`, paste with `encode_paste`, wait the settle time, check `still_valid` and the attempt, press Enter, reply `Typed`.
- The settle time is `PASTE_SETTLE` (400 ms) plus 2 ms per KiB, about 0.9 s at the frame limit. 400 ms was measured on bodies of at most 4096 bytes, and A16 removes that limit. Check 1 confirms a large paste arrives whole.
- Pause between paste and Enter means no Enter, and the reply is `paused_before_enter`: the text may be sitting in the input box. This matches the manual path. The request survives this (section 5.12).
- `Wake` first checks that the phase is not `Working`. If it is, it replies `Busy` and types nothing, so the bridge puts the notice in the inbox.
- A write does not wait for an acknowledgement. An open manual delivery does not block agent text. If agent text arrives during one, the existing rule may end that delivery `uncertain`, which is the honest outcome.
- `DeliveryState` gains `Typed`, a final state, so ledger rows for agent text show who typed what and do not offer **End delivery**.

Writes to one terminal go in arrival order, one at a time (A30). This short transit queue is not holding: nothing waits for the target to be idle. Short text drains at about 2.4 items a second, so the longest wait is about 7 s, or about 15 s if all 16 are at full size.

### 5.8 Turn tracker

`ToolEvent` changes to:

```rust
Started { tool_session: EventText, turn_log: Option<EventText> },
PromptSubmitted { prompt: EventText, turn: Option<EventText> },
TurnEnded { input: Vec<EventText>, reply: Reply, lifecycle_known: bool,
            background: Vec<EventText>, thread: Option<EventText>, turn: Option<EventText> },
NeedsUser,

enum Reply { Text(EventText), None, Dropped }   // Dropped: there was a reply, over the limit
```

The tracker keeps open prompts in order.

| Input | Effect |
| - | - |
| `PromptSubmitted` | Add an open prompt with any request ids in the text. If the phase was not `Working`, set it and emit `began`. Clear needs-user. Turn log tail on |
| `TurnEnded`, one prompt open | It closes, and its requests take this reply. Then the two "nothing left open" rows apply. Turn identity below limits this on Codex |
| `TurnEnded`, several prompts open | Evidence decides which close. See Pairing |
| Nothing left open and no background work | Phase `Idle`, emit `finished`, tail off. Waiting turn records end |
| Nothing left open, background work running | Phase `Background`, no `finished`. See Background ownership |
| `TurnEnded`, `lifecycle_known: false` | Nothing is known about which turn ended or what still runs. Open prompts' requests end `unavailable`. No `finished`, and the phase goes by the next event |
| Interrupt marker in the turn log | The interrupted prompts' requests end `interrupted`. On Codex that is the prompt with the record's `turn_id`. On Claude it is the open prompts whose `user` record the tail has already passed, because the log is in order and a newer prompt comes after the marker. If none stay open: phase `Idle`, emit `interrupted` |
| Session ended | All requests end `exited`, emit `exited` with the status text. The status (exit code or signal) and the last non-empty screen line are kept for the attempt and shown in `list` (`exit`), in the `exited` notice body and in the `result` reason of each request it ended; the line is the tool's own text and is quoted |
| Hook gap, or `HookHandle::rejected` rises during a turn | A frame was lost. Open prompts' requests end `unavailable`. The phase goes by the next event |

**Pairing several open prompts.** Text typed at a busy target can leave several prompts open. Time alone proves nothing, because a separate queued prompt can sit silent for minutes on a slow tool or model call. So there is no timeout, and a reply is given only on evidence.

- **Codex.** Exact, by turn identity (below). If the id is missing, a request id found in the turn's `input-messages` also proves it.
- **Claude.** The tail catches up with the transcript at `Stop`. A prompt belongs to this turn if its `user` record sits after the previous `end_turn` assistant record and before the last one. A later record means a separate turn, which stays open. This covers a measured case: a typed prompt landed 0.7 s before a self-started turn's `Stop`, which would otherwise look like the end of the typed turn.
- **A prompt older than the previous `end_turn`** had its own turn, and its end was never reported. Its requests end `unavailable`.
- **No evidence** (no record found, or the log is unreadable): every prompt then open is marked `ambiguous`, and the end closes the oldest of them. Both tools run separate prompts in order, so if they were separate the oldest is the one that ended, and the rest stay open for their own ends. If they were merged, the rest stay open as described below. Requests of `ambiguous` prompts end `unmatched` when their prompt closes, or on interrupt or exit, never `ok`. Being `ambiguous` lasts, so a later end that finds one prompt open still gives `unmatched`.

`finished` needs all prompts closed. If a tool merged prompts and no evidence shows it, a prompt can stay open until the next end, interrupt or exit. The terminal then reads `working`, which is the honest answer when Connecto cannot tell. Check 1 shows how often this can happen.

**Turn identity on Codex.** A turn is its `turn_id`. All four sources carry it: `UserPromptSubmit.turn_id`, `notify`'s `turn-id`, and `turn_id` in the rollout's `task_complete` and `turn_aborted` records, which was seen on every such record in the rollouts measured. The rules apply to any end, whichever source reports it:

- The first end for a `turn_id` wins. The tracker keeps the last 64 ended ids per attempt. A later end for one of them changes nothing: no prompt closes, no `finished` is emitted and no result changes. The one exception is that a `notify` still updates the `input-messages` count.
- An end whose `turn_id` matches an open prompt closes only that prompt, even if it is the only one open. So a `notify` arriving late, during the next turn, cannot close that newer prompt. The one-prompt rule above applies to Codex only when the prompt has no `turn_id`.
- Closing a prompt records both its own `turn_id` and the end's as ended.
- `task_complete` starts a 5 s wait for that `turn_id`, and a `notify` for it cancels the wait. At expiry the record is the turn's end, closing only that prompt. Its reply is the record's `last_agent_message` when the line was read whole, otherwise `Reply::Dropped`.
- The reverse order is covered too: after a `notify`, the tail may read that `task_complete` later, and it is an end for an ended id.
- An end on the pinned thread with an unseen `turn_id` follows the rule below.
- `connecto-incomplete` has no identity. Each fallback end adds one to `late_expected`, a real `notify` for that id takes one away, and an incomplete frame that finds `late_expected` above zero takes one away and is ignored. Only otherwise does it act on open prompts.

**If the ids do not match (not yet run).** Both are described as the turn's id, but nothing confirms that the hook's `turn_id` equals `notify`'s `turn-id`. A terminal must not stay `working` if they differ, so an end never waits on a match. When an end on the pinned thread has an unseen `turn_id`:

1. If nothing is open, the prompt hook was lost. It emits `finished`, and closes nothing.
2. If this turn's new `input-messages` entries hold the id of a request on an open prompt, that prompt closes, and its requests take this reply as `ok`. Those entries are the turn's own inputs, so this is evidence that needs no turn id.
3. Otherwise the no-evidence rule applies: the oldest prompt closes, and every prompt open is `ambiguous`. A prompt with no requests loses nothing by this.

An interrupt marker with an unseen `turn_id` closes the oldest open prompt.

So with mismatched ids, requests still get `ok` through rule 2, and a turn always ends. The cost is that a late `notify` can no longer be told from a new end by id. It closes the next prompt as `ambiguous`, which is `unmatched`, not a wrong reply.

Check 1 settles which case holds, by comparing three ids from one turn. The result is a constant in the Codex adapter for 0.166.0.

| Check 1 result | Effect |
| - | - |
| Hook `turn_id` equals `notify`'s | Exact pairing. The rules above rarely reach the unseen-id path |
| Hook `turn_id` differs from `notify`'s | Pairing by rule 2 and by order. Requests still work |
| `notify`'s equals `task_complete`'s | The 5 s fallback and its deduplication stand |
| `notify`'s differs from `task_complete`'s | The fallback is switched off, as a late `notify` could not be recognised. A turn with no `notify` then ends at the next end, by the no-evidence rule |

**Background ownership (Claude).** Measured on 2.1.318: `Stop` can fire with work still running, and Claude then starts a turn by itself and ends it with a second `Stop` whose `background_tasks` is empty. Tasks have ids, so ownership is tracked by task, not guessed.

- At a `Stop` that closes a turn, each task id not seen at earlier `Stop`s is owned by that turn. Task ids gone from the list are forgotten. A turn record with requests and running tasks waits.
- A self-started prompt continues one turn only if ownership is clear: it names a task owned by that turn, or it names none and every running task belongs to that one turn. Its reply then replaces that turn's latest.
- Otherwise every waiting turn it might belong to is marked `ambiguous`.
- Any other prompt is its own turn, and its reply never reaches a waiting request.
- Waiting records end at the `Stop` with an empty list, together with `finished` (L10). Their requests take `ok` with the latest reply, or `unmatched` if `ambiguous`.

So two requests that each left work running cannot overwrite each other's result. Check 8 shows whether notifications name their task. If they do not, overlap gives `unmatched`, and a single waiting turn still works.

`Background` counts as not busy for typing and wakes, but `finished` waits, as R19 says. The design adds no timeout for long background work. It makes the state visible instead: `list` shows `background`, and `result` says `pending`, with the reason that the target has background work running and that `read` is available.

**Claude specifics.** `Stop` fills `background` from the ids in `background_tasks`. `Notification` becomes `NeedsUser` under the filter in section 3. Without that filter an idle Claude would wake its watchers. `transcript_path` comes from hook payloads, and is used only if it sits under the terminal's profile. The interrupt marker was measured only for a turn interrupted during a tool call. An interrupted Claude turn fires no hook at all: no `Stop`, `StopFailure` or `Notification` in 8 s.

**Codex specifics.**

- `notify` stays the turn end. Launch adds hooks for `SessionStart` and `UserPromptSubmit` only. `Stop` and `Interrupt` hooks are not added, which keeps their unproven interplay with `notify` small.
- **Main thread pin.** Several Codex terminals on one profile mean the writer lock no longer identifies a terminal's own thread. So every `SessionStart` updates a pinned thread id, and the docs say subagent hooks use the parent's `session_id`. A `notify` whose `thread-id` differs from the pin is not a turn end. This matters because a child thread's `notify` was measured firing 1.5 s before its parent's, and the first `notify` of a session can be the hidden title thread. If Check 3 shows the id is unusable, the main turn's end is taken from the rollout, because a child has its own rollout file: a `notify` counts only if its `turn-id` matches a `task_complete` in the hook's `transcript_path`. If Check 1 also shows those ids differ, a child's `notify` cannot be told apart: see the risk in section 9.4.
- `input` keeps `input-messages`, so ids typed during a turn can still be found at its end. The list accumulates over the thread, so only entries past the count seen at the previous end belong to this turn. An older id proves nothing. Positions are absolute, and the tracker always records the original count, so a shrunk frame does not shift them (section 5.9). The count resets when the pin changes, and a child's `notify` never touches it.
- With the pin, the thread comparison replaces the `has_rollout` walk, which silently drops a turn end when it runs out of its 50,000-entry budget. The walk stays only for a terminal with no hooks.
- The turn log is the hook's `transcript_path`, and the interrupt marker is a `turn_aborted` event record (Check 4). The docs warn that this format is not a stable interface. Versions are pinned, so that is acceptable here.
- Codex readiness stays screen-based. Manual Send benefits from the change, as `working` now reflects Codex turns.

**Adapter methods.** `turn_log(&self, reported: &Path, ctx: &HookCtx) -> Option<PathBuf>`, `read_log(&self, line: &str) -> LogMark`, and `continuation(&self, prompt: &str) -> Option<Continuation>`. `LogMark` is one of `Interrupt`, `TurnComplete`, `Prompt`, `FinalReply` or `Other`.

**Turn log tail.** It reads at most 256 KiB every 500 ms while a turn is open, and holds a partial last line for the next read. At `Stop` it catches up, up to 4 MiB. A line over 1 MiB is classified from its first 4 KiB and its text is skipped: the rollouts measured put `type` and `turn_id` before `last_agent_message`, so an end or interrupt is still seen. Hitting either bound sets a log gap, which counts as no evidence for that turn, so a cut log can never look like an answer.

### 5.9 Hook frames and reply size

**Frames over the limit.** Today the helper drops any frame over 256 KiB, so the event is lost. Lifecycle facts must survive even when text does not, so the helper shrinks the payload instead. It reads the whole payload, up to 32 MiB, and drains the rest. If the frame fits, it is sent unchanged. If not, it parses the JSON and removes text in this order until it fits, naming what it dropped in `connecto_dropped`:

| Event | Removed first | Removed last | Always kept |
| - | - | - | - |
| `Stop` | | `last_assistant_message` | `background_tasks`, `transcript_path`, `session_id` |
| `notify` | Old `input-messages`, keeping the last 16, each cut to 4 KiB | `last-assistant-message` | `type`, `thread-id`, `turn-id` |
| `UserPromptSubmit` | `prompt` cut to its first 64 KiB, so the header and id survive | | `turn_id`, `session_id` |

When `notify` loses old entries, the helper adds `connecto_input_total`, the original list length. Retained entry `i` of `n` then has absolute position `total - n + i`, and the tracker records `total` as the count seen. If more than 16 entries are new, the older of them are gone, so ids among them are simply not found and evidence is missing, which fails towards `unmatched`.

A shrunk frame takes the normal path, so a child's `notify` is still ignored, and a `Stop` with background work still gives `Background`. If the reply itself was removed, the event carries `Reply::Dropped` and that turn's requests end `unavailable`. Removing old `input-messages` first means an accumulated list does not cost a small reply.

**Lifecycle unknown.** If the payload is over 32 MiB or is not JSON, nothing can be recovered. The helper knows the event from its arguments, because each hook registers its own command, such as `connecto-hook claude --event Stop`. It sends `{"hook_event_name":"Stop","connecto_incomplete":true}`, or `{"type":"connecto-incomplete"}` in `notify` mode. That maps to `lifecycle_known: false`.

**When no frame arrives at all:**

- **Codex:** `task_complete` in the rollout with no `notify` 5 s later is a real end of the main turn, because that rollout is the pinned thread's own file. Turn identity in section 5.8 gives the rules. This also covers a reply too large for `notify`'s argument. In the rollouts measured, 20 `task_started` records matched 18 `task_complete` plus 2 `turn_aborted`.
- **Any tool:** a refused frame raises `rejected`, and requests end `unavailable`, with no `finished`.
- **Claude:** if the `Stop` hook never ran, the next end finds the prompt older than the previous `end_turn`, and its requests end `unavailable`.

What stays open is a Claude turn whose hook never ran, with no later turn. Nothing can be observed there, so the request stays `pending`, and `read` still works.

**Reply size.** `EventText::new` cuts at 64 KiB, which would return a long reply as `ok` with its end missing. `EventText` gains a second constructor with a cap of 256 KiB, the frame limit, and turn-end replies use it. So every reply that fits a frame is held whole.

- The tracker treats a `truncated()` reply as `Reply::Dropped`, so a cut reply can never be `ok`. `Reply::None`, a turn that reported no text, also ends `unavailable`, so `ok` always has a reply.
- `EventQueue` bounds queued reply bytes at 8 MiB, because the 64 KiB cap existed to bound memory. Over that, the oldest queued reply becomes `Reply::Dropped`, keeping its lifecycle facts.
- `Capture` for the relay panel is cut back to 64 KiB, because `RelayView` is cloned on every update. That is display only.
- Prompts and `input` keep the 64 KiB cap. The header comes first, so the id survives. An id cut off gives `unmatched`.

### 5.10 Codex hook trust

Codex refuses to run a hook until it is trusted, and without trust it raises a full-screen review modal that swallows typed input. Passing `-c 'hooks.state={"<key>"={trusted_hash="<hash>"},...}'` on the launch line avoids the modal. That was measured on the bypass flags, with `SessionStart`, `UserPromptSubmit` and `Stop` all firing. The hash changes with the hook's definition, so it must be read per path and version.

During preflight in the pane worker, after the version check:

1. Build the hook definitions for this helper path.
2. Take the single trust lock, and look in a memory cache keyed by Codex version, hook command and profile path.
3. On a miss, run `codex app-server` over stdio with the same `-c hooks.*` values and the terminal's `CODEX_HOME`. Send `initialize` then `hooks/list`, with a 5 s limit.
4. Check every expected hook is listed, build `-c hooks.state={...}` from key and `currentHash`, and cache it. A failure is cached too, for the life of the app.

**The read never touches a live profile.** All Codex terminals of a project share one profile, and none can launch before its preflight has an answer. So the first read happens with no Codex terminal live, and everything after it hits the cache, success or failure. A login that rotates the generation changes the profile path, and login needs every terminal stopped (section 5.4). Caching failures is what keeps this true: a terminal launched without hooks is live, and a retry would read beside it. So a failed read is not retried until the app restarts.

On any failure, launch with `notify` only and mark that terminal bridge off, with the reason. **Never launch with `hooks.*` and no valid `hooks.state`.**

### 5.11 Bridge server

- **Socket.** `/private/tmp/mg-<uid>/bridge-<app pid>.sock`, in the checked private folder. Bound in `Relay::start`, removed at shutdown. One per app run.
- **Caller.** `LOCAL_PEERPID`, then a parent walk (depth 64) against the published root pids of live terminals. The first match is the caller, as an id and attempt read from one `LiveSession`, which holds both, so the pair cannot be mixed by a restart. No match gives `not_a_terminal`. The client stays alive awaiting its reply, so the walk cannot race its exit.
- **Handlers.** One short-lived thread per connection, at most 32 at once. Above that the connection gets `busy`. Request read limit 1 s, reply write limit 5 s.
- **Routing.** `list`, `result`, `watch`, `unwatch` and `drain` go to bridge state. `send`, `keys`, `add`, `start` and `close` go through bridge state to an actor or the registry. `read` takes the terminal's `LiveSession` and streams, without touching bridge state beyond the audit note.
- **Events, two routes.** What must not be lost and what may be lost travel apart.
  - **Outbox, lossless.** Each slot has a small mutex-guarded outbox, appended to by its actor and emptied by bridge state on every tick and on every channel wake-up. It carries what decides a final state or a wake's life: typing outcomes, request binds, request ends with their replies, wake outcomes, and `exited`. Entries keep the actor's order. The outbox is bounded by what it carries: at most 16 typing outcomes and one wake in flight, ends of open requests (bounded by the results cap, L9), and one `exited`. So nothing in it is ever dropped, and nothing that decides a request depends on a later message.
  - **Channel, lossy.** A bounded channel (256 events, 16 MiB) with `try_send` carries `began`, `finished`, `interrupted`, `needs-user` and `LeftWorking`. The actor never waits on it.
  - **Both** carry slot, attempt and `project_seq`. Each refused channel send raises a per-slot lost counter, an atomic beside the phase.
  - **The lost counter is read on the tick,** every 250 ms, so a drop is found with no later message. On a rise, bridge state adds the "events were lost" line to the inbox of every terminal that watches that one. If the published phase is not `Working`, it also runs the unread check as if `LeftWorking` had arrived. Requests are not touched: their ends travel in the outbox.

### 5.12 Bridge state

All of it is in memory, and all of it is lost on restart (A29, A31, A42).

```rust
struct Watch { watcher: Attempted, target: Attempted }       // id plus attempt

struct Request {
    id: String, project: String,
    from: String, from_attempt: u64, target: String, attempt: u64,
    ask: bool, clock_from: Instant, id_given_out: bool, note: Option<&'static str>,
    state: RequestState,  // Pending, Ok, Interrupted, Exited, Unmatched, Unavailable
    reply: Option<EventText>,
}

struct Notice {
    source: String,
    text: String,   // generated, at most 120 bytes
    wanted: bool,   // a watched event or an --ask notice, not yet the subject of a wake
}

struct Inbox { entries: VecDeque<Notice>, merged: u32, lost: u32, first_drain_done: bool, wake: Wake }

enum Wake { None, InFlight { id: String, stage: WakeStage, deadline: Instant } }
enum WakeStage { Queued, Typing, AwaitingPrompt }

struct Reservation { id: u64, owner: Attempted, entries: Vec<Notice>, merged: u32, lost: u32, first: bool }
```

**Results cap.** Up to 64 MiB of replies are held. Past that the oldest reply goes and its request says `unavailable` (A42).

**Typed text that starts no turn.** Bridge state checks unbound requests on its tick. `clock_from` is the latest of: Enter, the moment typing failed after a paste, the `timeout` reply, and the target's last `LeftWorking`. The clock runs only while the phase is not `Working`. The clock does not start until the target is ready (its start hook has arrived in this attempt) or has been seen `Working`: a tool that takes longer than 15 s to load its servers keeps the pasted text in its input and still answers. A target that never becomes ready ends the request `unmatched` after 180 s (`BOOT_CAP`). Once ready, after 15 s the request ends `unmatched`, and an `--ask` sender gets its notice. So a not-busy target whose menu swallowed the text (L5) never needs to stop being busy: the clock has been running since Enter. A target that is `Working` when the text lands is judged when it stops. This also covers a screen with no hooks firing, such as a first-run dialog. A prompt that arrives later binds nothing, since the result is final (a later-starting turn is not bound after `unmatched`; the boot gate is what keeps a slow start from getting there). A request whose id was seen is not on this clock, and ends by the tracker's rules.

**Sends that may have reached the target.** The line is the paste. Bridge state allocates the id at admission, and what follows depends on how far the send got.

| Outcome | Record | Client gets |
| - | - | - |
| Refused before any paste, with no id given out | Removed | A refusal with no id |
| Typed: Enter was written | Kept | `r12 typed` |
| Pasted, no Enter (`paused_before_enter`, or the target exited) | Kept, with a note | A refusal with the id |
| `timeout`: outcome unknown | Kept | A refusal with the id |
| Late outcome after a `timeout`, nothing typed | Ends `unmatched`, with the note "was not typed", because the id is out | Nothing more |

A kept request is an ordinary unbound request. If its header is submitted within the clock, for example when the user presses Enter on the leftover text, it binds as usual and the reply is its real answer. That is not queueing for Resume, as Connecto types nothing further. After the clock it ends `unmatched`, with the reason that the text was not entered and may still be in the input box. A result is final, so a later submission binds nothing. If the target exited, the request ends `exited`. `keys` has no request, so only its id is reported.

**Sender attempt.** An `--ask` notice belongs to the conversation that asked, and `from_attempt` records it. A restart is a new conversation (A12), so it gets no wake and no notice for its predecessor's request. If that attempt has ended when the request ends, the notice is dropped. The result stays readable by any agent in the project.

**When an attempt ends.** Its inbox goes, unread and wanted notices included, together with its `merged` and `lost` counts, its watches, its wake and any reservation. No wake is sent. The next attempt starts with an empty inbox and a first drain that carries the roster. This follows A29: a dead conversation has no one to tell.

**Changing project.** `choose_project` needs nothing live, so every attempt has already ended, every inbox and watch has gone by the rule above, and every target's requests have ended. Only ended requests remain, and they are partitioned rather than cleared:

- Each request records its project id. `result` looks only within the current project, and another project's id gives `unknown_request`, so it reveals nothing (A2, A43).
- Returning to a project in the same run finds its results again, so A42's "life of the app" holds. The 64 MiB cap is shared, oldest first.
- The request and key id counters are per app run and never reset, so `r12` can't recur.
- Default terminal ids and attempt numbers do recur, because new panes count from zero. So the registry bumps a `project_seq` on every change, every event to bridge state carries it, and a late event from an old actor is ignored.

**Every event.** For each event from target T, bridge state goes through every other live terminal W:

1. If W watches T, W is not busy, Pause is off, and no wake is in flight for W, send `AgentCommand::Wake`. This applies to all five events (R5).
2. Otherwise the event goes to W's inbox. That covers non-watchers, busy watchers, and every watcher under Pause. The notice is wanted if W watches T.
3. A wake that was not typed puts its notice in the inbox as wanted. That covers `Busy`, refused, stale, and no outcome within 30 s. It is not retried at once, and may be tried again the next time W stops being busy (A29).

**Requests.** A request that ends follows the same path for its sender when `ask` was set, and appears in no one else's inbox. In the sender's inbox it is wanted. `--ask` wakes on the end of a request only. It does not make the sender a watcher of the terminal, so it never wakes on `began`. A plain `send` stays quiet, so a one-way message does not cost the sender a turn.

**Unread-notice wake.** On `LeftWorking` from W, if W's inbox holds a wanted notice, Pause is off and no wake is in flight, bridge state does two things:

1. It clears the wanted flag on every notice in the inbox, and records their ids in the wake.
2. It sends one `AgentCommand::Wake` with the fixed "Notices arrived" text.

That wake's `UserPromptSubmit` drains the whole inbox as context, so one wake covers everything unread at that moment. A non-watcher's notices are never wanted, so they can only ride along on a wake W has for another reason.

- **"Stops being busy"** is the same test as before typing a wake: the phase is no longer `Working`. Both `Idle` and `Background` count, and so does an interrupt. If the user presses Esc on a watcher holding wanted notices, a wake follows.
- **It cannot loop by itself (L11).** The wanted flag is cleared when the wake is attempted. It comes back only if the wake was never typed, and then only for a later stop. A typed wake never gives it back, even if the hook never prints, so only a new event can set it again.
- **One attempt per stop (L12).** Each `LeftWorking` allows at most one unread-notice wake, and nothing retries it at once. The same rule as rule 3 applies to its outcome:
  - **Not typed** (`Busy`, refused, stale, or no outcome within 30 s): the wanted flag goes back on the recorded notices that are still unread, in the inbox or in a drain reservation. The wake was never delivered, so the next `LeftWorking` may try again. After `Busy`, W's new prompt has usually drained them already, and then there is nothing to restore.
  - **Typed** (`AwaitingPrompt` ended by a prompt or by the deadline): nothing goes back. A wake swallowed by a menu looks delivered to Connecto, so its notices wait for W's next prompt, which may not come.
  - **Under Pause** no wake is attempted. The notices stay wanted, and W's next stop after Pause can try.
  - A new `LeftWorking` needs W to have worked again, so a failed wake can never repeat while W sits idle.
- **`unwatch`** clears the wanted flag on that terminal's notices, and they stay as context.
- **Merging** keeps the flag if any merged entry had it.

**How long a wake is in flight.** A wake can wait up to about 15 s in the typing queue, so "in flight" follows the wake's stages, not a fixed window. "No wake in flight" means `Wake::None`.

| Stage | Left by | Goes to |
| - | - | - |
| `Queued`, entered when the actor's queue accepts it | The actor takes it | `Typing` |
| `Queued`, `Typing` | Outcome `Typed` | `AwaitingPrompt`, deadline 3 s after Enter |
| `Queued`, `Typing` | Outcome `Busy`, refused or stale | `None`. Rule 3 applies |
| `Queued`, `Typing` | No outcome 30 s after queuing, so the actor is stuck or the outcome was lost | `None`. Rule 3 applies. A late outcome for that wake id is ignored |
| `AwaitingPrompt` | W's next prompt | `None` |
| `AwaitingPrompt` | The deadline passes | `None`. The wake was typed and swallowed, so nothing goes back (L12) |
| Any | W's attempt ends | Gone |

W's prompt during `Queued` or `Typing` changes nothing, because the outcome will say `Busy`, or that the wake was typed. Events that would wake W go to its inbox as wanted at any stage, and W's own wake turn then drains them. So one burst gives one wake, however long the queue. W needs no other wake in that time, as one is already on its way, so this keeps to R5.

**Watch lifetime.** A watch ends on `unwatch`, or when either terminal's attempt ends. The watcher gets `exited` first. Watches are not saved, so saved terminals return after a restart without them (A31).

**Inbox.** Up to 32 entries. When full, the oldest entries from one source merge into one, and the `merged` count is stated. A full inbox plus the roster fits in one drain of about 6,000 bytes, which stays under Claude's 10,000-character threshold and Codex's context size of about 2,500 tokens. A drain renders the whole inbox, newest first, so one wake's prompt always reaches what was unread. Section 6.7 fixes the rendered form.

**Drain reservation.** Bridge state is one thread, but handler threads do the writing, so a drain moves its notices out of the inbox instead of marking them:

1. On `drain`, bridge state takes every entry, the `merged` and `lost` counts and the first-drain flag out of the inbox into a `Reservation`, and renders them.
2. The handler writes the reply, then sends `DrainDone { reservation, written }`. A drop guard sends `written: false` on every other way out.
3. `written: true` discards the reservation. Only what it holds goes.
4. `written: false` puts the entries back at the front in their order, and restores the counts and the flag. The inbox bound then applies as usual.

Two drains cannot return the same notice, as the second finds only what arrived since. A notice that arrives during a write lands in the inbox, and is untouched by that write's success. A reserved notice is not unread for the unread-notice wake. A release puts entries back with whatever wanted flag they then hold, and, like a failed wake, waits for the next `LeftWorking`. Nothing is attempted at the moment of release. A `DrainDone` for an ended attempt is ignored.

**When a drain commits.** It commits when the reply has been written to the socket. If that write fails, the reservation is released, and the notices wait for the next prompt. If the helper dies after that point, those notices are lost. This is deliberate, and it is what A29 asks for: delivery at most once. A lost drain loses only a pointer: `list` and `result` still hold the facts, and a result is never consumed by a drain.

#### Shared state and timers

Rules that hold across the bridge:

- **Bridge state never blocks.** It sends with `try_send` only, replies go straight to handlers, and it never waits on an actor, the registry or the audit log. Nothing waits on it in return, except handlers with their own deadlines. So no cycle can form.
- **Bridge state decides a request's final state, and the first one wins.** The actor reports binds and ends through its outbox, and bridge state ignores any report for a final request. So `unmatched` at 15 s and a late bind cannot disagree.
- **The actor binds only ids it typed itself.** It keeps those ids for the attempt.
- **Phase is published, not derived.** Each actor stores its phase in an atomic for its slot, and `list` and the wake rules read that. A lost event therefore cannot leave a terminal `Working` for ever in bridge state. The tick reads the lost counters and handles a rise as described under Events in section 5.11.
- **Decide on the object, not a stage.** `read` takes the `LiveSession` and refuses one with no hook socket. A login session has none, so a login starting at that moment cannot slip through a stage check.
- **Stale work is keyed.** Attempt, `project_seq`, wake id and reservation id all let a late message be ignored.
- **Shutdown.** `shut_down` sets a flag on the registry before it stops anything, so exits seen during shutdown do not save `desired: stopped`. A handler waiting on an ended actor sees its reply channel close, and answers `not_running`. The connection count is held by a drop guard.

| Timer | Owner | Starts | Ends early on | On expiry |
| - | - | - | - | - |
| Paste settle, 400 ms plus size | Actor | Paste written | Attempt change | Check validity, press Enter |
| Write queue | Actor | Queued | Admission stale at dequeue | No timer. It drains at the settle rate |
| Handler wait, 30 s | Handler | Request sent on | Reply | `timeout`, id kept |
| Wake in flight, 30 s and 3 s | Bridge state | See the stage table | Outcome, prompt, attempt end | `None`. A wake never typed gives its notices back for the next stop |
| Unmatched clock, 15 s | Bridge state | `clock_from`, restarted when the target is first ready | Bind, any final state. Held while the target is not ready (cap 180 s) and paused while `Working` | `unmatched` |
| `task_complete` wait, 5 s | Tracker | Record read | `notify` for that id, attempt end | The record is the end |
| Turn log tail, 500 ms | Tracker | First open prompt | Nothing open | Reads on |
| Self-close, 250 ms | Registry | Reply written | Attempt no longer the admitted one | Stop, only if the attempt still matches |
| Helper drain, 2 s | Helper | Connect | Reply | Prints nothing. If the host had written, those notices are lost (A29) |
| Trust read, 5 s | Pane worker | Spawn | Answer | Kill the child, cache the failure |

Bridge state ticks every 250 ms, which is the resolution of its timers.

**Loops between agents.** The design adds no limit and no duplicate check (R7). A wake in flight only merges a burst for one watcher. What the design gives: Pause stops a loop, `unwatch` ends it, `list` marks watches so an agent can see one, ledger rows and the tab activity mark show it to the user, and the launch text says each wake is a turn and points to `--ask` for a single answer. See L11.

### 5.13 Scrollback export

`Emulator::text_all()` takes the screen lock once and walks history and the visible grid into a `String`. It joins wrapped rows, skips wide-character spacers, and reports whether history has reached its 10,000-line limit. It runs on a handler thread. At 10,000 lines it holds the PTY reader for a few milliseconds, and an atomic view is worth that. This has not been measured.

The output is the terminal's screen history, not the tool's transcript files. Check 7 shows how much of it is real history rather than repaints.

### 5.14 Helper refresh (R20)

Today the helper reaches the store only through `provision`, and a plain `provision` also copies the newest Claude (now 2.1.319), which `Store::newest` then picks, turning the bridge off.

So at the start of `managed_panes`, `Store::refresh_hook(source)` compares the digest of `connecto-hook` beside the running `connecto-app` with the stored one, and calls `install_hook` if they differ. It then ensures `cli/bin/connecto` is a relative link to `connecto-hook`. It never reads or changes the stored tools. If no helper sits beside the app, the stored one stays. `install_hook` is atomic, so running hooks are safe.

**The helper keeps every older form working.** `connecto-hook claude` with no `--event` and `connecto-hook codex <json>` forward as they do today and print nothing. Shrinking still applies, and adds only fields an older parser ignores. Without `--event` the helper cannot name the event, so a payload it can't parse is dropped, as it is today. The fake CLI scripts in `crates/connecto-session/tests/relay.rs` call `"$H" claude`, so that suite is the standing proof.

### 5.15 What a terminal gets at launch

`LaunchCtx` gains `bridge: BridgeLaunch`, holding the socket, terminal id, helper folder, and launch text with the Codex hook values. A bridge-off terminal has no launch text, no Codex hook values and no `--context`. It still gets the socket and `PATH`, so the command can say why.

| Value | Claude | Codex |
| - | - | - |
| `PATH` | `<data>/cli/bin` first | Same |
| `CONNECTO_BRIDGE_SOCK` | Socket path | Same |
| `CONNECTO_TERMINAL` | Own id, for the agent's reading only. Never trusted | Same |
| Launch text (section 6.8) | `--append-system-prompt` | `-c developer_instructions=...` |
| Hooks | One command per event, `connecto-hook claude --event <name>`, with `--context` on `UserPromptSubmit` | `-c hooks...` with `connecto-hook codex-hook --event <name>`, `--context` on `UserPromptSubmit`, plus `-c hooks.state=...` |

`cli/bin` holds no model CLI, so the private copies stay off `PATH`. `connecto-hook codex-hook` is a new mode that reads stdin. `connecto-hook codex` keeps reading `notify`'s argument.

### 5.16 Failure handling

| Failure | Handling |
| - | - |
| Bridge socket can't bind | App runs. Every terminal is bridge off, with a visible warning. Manual Send works under its usual rules |
| Helper can't reach the socket during a hook | Prints nothing, exits 0. The prompt continues. Bounded at 2 s |
| Unverified tool version | That terminal is bridge off and says so. The user can still type in it and Send to verified terminals. Send to it stays denied (A32) |
| Hook hash can't be read, on a verified version | That terminal is bridge off, says so, and runs without hooks. It is not in manual mode, so the user can type in it and Send to it under today's rules (A32). The failure is cached, and not retried until the app restarts |
| Stale helper | Cannot happen after a refresh. Otherwise `bad_version` from the wire version |
| Target exits during paste | `not_running` or `timeout`, with the id. Request `exited` |
| Helper dies after a drain was written | Those notices are lost (A29). `list` and `result` hold the facts |
| Audit stalls | `audit_failed`, nothing typed |
| Session actor stuck | The caller gets `timeout`. Other terminals are unaffected |
| Event channel full | A notification is lost. It is counted, found on the next tick, and reported to the watchers. Request ends, wake outcomes and `exited` use the lossless outbox, so a pending request or a wake never waits on a lost message |
| Unread-notice wake fails, or Pause is on | One attempt per stop, nothing queued. A wake never typed gives its notices back for the next stop. A typed wake that was swallowed leaves them for the next prompt, which may not come (L12) |
| `terminals.json` unreadable | Default two, warning, bad file kept. If the file can't be read or renamed: read-only list mode (section 5.4) |
| Saving the list fails | Add and remove are refused with `save_failed`, and nothing changes. Starts and stops happen anyway, the list is marked unsaved, retries run every 30 s, and a warning shows until a save succeeds |
| App crash or `kill -9` | Sockets stay in the folder and are replaced on bind. The list keeps its last state, so running terminals restart |
| Stopped terminal | Its processes are killed on Stop, and any straggler has lost its parent, so `not_a_terminal` applies. Its screen stays readable until restart, because `pane.session()` is kept. After an app relaunch there is no buffer, which gives `no_content` |
| Login pane | `read` is refused, as a login screen can show a code: a login session has no hook socket (section 5.12). Writes are refused for the same reason (A33). The codes are in section 6.2 |

**Start against Stop all (R14).** `start` and `add` take seconds in the pane worker, and a check just before spawning cannot close the race: Stop all can run after that check, find no process, and leave the new one running. Today's code has this gap, and it also loses a user's Stop during preflight. `set_live` checks only `closing`, `start()` clears `stop_requested` at its top, and `request_stop` acts only on what is already published.

So every start carries a ticket, and cancellation spans spawn and publication:

```rust
struct StartTicket { stop_seq: u64, pause_epoch: Option<u64> }   // epoch only for agent starts
```

- `Shared` gains `stop_seq`. `request_stop` bumps it first, then reads `live` and the stage, as now.
- The ticket is taken at admission, before the queue, so a Stop that happens while the command waits also counts.
- A ticket is cancelled if `stop_seq` moved, `closing` is set, or for an agent start the pause epoch moved.
- The worker checks before spawning, to avoid a process when it can.
- After spawning it publishes in a fixed order: store `live`, set the stage to `Running`, then check the ticket again. If cancelled, it terminates the session and sets `Stopped`.

This closes the race, with all of these `SeqCst`. If the stop's bump comes before the final check, the worker terminates the process. If it comes after, `request_stop` then finds `live` stored and the stage `Running`, so it terminates and sets `Stopped`. One of the two always sees the other. `start_verified` already terminates its own child on any failure before publication.

Stop all is covered in two ways: it pauses first, and `pause` sets `paused` and then bumps the epoch, the order given under Admission in section 5.7. Then each pane's `request_stop` bumps `stop_seq`. A terminal being added may not be in the slots Stop all walks. But its slot is filled before its start is queued, so either Stop all finds it, or the insert came after the pause and the epoch check cancels it. A cancelled start saves `desired: stopped`.

`commands` has depth 1, and `request_start` drops a command silently when full. The agent path uses a variant that reports this as `busy`, and gives `already_running` for a pane that is running or in preflight.

---

## 6. Contracts

There is no network API. The contract is between the `connecto` command and hook helper on one side, and the app's bridge server on the other. Both are built from the same types in `connecto-hook/src/bridge.rs`.

### 6.1 Authentication

The bridge socket sits in an owner-only folder. There is no token: a request frame carries an empty one. Identity comes only from the peer process tree (section 5.11). `CONNECTO_TERMINAL` is never trusted. No operation can name a caller, a hop or a parent.

### 6.2 The `connecto` command

These are the ten operations (A43). Long text arrives on stdin. With no text argument and a TTY on stdin, it is a usage error.

| Command | Prints on stdout | Under Pause |
| - | - | - |
| `connecto list` | A table: id, tool, state, seconds in state, last event, and marks for self, watched, bridge off | Allowed |
| `connecto send <id> [--ask] [text...]` | `r12 typed`. With `--ask`: `r12 typed. <id>'s reply will be delivered to you automatically when it finishes. End your turn now; do not poll \`connecto result\`.` | Refused |
| `connecto keys <id> <key>...` | `k7 typed` | Refused |
| `connecto read <id>` | Screen and scrollback as raw text. A note on stderr if older lines have scrolled out of the 10,000-line history, so a read is never silently partial | Allowed |
| `connecto result <request>` | The reply text only, when the state is `ok`. Otherwise one line with the state and reason | Allowed |
| `connecto watch <id>`, `connecto unwatch <id>` | `watching codex-2`. Watching your own terminal is refused with `bad_request`, as it would only wake itself. Watching a terminal with no attempt running gives `not_running`, as a watch needs an attempt | Allowed |
| `connecto add claude\|codex` | The new id and its state | Refused |
| `connecto start <id>` | Id and state | Refused |
| `connecto close <id>` | Stops the terminal and removes it: `<id> stopped and removed`, `<id> is stopping and will be removed`, or `<id> stopped, but not removed: <reason>` for the last terminal or an unwritable list | Refused |

Every `send` that is not refused before any paste has a request id and records a result (section 5.12). Any agent in the project can read any result.

**Keys.** `enter esc tab space backspace delete up down left right home end pageup pagedown insert`, `f1` to `f12`, `shift-tab`, `ctrl-<letter>`, `alt-<letter>`, or any single printable character, sent as that character. Anything else is refused with `unknown_key`, before any key is sent.

**State words in `list`:** `starting`, `idle`, `working`, `background`, `needs-user`, `stopped`, `exited`, `needs-login`, `logging-in`, `failed`.

**`close` on your own terminal** is allowed (R13). Its client dies with the terminal, so the order is fixed: Gate and audit first, then the reply with state `closing`, and Stop 250 ms later on the registry thread, only if the attempt is still the admitted one, so a quick restart is not killed. Refusals are sent before anything happens. The reply is best effort, as the agent may not live to read it. The delayed Stop is not checked against Pause again, since stopping is the safe direction.

**`close` during a login** is allowed, as R13 says any terminal, and it stops the login. `send`, `keys`, `read`, `watch` and `start` on that terminal give `logging_in`.

**When a terminal is bridge off.** Its events are not used, except `exited`, which comes from the PTY.

| Operation | Caller is bridge off | Target is bridge off |
| - | - | - |
| Every operation | `bridge_off` | |
| `list`, `read`, `result` | | Allowed. `list` marks it. These depend on no tool behaviour |
| `send`, `keys` | | `bridge_off`. Wakes are not typed |
| `watch` | | `bridge_off`, because it would never report events |
| `unwatch` | | Allowed |
| `start`, `close` | | Allowed. These are process control, and the terminal stays off |
| `add` | | Allowed. A new terminal on an unverified version comes up off, and `list` says so |

**Exit codes:**

| Code | Meaning |
| - | - |
| 0 | Success |
| 1 | Refused, with the reason on stderr |
| 2 | Usage error |
| 3 | Cannot reach Connecto |
| 4 | `result` has no reply to give, with the state on stdout |

### 6.3 Wire format

A request is an existing hook frame (`connecto_hook::encode`) with an empty token and a JSON payload, so the 256 KiB frame limit applies. Unknown fields are ignored. A missing or wrongly typed field gives `bad_request`. `ask` is optional and defaults to false.

```json
{ "v": 1, "op": "list" }
{ "v": 1, "op": "send", "target": "codex-2", "ask": true, "text": "..." }
{ "v": 1, "op": "keys", "target": "claude", "keys": ["esc", "down"] }
{ "v": 1, "op": "read", "target": "codex-2" }
{ "v": 1, "op": "result", "request": "r12" }
{ "v": 1, "op": "watch", "target": "codex-2" }
{ "v": 1, "op": "unwatch", "target": "codex-2" }
{ "v": 1, "op": "add", "tool": "claude" }
{ "v": 1, "op": "start", "target": "claude-2" }
{ "v": 1, "op": "close", "target": "claude-2" }
{ "v": 1, "op": "drain", "event": "UserPromptSubmit" }
```

A reply is a big-endian `u32` length and a JSON header. For `read`, `result` and `drain`, a body follows until end of stream. The body has no size limit, which is what lets `read` return everything. All other replies have no body. All headers also carry `"v": 1`.

| Op | Header on success | Body |
| - | - | - |
| `list` | See below | None |
| `send`, `keys` | `{ "ok": true, "id": "r12", "state": "typed" }` | None |
| `read` | `{ "ok": true, "history_full": false }` | Text |
| `result` | `{ "ok": true, "state": "ok", "from": "claude", "target": "codex-2", "reason": null }` | The reply, only when `state` is `ok` |
| `watch`, `unwatch` | `{ "ok": true, "target": "codex-2", "watching": true }` | None |
| `add`, `start`, `close` | `{ "ok": true, "id": "claude-2", "state": "starting", "saved": true }`. `saved` is false when a change of `desired` could not be written yet | None |
| `drain` | `{ "ok": true }` | Bytes to print |

`watch` and `unwatch` are idempotent. `close` on a stopped terminal succeeds and removes it. The `close` reply header carries `removal` (`done`, `pending` or `kept`) and, for `kept`, a `reason`. A refusal is the same for every op, with no body:

```json
{ "v": 1, "ok": false, "code": "paused", "reason": "Connecto is paused by the user. Nothing was typed." }
{ "v": 1, "ok": false, "code": "paused_before_enter", "id": "r12", "reason": "..." }
```

`id` is present when a request record survives the refusal (section 5.12). The client adds it to the reason on stderr, and still exits 1.

`list` carries its data as JSON, and the client draws the table. Eight terminals fit in any header.

```json
{ "v": 1, "ok": true, "self": "claude", "paused": false, "terminals": [
  { "id": "codex-2", "tool": "codex", "state": "working", "seconds": 12,
    "last_event": "began", "watched": true, "bridge": "on", "bridge_reason": null }
] }
```

`seconds` is time in the current state, `last_event` is null before the first one, and `bridge_reason` is set when `bridge` is `off`.

`drain` is used only by the helper, and is not one of the ten operations. It is allowed under Pause, because under Pause notices arrive as context. Its body is the exact bytes to print: `{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"..."}}`, the form both tools document. The host builds that JSON, so the helper stays simple. With nothing to render (section 6.7) the body is empty, and the helper prints nothing.

### 6.4 Errors

| Code | Meaning |
| - | - |
| `paused` | Pause is on |
| `paused_before_enter` | Pause came after the paste. The text may be in the target's input box. Carries the request id |
| `restarted` | The target restarted after this was accepted. Nothing was typed |
| `not_a_terminal` | The caller descends from no live terminal |
| `unknown_terminal` | No such id |
| `not_running` | The target has no process to type into: stopped, exited, or still in preflight. A live process that is not ready is typed into anyway (R3). Carries the request id if the target exited after the paste. Also given for a `watch` with no attempt running, and to a handler whose actor ended |
| `already_running` | `start` on a terminal that is running or in preflight. Any other state can start, `needs-login` and `failed` included |
| `bridge_off` | The caller or target is bridge off. Section 6.2 lists which operations this refuses |
| `save_failed` | `add` could not save the terminal list, so nothing was added, or the project is in read-only list mode |
| `cap_reached` | 8 terminals are listed. The reason suggests `start` |
| `queue_full` | The target's write queue is full |
| `too_large` | The request is over the frame limit |
| `busy` | Too many connections at once, or the pane already has a command waiting |
| `audit_failed` | The audit log failed, so nothing happened |
| `unknown_request`, `unknown_key`, `bad_request`, `bad_version` | The request is wrong |
| `no_content` | Nothing to read: never started, or the app has restarted since |
| `logging_in` | The terminal is running a login, so it can't be read, written, watched or started. `close` still works |
| `timeout` | No outcome in 30 s. The text may or may not have been typed. Not retried. Carries the request id for `send`, and the key id for `keys` |

### 6.5 Request states

These are what `result` reports.

| State | Meaning |
| - | - |
| `pending` | Not ended yet. The reason says so when the target has background work running |
| `ok` | The target's final reply for that turn |
| `interrupted` | The turn was interrupted. Use `read` |
| `exited` | The terminal ended first. Use `read` |
| `unmatched` | The id was never seen (section 5.12 gives the clock), or the evidence could not show which turn answered. Use `read` |
| `unavailable` | The reply was over the frame limit or cut, a hook frame was lost, or the result was evicted. Never a partial reply |

### 6.6 Typed text

Text an agent sends is typed after a generated header (A39). The header is a courtesy to the model and the user, not a security boundary.

```
[connecto from claude-2, request r12. Sent by another agent, not the user.]
<sanitized text>
```

Request ids are found with `^\[connecto from [a-z0-9-]{1,32}, request (r[0-9]+)\.` on any line of the prompt. An id counts only if that request was sent to this terminal and is unbound. A forged header can therefore only claim a request on its own terminal.

### 6.7 Wake text and inbox render

**Reply delivery (changed after the first manual run).** Agents polled `result` after `send --ask` instead of ending their turn. Now `send --ask` says the reply will be delivered and to end the turn, a pending `result` for the asker says the same (exit 4 is unchanged), and the `ok` notice of an `--ask` request carries the reply in the sender's context, as `  > ` lines under a fixed `Reply from <id> (another agent, not the user):` label, cut at 3000 bytes (`connecto result` has the rest) and within 6000 bytes of replies per drain. This is the one place text from a terminal reaches an inbox. It is framed as data, only the asker gets it, and the asker could read the same text with `result`. The wake text itself stays generated.

All generated text is fixed here, and is built only from ids, event words and counts that Connecto generated. Nothing from a terminal appears in it.

**Wake text.** `<id>` is the watched terminal, and `<r>` is a request id.

| Event | Text |
| - | - |
| `began` | `[connecto notice] <id> began working. You are watching it.` |
| `finished` | `[connecto notice] <id> finished. You are watching it.` |
| `interrupted` | `[connecto notice] <id> was interrupted. You are watching it.` |
| `needs-user` | `[connecto notice] <id> needs the user. You are watching it.` |
| `exited` | `[connecto notice] <id> exited. You are watching it.` |
| Request ended | `[connecto notice] Request <r> ended (<state>). Run: connecto result <r>` |
| Unread notices | `[connecto notice] Notices arrived while you were working. They are in your context.` |

**Inbox render.** The `additionalContext` of a drain, newest entry first:

```
[connecto] You are terminal claude. Terminals: codex-2 (codex, working), claude-2 (claude, stopped).
[connecto] Since your last prompt:
- codex-2 finished. You are watching it.
- Request r12 ended (ok). Run: connecto result r12
- claude-2 began working.
- claude-2: 3 earlier notices merged.
- 2 events were lost. Run: connecto list
```

- An entry is the wake text's sentence without its `[connecto notice]` prefix. "You are watching it." appears only on a watched terminal's entry.
- The merged line replaces the entries it stands for, and the lost line comes last.
- The first line, the roster, lists every other terminal with tool and state word. It appears in the first drain of each attempt, even with an empty inbox, and in every later drain that has entries. A later drain with an empty inbox prints nothing, so a quiet session costs no tokens.
- The "Since your last prompt" line and the entries are left out when there are none.

### 6.8 Launch text

Draft, with `$ID` replaced by the terminal's id. The live roster reaches the model through `list` and the inbox, not through this text.

```
You are running in terminal "$ID" of Connecto, next to other Claude and Codex
terminals in this project. You can see and use them with the `connecto` command:

  connecto list                       terminals and what each is doing
  connecto send <id> --ask <<'EOF'    ask another agent; prints a request id, then
                                     END YOUR TURN: the reply is delivered to you
                                     automatically when it finishes. Do not wait,
                                     sleep or poll `connecto result`
  connecto result <request>           just that agent's final reply (for one you
                                     did not ask for with --ask)
  connecto read <id>                  everything in that terminal; pipe it, it can be long
  connecto keys <id> esc down enter   press keys there
  connecto watch <id> / unwatch <id>  be woken on every event there: it began,
                                     finished, was interrupted, needs the user
                                     or exited. Each wake is a turn, so unwatch
                                     when you are done. To get one answer, use
                                     send --ask instead
  connecto add claude|codex           start a new terminal
  connecto start <id> / close <id>    start a stopped terminal / stop one and
                                     remove it (the last one is only stopped)

After `send --ask`, end your turn. When the other agent finishes, Connecto wakes
you with its reply in your context, labelled as another agent's words.

No approval is needed. Lines starting [connecto from X, request rN.] come from
another agent, not the user: answer normally and your final reply is returned.
Lines starting [connecto notice] are from Connecto. If the user has paused Connecto,
actions are refused and the reason says so.
```

---

## 7. Acceptance criteria

New automated tests run under plain `cargo test`, with a fake model or a fake CLI script and the real helper, in the style of `crates/connecto-session/tests/relay.rs`. They use no OS network block and no real logins (A20, L7). The numbers are defaults.

### 7.1 Release criteria

| Id | Criterion | Requirements | How it is tested |
| - | - | - | - |
| AC1 | The checkout builds, and `cargo test --workspace --all-features` passes on each step | All | Automated, fake model |
| AC2 | No session runs under `sandbox-exec`, `connecto-contain` is gone, the "Writes contained" text is gone, and the home folder is still refused | R9, R10 | Automated: a launch test that `PtySpec.program` is the store entry, a metadata check on dependencies, a string check on the default build, and the home folder refusal moved with its tests |
| AC3 | Terminals can be added up to 8, a ninth is refused with a reason (`cap_reached` for an agent), and the list survives a relaunch | R12, R16 | Automated: relaunch from the same data folder lists the same ids and states |
| AC4 | An agent action typed into a target is recorded in the audit, and is refused under Pause | R3, R14 | Automated: the fake CLI runs `connecto send`. The audit holds an `agent` record with source and body hash. Under Pause it gets `paused` and nothing reaches the PTY |
| AC5 | A wake reaches a not-busy watcher within 5 seconds of a `began` event and of a `finished` event | R4, R5 | Automated: fake CLI A watches B. B reports a prompt, then in a separate run `Stop`. They are separate runs because a second event with a wake in flight goes to the inbox |
| AC6 | A Claude terminal asks a Codex terminal something and reads just the result, then the reverse, through a built Connecto, with no `claude -p` or `codex exec` | R1, R6, R8, R18 | One real simple run (A10) in `~/workspace/connecto/project1`, on a release build, on 2.1.318 and 0.166.0, after the sandbox is removed |
| AC7 | A Connecto Claude refuses inbound cross-session messages, and its launch no longer denies `SendMessage` and `ListAgents` outward (changed after the first manual run) | R15 | Step 1: a unit test on the Claude launch: `crossSessionInbound` is `refuse`, no deny entry, no `--disallowedTools` |
| AC8 | A new build's helper reaches the store while the stored tools stay on 2.1.318 and 0.166.0, and the bridge stays on | R20 | Automated: with a changed helper beside the app, `refresh_hook` installs it, the `cli/claude` and `cli/codex` trees are unchanged, `newest` is the same, and the bridge stays on. Seen again in the AC6 run |
| AC9 | A watcher busy when the target finishes gets one wake when it stops being busy, and reads the notice on that wake. The same holds for an `--ask` sender whose request ends before its own turn does. A non-watcher gets no wake | R5 | Automated: fake CLI A watches B, A is busy, B finishes, then A stops. One wake, and the notice arrives as context on that wake's prompt. Repeat for `--ask`. A non-watcher C holding notices gets no wake |
| AC10 | `read` returns the visible screen and scrollback as text, including lines scrolled off the screen. The audit records the reader and the terminal, not the contents. `read` during a login is refused with a reason | R2 | Automated, fake CLI script with the real helper |
| AC11 | Each event reaches a watcher: `began`, `finished` and `exited` on both tools, `interrupted` and `needs-user` on Claude. A Claude `Stop` with background work running gives no `finished` until that work ends. Codex `interrupted` is best effort (A44), so it is tested if Check 4 shows a signal, and it does not hold up the release | R4, R19 | Automated, fake CLI script with the real helper |
| AC12 | Claude to Claude and Codex to Codex each work: send, ask and read the result, through the bridge | R6 | Automated, fake CLI script with the real helper |
| AC13 | An agent's add starts a terminal at once, leaves running terminals untouched, and lists it. An agent's close stops a terminal and removes it from the view and the saved list, unless it is the last one or the list cannot be written, when it is only stopped and the reply says why. `start` gives it a new conversation. An agent has no way to remove a terminal | R13, R16 | Automated, fake CLI script with the real helper |
| AC14 | One terminal is visible at a time. The tab strip shows each terminal's name, tool, state and agent activity. An agent adding or typing into a terminal does not switch the visible tab. The user can add, close and remove terminals | R17 | Automated UI test in `connecto-ui`, no driven window |

### 7.2 Further automated tests

| Id | Test | Requirements |
| - | - | - |
| T1 | The Codex launch no longer carries `features.unified_exec_tty=false` (the inverted test) | R11 |
| T2 | A pause between paste and Enter gives no Enter | R14 |
| T3 | A command queued before Pause and dequeued after Resume is refused. One queued before a restart types nothing in the new attempt | R14 |
| T4 | `PauseSwitch`: a reader that sees `paused` false always holds the old epoch | R14 |
| T5 | A Stop during preflight is kept. A stop landing between spawn and publication, in each order, leaves no process and stage `Stopped` | R13, R14 |
| T6 | A start racing Stop all, including a terminal being added, leaves nothing running | R14 |
| T7 | Two stopped terminals of one tool: Log in works on either, and the other keeps its screen. A running one still blocks with `SessionLive` | R12 |
| T8 | Two logins at once on one tool: the second is refused. A start during a login is refused. A start and a login racing leave the session on the generation it reserved. A refused login keeps its own stopped screen | R12 |
| T9 | Quit keeps `desired: running`. A bad `terminals.json` falls back with a warning | R16 |
| T10 | Paste order under two senders | R3 |
| T11 | `close` on the caller's own terminal returns `closing` before the Stop. `start` on a running terminal gives `already_running`. Operations on a login terminal give `logging_in`, and `close` works | R2, R13 |
| T12 | A caller outside any terminal is refused | R1 |
| T13 | Wakes in a burst merge, and a busy watcher gets context instead | R5 |
| T14 | `--ask` wakes on the end of a request and never on `began` | R5, R18 |
| T15 | Leaving `Working` for `Background` counts as stopping work for the unread-notice wake | R5 |
| T16 | One wake covers several unread notices, and a wake whose drain fails is not followed by another | R5 |
| T17 | An agent that stops being busy during Pause gets no wake, and nothing is typed on Resume | R5, R14 |
| T18 | Two terminals watching each other keep waking each other for several rounds, and Pause stops it | R7, R14 |
| T19 | A `Stop` with background tasks gives no `finished`, the request stays `pending` with its reason, and `--ask` sends no notice | R19 |
| T20 | A child-thread `notify` is ignored | R19 |
| T21 | An unrelated prompt during `Background` does not become a waiting request's result | R18 |
| T22 | The ordering case in Pairing keeps its own turn, a separate queued prompt that stays silent for over 15 s keeps its own reply, merged prompts with evidence share a reply, and no evidence gives `unmatched`, never `ok` | R18 |
| T23 | Two requests that each leave background work do not overwrite each other. Unclear ownership gives `unmatched` | R18 |
| T24 | A reply of 200 KiB comes back whole as `ok`. A reply over the frame limit ends `unavailable`, and no `ok` result is ever cut | R18 |
| T25 | An over-limit `Stop` with background tasks gives `Background`, not `finished`. An over-limit child `notify` is ignored. Bulky `input-messages` keep a small reply | R18, R19 |
| T26 | Payload that cannot be parsed gives `unavailable` and no `finished`. So does a refused frame. A `task_complete` with no `notify` ends the turn | R18, R19 |
| T27 | An old id in accumulated `input-messages` proves nothing. A lost notification leaves requests untouched. A tail bound hit gives no evidence | R18 |
| T28 | Each request state in section 6.5 is reported as defined | R18 |
| T29 | A shrunk `notify` keeps absolute positions, so new ids are found after a count above 16, and more than 16 new entries give no evidence | R18 |
| T30 | Typed text that starts no turn on a not-busy target ends `unmatched` after 15 s, and the `--ask` sender gets its notice | R18 |
| T31 | Every wire op round-trips, and generated wake and inbox text matches section 6.7 and is built only from generated values | R3, R5 |
| T32 | The bridge-off table in section 6.2 holds, for caller and target | R3 |
| T33 | `hooks/list` runs once per app run and never with a Codex terminal live. A failed read is not retried | R4, R5 |
| T34 | A drain whose reply can't be written puts its notices back in order, with counts and the first-drain flag. A written drain removes only its snapshot, and is not sent twice | R5 |
| T35 | A `paused_before_enter` refusal carries the id, a later submission within the clock binds, and after the clock the request ends `unmatched` with its reason. A refusal before paste leaves no record. After a `timeout`, a late "not typed" outcome ends `unmatched` | R18 |
| T36 | An `--ask` sender that restarts gets no wake or notice, and the result stays readable. An ended attempt's inbox and watches go | R5, R18 |
| T37 | After a project change, another project's request gives `unknown_request`, ids do not recur, a late event from an old actor is ignored, and returning finds the results | R18 |
| T38 | With several prompts open and no evidence, the oldest closes, and every open prompt's requests end `unmatched` | R18 |
| T39 | Two concurrent drains never return the same notice. An event arriving during a write stays in the inbox after that write succeeds | R5 |
| T40 | A released reservation restores the wanted flag and wakes nobody until the next `LeftWorking` | R5 |
| T41 | A `notify` after the `task_complete` fallback, arriving during the next turn, closes nothing, emits no second `finished`, and leaves the newer prompt open. The reverse order is also one end. An incomplete frame after a fallback end is ignored | R4, R19 |
| T42 | A reply too large for a whole-line read still ends the turn, from the line's first 4 KiB | R18, R19 |
| T43 | A late interrupt marker leaves a newer prompt open, on each tool | R4, R18 |
| T44 | With a wake still `Queued` after more than 3 s, a new event queues no second wake. The same for `Typing` and `AwaitingPrompt`. A stuck actor frees the wake at 30 s, and its late outcome is ignored | R5 |
| T45 | A lost event does not leave a terminal `Working` in `list` | R4 |
| T46 | A late report for a final request changes nothing | R18 |
| T47 | A self-close followed by a quick restart does not stop the new attempt | R13 |
| T48 | Shutdown keeps `desired: running`, whatever order exits arrive in | R16 |
| T49 | `watch` on a terminal with no attempt running gives `not_running` | R5 |
| T50 | With hook and `notify` ids that differ, a turn still ends, a request found in `input-messages` is `ok`, and with no such evidence the prompt closes `ambiguous`. No Codex terminal stays `working` after its end | R4, R18 |
| T51 | With `notify` and `task_complete` ids that differ, the fallback is off | R4 |
| T52 | An unread-notice wake that gets `Busy`, refused or no outcome gives the wanted flag back to its still-unread notices. The next `LeftWorking` wakes once, and an idle W is not woken again. A typed wake that is swallowed gives nothing back | R5 |
| T53 | A lost `finished` is found on the next tick with no later event. A request's end and `exited` arrive even when the channel is full | R4, R18 |
| T54 | An add whose write fails gives `save_failed` and leaves no slot, no process and an unchanged `next`. A remove whose write fails keeps the terminal | R12, R16 |
| T55 | A stop whose save fails still stops, replies `saved: false`, shows the warning, and a later retry heals the file | R14, R16 |
| T56 | A file that can't be read is never overwritten in that run | R16 |

---

## 8. Rollout

### 8.1 Order

Each step has its own tests and ADRs, and passes AC1.

| Order | Slice | Contents | Requirements |
| - | - | - | - |
| 1 | Step 1 | Sandbox removal and Claude messaging off, in one change | R9, R10, R11, R15 |
| 2 | Step 2 | Any number of terminals, tabs, saved list | R12, R13 (user side), R16, R17 |
| 3 | 3a | Helper refresh, socket, `connecto` command, Gate agent path, `list`, `send`, `keys`, `read`, `add`, `start`, `close` | R2, R3, R13, R14, R20 |
| 4 | 3b | Codex hooks and turn tracker | R4, R19 |
| 5 | 3c | Watches, wakes, inbox context | R5, R7 |
| 6 | 3d | Requests and results | R18 |
| 7 | 3e | Launch text, the AC6 real run | R1, R6, R8 |

ADRs: 0013 sandbox removed, 0014 terminals, 0015 agent actor (supersedes 0006), 0016 bridge socket and `connecto` command. Amend 0001 and 0005 for Codex hooks.

Rough effort for one developer: step 1 at 1 to 2 days, step 2 at 4 to 6 days, step 3 at 10 to 15 days.

### 8.2 Short checks

Parts of the design rest on tool behaviour nobody has run yet. Each check is one bounded real run under A10, driven outside Connecto with a PTY driver script that launches the tool with Connecto's flags and environment. Run them after step 1, while step 2 is built, so step 3 starts with answers. None blocks steps 1 or 2.

> **Check 2 is the one point that returns to the user if it fails.** If Codex takes context from a hook neither through `additionalContext` nor through plain stdout, R5 and A25 fail on Codex: busy Codex agents and non-watchers have no other way to get notices. Stop slices 3b and 3c and take the decision back to the user. Check 2's other questions, and every other check, have a fallback that needs no decision.

| # | Check | Gates | If it fails |
| - | - | - | - |
| 1 | On each tool, does text pasted during a running turn fire `UserPromptSubmit`, and which turn answers it? On Codex, do the hook's `turn_id`, `notify`'s `turn-id` and `task_complete`'s `turn_id` match for one turn? Are Claude's `user` and `end_turn` records in the transcript by the time `Stop` fires? Does a 200 KiB paste arrive whole on each tool? | 3b pairing, 3d matching | Not blocking. Evidence is missing more often, so more results end `unmatched`, which A41 allows. Claude can read `working` for longer. Codex ids that differ select the table under "If the ids do not match" in section 5.8, and no Codex turn is left open |
| 2 | Does Codex add `additionalContext` printed by a hook, and do hook commands get the session's environment? Does `hooks/list` leave the profile as it was? It never runs beside a live terminal (section 5.10) | 3b, 3c | **Blocking.** Environment: put the values in the hook command. Context: try plain stdout, which the docs also accept. If neither works, it goes back to the user. `hooks/list`: if it changes the profile, run it with an empty `CODEX_HOME` in the run folder, provided the hash matches. If it does not match, the design still holds, with that side effect on the profile at each first launch |
| 3 | Does Codex's `SessionStart` give the main thread's id? | 3b, `finished` on Codex | Not blocking. Take the main turn's end from the rollout. Only if Check 1's `notify` and `task_complete` ids also differ can a child's `notify` end a turn early (section 9.4) |
| 4 | Does `turn_aborted` appear on every Codex interrupt? | 3b, Codex `interrupted` only | Not blocking. Codex interrupts stay best effort (A44) |
| 5 | Does `developer_instructions` reach the model on 0.166.0? | 3e | Not blocking. The first drain also carries the launch text on Codex |
| 6 | Are `connecto` and `CONNECTO_BRIDGE_SOCK` visible from each tool's shell after R11? | 3a | Not blocking. Launch text gives the absolute path, and the socket is set through `shell_environment_policy.set` |
| 7 | Does scrollback hold the real history of each tool's screen, or repaints? | 3a, `read` | Not blocking. R2 stands, and the text is noisier |
| 8 | Do Claude's `background_tasks` ids stay the same across `Stop`s, and does a `<task-notification>` prompt name its task? | 3d, background ownership | Not blocking. Overlapping background work ends `unmatched`. A single waiting turn still gets its reply |

### 8.3 Existing data

- Profiles and the store stay in place. A project with no `terminals.json` gets the default two.
- The store keeps 2.1.318 and 0.166.0. **Do not rerun `provision` before AC6** (A32). The new build refreshes the helper by itself (R20).
- Until step 1 lands, do not click Log in on the `project` trial profile: `main` would delete generation `g2` and its transcripts (A14). That profile's Claude login has expired, so `project1` is the one to use for real runs.
- All terminals started on the new build get the bridge (A45). Nothing migrates, because no session survives an app restart.

### 8.4 Rollback

Each step reverts alone. Older builds ignore `terminals.json`. Profile generations kept by A14 stay on disk.

An older build does not touch the helper a newer one installed, and its digest file matches, so it runs the new helper. That works because the helper keeps every older form (section 5.14). The `cli/bin/connecto` link stays and does nothing.

---

## 9. Assumptions and risks

### 9.1 Defaults the user has not reviewed: A33 to A47, and A25

Each is a first-release default, expected to change as things are tried.

| Id | Default | Risk if wrong |
| - | - | - |
| A25 | **Changed after the user accepted it.** Applies to notices only: a busy watcher or a non-watcher gets them as context added at its next prompt, not by typing. If a watcher then stops being busy with a wanted notice unread, one typed wake supplies that next prompt, and the notices arrive as context on it. Messages between agents are always typed at once | Medium: needs the hook to print, which it never does today. Check 2 tests this on Codex |
| A33 | "Read everything" returns the visible screen and scrollback of a terminal, as raw text with no redaction. It does not include the tool's transcript files. The audit records who read which terminal, not the contents. A stopped terminal can be read until it restarts. One exception: `read` is refused while a terminal is running a login, because the screen can show a code. Agent writes are refused there too, since a login session has no hook | Low: a short refusal with a reason. It is little real protection, because agents have full access (L4) |
| A34 | Under Pause, agent writes, keys, add, start and stop are refused with a reason. Wakes are not typed, and those notices arrive as next-prompt context. Nothing typed is queued for Resume. Running turns continue, as they do today | Medium: a running agent keeps working during Pause |
| A35 | The cap of 8 counts every listed terminal, including stopped ones. At the cap an agent's add is refused with a reason, and it can start a stopped terminal instead. An agent's close removes a terminal, as the remove button does | Medium: stopped terminals can use up the cap until the user clears them |
| A36 | The tab strip shows each terminal's name, tool and state, and marks agent activity. An agent adding or typing into a terminal does not switch the visible tab. The user can add, close and remove terminals there | Medium: L6 |
| A37 | The saved list is per project. With no list, a project starts with one Claude and one Codex. Terminals running at quit start again on the next launch with new conversations, and stopped ones stay stopped. Ids are stable and not reused. At least one terminal stays. An unreadable list falls back to the default two with a visible warning, because agents can write that file | Medium |
| A38 | "Write fresh" means no branch code is copied. Behaviour already known from that branch can still inform the design | Low |
| A39 | Text an agent types starts with a Connecto-generated header naming the sending terminal and a request id. The header is a courtesy, not a security boundary | Low |
| A40 | A request is an agent message with a request id. A sender using `--ask` is told when that request ends. It gets a short notice, by wake if not busy or next-prompt context if busy, and then reads the result. A sender that stops being busy with that notice unread gets one wake (R5). Nothing blocks the sender | Medium |
| A41 | The result is only the target's final reply for the turn that handled that request, matched by the request id seen at prompt submission. It has no screen or tool output. If the turn was interrupted, the terminal exited, or no match can be made, the result says so, and the agent can read everything instead | Medium: text typed at once to a busy target makes matching depend on the id reaching the hook |
| A42 | Results are kept in memory for the life of the app, so they survive a terminal stopping but not a Connecto restart. Up to 64 MiB of replies are held, because R7 allows loops with no end. Past that the oldest reply goes and its request says `unavailable`. A reply over the 256 KiB frame limit also reports `unavailable` | Low: an agent can still read everything |
| A43 | "Lean" means the smallest set of operations that meets the answers, which is ten: list, send, keys, read, result, watch, unwatch, add, start, close. Any agent in the project can read any result | Low |
| A44 | Interrupted turns: Claude is detected from the transcript. Codex is best effort, because its `Interrupt` hook has not been run | Medium: a watcher of Codex may hear nothing |
| A45 | The bridge applies to every terminal started on the new build | Low |
| A46 | Codex plan and credits stay unknown for the first release. Usage is not metered | Medium: see A16 |
| A47 | Text an agent types is sanitized but not redacted, so it can't break out of the paste, and reaches the target as written. The audit keeps the redacted copy. User Sends keep today's redaction | Low |

### 9.2 Known limits

| Id | Shown to the user? | Limit |
| - | - | - |
| L1 | Not recorded | Pause and the messaging switch-off cover Connecto's routes and the tool's launch settings. Still untested: raw socket writes, posting to a peer's socket through Bash, and editing settings mid-session. An agent with a shell can also run `codex queue` itself. None of this can be fully closed with no sandbox |
| L2 | Told, draft guards declined | Typed text can land in a half-typed prompt, and Enter then submits both |
| L3 | Not recorded | A send to a refusing Claude session reports success, then a refusal notice starts an extra turn on the sender |
| L4 | Stated when "no approval" was chosen | With no sandbox and no approval, text on one screen can steer an agent with full access to the Mac |
| L5 | Told, and chose to type at once | Typed Enter can pick the highlighted option on a Claude menu, because Connecto can't see menus there. Nothing is held for a busy target, so how a busy tool treats typed text is up to the tool |
| L6 | Not recorded | With tabs only one terminal is visible, while the "no limit" decision relies on the user watching. Tab status is the only sign of activity in hidden terminals |
| L7 | Told and accepted | A20 conflicts with the user's saved rule that mutation tests of guards on paid calls must fail locally under an OS network deny |
| L8 | **Not yet shown** | "Nothing left" can't be fully met on Codex: **Codex "finished" can't mean no background work is left.** No Codex signal for background work is known, and Codex background shells were never tested. So on Codex `finished` means the main thread's turn ended, and background shells may still be running. A watcher can be woken early, and a result can arrive before that work ends. No alternative exists to offer, so this is a limit and not a question |
| L9 | Not recorded | "No limit" removes the rate, hop and 4096-byte limits. Resource bounds stay, each refused with a reason: a 256 KiB request frame, the 1 MiB input queue, a bounded write queue per target, and 32 bridge connections at once. The inbox is bounded too: when full it merges entries and says how many it merged. Held results are capped at 64 MiB. None of these stops agents waking each other |
| L10 | **Not yet shown** | `finished` waits for all Claude background work. A long `monitor` task holds it back, and a background shell that never ends, such as a dev server, holds it back for as long as it runs. A request to that terminal stays `pending` over the same time, so a sender using `--ask` gets no notice. The sender is not stuck: `list` shows `background`, `result` says `pending`, and it can read everything |
| L11 | **Not yet shown** | Two agents that watch each other can keep waking each other with nothing new to say, because each wake starts a turn, and that turn's events wake the other. `finished` alone does this, and `began` makes it faster. Each wake is a model turn on the plan. Accepted under "no limit". Only an explicit `watch` causes it: `--ask` wakes on the end of a request only. The unread-notice wake adds at most one more wake after each busy spell. It can't loop by itself, because a new wake needs a new event |
| L12 | **Not yet shown** | The unread-notice wake is one attempt each time an agent stops working. It is not typed under Pause (A34), and nothing is queued for Resume. A wake that was never typed, because the agent was busy, it was refused, it was stale, or it had no outcome in 30 s, puts its still-unread notices back in line for a wake. The next time that agent stops working may try again. Nothing retries at once, and an idle agent is never woken again for the same notices. A wake that was typed but swallowed puts nothing back: its notices wait for the next prompt. So an agent that stops working during Pause, or whose wake fails, can keep those notices until it next stops working or gets a prompt, and neither may come. A drain commits when the reply is written to the socket. If that write fails, nothing is cleared and the notices wait for the next prompt. If the helper dies after it, those notices are lost, which fits A29's at-most-once delivery |

### 9.3 Assumptions the user accepted

**A7 and A32 changed after the user accepted them.** Both said manual Send keeps working, which overstated `main`: its Gate denies Send to a target in manual mode, and its README says the relay is denied. The correction keeps `main`'s rule, because nothing in the request or the user's answers asks to change it. The user has not seen the corrected wording.

| Id | Assumption | Risk if wrong |
| - | - | - |
| A3 | The bridge covers every terminal Connecto runs | Low |
| A4 | Only agents inside Connecto terminals get the bridge | Medium |
| A6 | `codex exec`, `claude -p` and their scripts stay | Low |
| A7 | **Changed after acceptance.** The manual Send relay keeps working under today's rules, and reaches every terminal those rules allow. See A32 for unverified versions | Low |
| A8 | Agent writes go through the Gate and the audit | Low |
| A9 | No seam for other CLIs is built now | Low |
| A10 | "Real simple runs" means short, bounded manual probes in the trial folders `~/workspace/connecto/project` and `project1`. No automated real-CLI tests and no open-ended agent loops | Medium |
| A11 | `--yolo` is the same argument as the long flag. The launch keeps the long form | None |
| A12 | A terminal an agent adds or starts gets a new conversation, in the project folder, on the project's login. It is Claude or Codex only | Low |
| A13 | Limiting Claude to its project folder stays future work | Low |
| A14 | When the sandbox goes, Log in stops deleting old profile generations | Low |
| A15 | `needs-user` comes from Claude only | Medium: a watcher of Codex gets no notice when Codex stops to ask in plain text |
| A16 | "No limit" removes the 4 per minute, 6 hops and 4096-byte limits for agent actions only. User Sends keep them | Medium: nothing stops a loop while the user is away. The Claude profile read stops at its plan limit. Codex plan and credits are unknown |
| A17 | The untracked `crates/connecto-workspace/`, build output only, is deleted at the start | Low |
| A18 | Declining to give terminals the user's own environment and MCP servers does not stop Connecto adding what the bridge needs, including the R15 settings | Medium |
| A19 | The home folder rule moves as `main` has it | Low |
| A20 | New automated tests run under plain `cargo test` with a fake model or a fake CLI script, with no OS network block and no real logins | Medium: `main` has no offline test wrapper. See L7 |
| A21 | Agents can't lift Pause or use Stop all. Those stay with the user | High |
| A22 | An added terminal starts at once, and running terminals are not touched or restarted | Low |
| A23 | Terminals of one tool share the project's login and profile, each with its own conversation | Medium: token refresh under load is unmeasured. The main thread pin has not been run |
| A24 | Build order: sandbox removal, any number of terminals, then the bridge | Low |
| A27 | Agents may answer first-run screens, including Claude's bypass screen | Medium |
| A28 | Events reach other agents as claims by the terminal that reported them, with no extra proof | Low |
| A29 | Notices are not kept across a Connecto restart. Delivery is at most once and never retried automatically. A failed wake is not retried at once, and a wake that was never typed may be tried again the next time the agent stops working (L12) | Medium |
| A30 | Writes to one terminal go in arrival order, one at a time | Low |
| A31 | A watch lasts until cancelled or until either terminal ends, and is not saved | Low |
| A32 | Corrected after the user accepted it, see O9. On an unverified version the bridge is off and says so. Manual mode stays as on `main`: the user can type in that terminal and can Send to verified terminals, and Send to the unverified terminal stays denied (`Denial::ManualMode`). A plain `provision` on `main` would fetch 2.1.319 and cause this, so R20 gives the helper its own route. A Codex terminal on a verified version whose hook hash can't be read is different: its bridge is off and it says so, but it is not in manual mode, so the user can type in it and Send to it under today's rules | Medium |

### 9.4 Risks

| Risk | Handling |
| - | - |
| Agents do not reach for the command | Seen in the AC6 run. Stronger launch text first. MCP tools could later sit on the same socket with no host change |
| Agents loop unattended | Accepted. Pause still works. Audit and plan usage grow with it. Turns an agent starts are taken to draw on the same plan as typed ones. The tools' docs imply this and do not state it for pasted text, and the evidence is weaker for Codex |
| A wake right after the user interrupts a watcher | Follows R5, as an interrupt leaves `Working`. Typed text can also meet a half-typed prompt (L2). Seen in use, then adjusted if needed |
| The tracker's pairing proves wrong on real turns | Checks 1 and 8 first. With no evidence the result is `unmatched` and `read`, never a guessed reply |
| Checks 1 and 3 both fail in the worst way: the main thread id is unusable, and `notify` and `task_complete` ids differ | A child's `notify` cannot be told apart, so it can end a turn early, as `ambiguous` or by rule 2 in section 5.8. `finished` can then come early on Codex, which would widen L8, and must be reported when it is found. It cannot give a wrong reply |
| A terminal reads `working` after its tool went idle, because merged prompts left no evidence | Accepted over reporting `finished` early. It clears at the next end, interrupt or exit. A watcher hears late, and that terminal gets context in place of wakes until then |
| Eight terminals never measured | About 7 threads each, plus the bridge. Only the visible tab polls |
| Token refresh with terminals sharing a login | Unmeasured. No design change |
| A Codex turn log format change | Versions are pinned, and new ones open on evidence |

### 9.5 Expected later fixes

Accepted for the first release. None holds it up.

- The untested routes in L1.
- Codex interrupt detection (A44), and `needs-user` for Codex (A15).
- Nested subagents in `finished`, and background work on Codex (L8). Codex's `task_started` and `task_complete` rollout records may help. Not checked.
- `codex queue` or hook wakes in place of typing, if L2 or L5 cause trouble.
- Draft guards and resume, if wanted after trying it.
- Usage metering, or limits on unattended loops (A16, A46).
- A way past unverified versions (A32).
- Behaviour at eight live terminals.
- Self-started Claude turns closing a delivery early. The design handles this where it can.
- A rule for `finished` when background work runs for a long time or never ends (L10).
- Letting `watch` choose which events wake, if `began` proves noisy (L11).

### 9.6 Open points

No open points remain. All nineteen raised during writing are closed:

| Id | Point | Settled in |
| - | - | - |
| O1 | `input-messages` positions after shrinking | Sections 5.8 and 5.9 |
| O2 | Wire payloads and `list` | Section 6.3 |
| O3 | Wake text and inbox render | Section 6.7 |
| O4 | Refusals at the edges | Sections 6.2 and 6.4 |
| O5 | `hooks/list` on a live profile | Sections 5.10 and 8.2 |
| O6 | `unmatched` clock and `bridge_off` scope | Sections 5.12 and 6.2 |
| O7 | Helper after a rollback | Sections 5.14 and 8.4 |
| O8 | Requirements with no test | AC10 to AC14 |
| O9 | Manual Send to an unverified terminal | A32 and A7, section 9.3 |
| O10 | Send to a verified Codex terminal whose hook hash can't be read | A32, section 9.3, and section 5.16 |
| O11 | When a drain commits | Section 5.12, and L12 |
| O12 | Bridge state when the project changes | Section 5.12 |
| O13 | `--ask` sender attempt, and notices when an attempt ends | Section 5.12 |
| O14 | Sends that may have reached the target | Sections 5.12, 6.3 and 6.4 |
| O15 | Which prompt closes with no evidence | Section 5.8 |
| O16 | Late `notify` after the `task_complete` fallback | Section 5.8, Turn identity |
| O17 | Overlapping drains | Section 5.12, Drain reservation |
| O18 | How long a wake is in flight | Section 5.12 |
| O19 | A `turn_id` mismatch between hook and `notify` | Sections 5.8 and 8.2 |

---

## 10. Decision log

The user's overall direction, word for word: "go for all your assumptions, we should work in steps, let's try the first release and then we fix things as we go."

"Not recorded" means the choice stands and its other options were not kept.

### 10.1 User decisions

| Id | Decision | Alternatives rejected | Why |
| - | - | - | - |
| Q1 | Claude and Codex only | Any other CLI | Deferred |
| A2 | Own project only | Reach across projects | One project at a time |
| Q2 | "Use them freely" means prompts plus keys, menus, and opening and closing terminals | Not recorded | Open is add, close is Stop |
| Q3 | No approval, always on | A click per message, or a standing permission | The user wants agents to act freely. Always on means on verified versions |
| Q4 | Wake watchers only | Not recorded | R5 |
| Q5 | Events are turn-level plus exit | Not recorded | R4 |
| Q6 | No limit on agents waking each other | Not recorded | R7, A16, L11 |
| Q7 | Real runs allowed: "You can go for real simple runs" | Not recorded | Bounded by A10 |
| Q8 | Removing the sandbox also drops the Codex setting `features.unified_exec_tty=false` | Also passing through the user's own environment and MCP servers | Its reason goes with the sandbox |
| Q8a | Work on `main`: "No, that automation attempt failed, we should work on main" | Merging `automations-attempt` (this reversed an earlier choice to start from it) | The attempt failed |
| Q8b | Keep refusing the home folder as a project | Dropping the rule with the sandbox crate | R10 |
| Q9 | Superseded by Q16 | | `main` has no Supervised mode |
| A32, A7 | Corrected after the user accepted them: Send to an unverified terminal stays denied, as on `main` | Changing the Gate to allow it | Nothing in the request or answers asks for that, and A7 is about keeping Send as it is. The user has not seen this correction |
| Q15 | Sandbox removal is the first step of this work | Not recorded | "claude and codex should run freely with --dangerously-skip-permissions and --yolo mode to have the ability to use the laptop to full extent" |
| Q16 | Leave Supervised mode out | Keeping it | R9 |
| Q17 | Build any number of terminals | Staying at two | R12 |
| Q18 | An agent may close a terminal at any time | Resume after close | R13 |
| Q19 | Connecto's controls are a full brake | Not recorded | R14, R15 |
| Q20 | Idle agents are woken by typing into the terminal | `codex queue`, Claude channels, `asyncRewake`. Channels also show a full-screen dialog on every launch | User's choice. Later fix if L2 or L5 cause trouble |
| Q21 | Take nothing else from the failed branch | The offline test block, draft guards, shared profiles, several projects, automations, Jev | The attempt failed |
| Q22 | Added and closed terminals live in a saved list | Not recorded | R16 |
| Q23 | Text to a busy or not-ready terminal is typed at once | Holding it until the target is idle | R3, L5 |
| Q24 | Cap of 8 terminals | Not recorded | R12, A35 |
| Q25 | Tabs | Not recorded | R17, L6 |
| Q26 | Write fresh | Reusing failed-branch code | A38 |
| Q27 | "Finished" means nothing is left running | Not recorded | R19, L8, L10 |
| Q28 | "The llm can choose to read everything, or can choose to read the result of the thing that it asked the other llm in the other terminal to do. it should be a lean channel to let the llm on different clis talk instead of going throught the claude -p or codex exec" | Not recorded | R2, R18 |
| Option A | Agents reach the bridge through a `connecto` shell command: "A: connecto shell command" | MCP tools: a process per terminal, Codex MCP behaviour nobody has run, capped big reads, and paging for `read`. They can still be added on the same socket | User's choice |

### 10.2 Design decisions

| Decision | Alternatives rejected | Why |
| - | - | - |
| Bridge on its own socket, one per app | A socket per terminal | That adds eight listeners and a token to carry, for no gain in attribution |
| | Hook replies on the hook socket | That would change the proven one-way path, and put reads behind its serial accept loop |
| | Shared folder as a mailbox, read with `cat` | No sender identity, so A39 and the audit lose meaning. Refusals cannot be synchronous |
| | Commands written in the agent's reply, parsed at turn end | Works only at turn end, so an agent cannot read and continue within a turn |
| Agent text has its own path in the actor | The relay's `Open` machine | One open delivery per target blocks "typed at once" |
| Terminals stay as they are, driven through their PTY | Codex through `app-server` and `--remote` | A second client is refused (`thread <id> already has an active writer`), and `--remote` stopped at a saved trust decision. ADR 0001 rejected it |
| Bridge covers both tools the same way | Claude `SendMessage` | R15 turns it off, and it never reaches Codex |
| Codex hooks added, with trust through `hooks.state` | Next-prompt context without Codex hooks | No other route adds context to a Codex prompt |
| Roster in the first `UserPromptSubmit` drain | Roster in `SessionStart` context | That is a second context path |
| Helper refreshes at launch | A helper-only `provision` flag | It works, but a forgotten step fails quietly: no context and no `connecto` command |
| Every R4 event wakes a not-busy watcher, `began` included | `began` never wakes | The original request names "started" as an event. The worry that a wake could set off another applies equally to `finished`, and Q6 accepted it |
| Unread-notice wake | Leaving notices for a next prompt | An agent that goes idle may have no next prompt, so a watcher or `--ask` sender could never hear what it asked for |
| Reply pairing uses evidence, with no timeout | Pairing by time | A silent queued prompt can last minutes, so time proves nothing |
| No timeout for long background work | A rule ending `finished` after a time | Follows Q27. Listed as a later fix |
| R15 ships in step 1 | R15 with the bridge | Sandbox removal reopens Claude's messaging until R15 lands |
| A shrunk `notify` carries `connecto_input_total`, keeping positions absolute | Disabling input-based pairing when entries are dropped | Retained entries stay usable as evidence, and any lost ones fail towards `unmatched` |
| `hooks/list` runs under one lock and is cached, failures included | Retrying a failed read at each start | A terminal launched without hooks is live, and a retry would read beside it |
| The `unmatched` clock runs from the latest of Enter, a failed paste, a `timeout` or the last `LeftWorking`, only while not busy | Counting only from the target stopping being busy | A target whose menu swallowed the text never stops being busy, so an `--ask` sender would hear nothing |
| A bridge-off target still allows `list`, `read`, `result`, `unwatch`, `start`, `close` and `add` | Refusing every operation | These depend on no tool behaviour, or are process control |
| `close` on your own terminal replies first, and stops 250 ms later | Stopping at once | The client dies with the terminal, so it could never get a reply |
| A later drain with an empty inbox prints nothing | Printing the roster in every drain | A quiet session costs no tokens |
| Add and remove write the list first; changes of `desired` never wait on the disk | Waiting on the disk for every change | Stop must always work (R14), and a failed add must not give out an id |
| What decides a request travels in a lossless outbox | One lossy channel for everything | A lost message must never leave a request pending or a wake in flight |
| The helper keeps every older form working | Requiring `--event` | An older build runs the new helper after a rollback |
| A drain commits when its reply is written to the socket | An acknowledgement from the helper | It could only prove the helper printed, not that the tool took the text, and a lost acknowledgement would deliver twice |
| Results are partitioned by project | Clearing them on a project change | Returning to a project finds its results, so A42 holds, and another project's id reveals nothing |
| An `--ask` notice belongs to the attempt that asked | Not recorded | A restart is a new conversation (A12) |
| The paste decides whether a request record is kept | Not recorded | Text that reached the target can still be submitted, so its id must stay usable |
| With no evidence, the oldest open prompt closes | Not recorded | Both tools run separate prompts in order |
| A Codex turn is its `turn_id`, and the first end wins | Not recorded | Two sources can report one end, and a late one must not close a newer prompt |
| An end never waits on a `turn_id` match | Closing a prompt only on a match | Nothing confirms the ids are equal, and a terminal must not stay `working` if they differ |
| A drain moves notices into a reservation | Marking them in the inbox | Bridge state is one thread but handlers write, so two drains could return the same notice |
| A wake stays in flight through its stages until its outcome | A fixed 3 s window from dispatch | A wake can wait about 15 s in the queue, so later events could queue more wakes |
| Phase is published by each actor | Deriving it in bridge state from events | A lost event could leave a terminal `Working` for ever |
| `read` decides on the session | Checking the stage | A login starting at that moment could slip through a stage check |
| Bridge state alone decides a request's final state, and the first wins | Not recorded | `unmatched` at 15 s and a late bind cannot disagree |


**Project settings that defeat the launch (added after the multi-agent run).** `--settings` hooks cannot override a project's `disableAllHooks: true`, and `permissions.disableBypassPermissionsMode: "disable"` defeats `--dangerously-skip-permissions`. Neither can be prevented from the command line (checked on Claude 2.1.318 without a model call), so both are detected at launch from the project's `.claude/settings.json` and `settings.local.json`. Hooks disabled: the terminal's bridge is off with a reason naming the setting (`list`, `bridge_off_reason()`). Bypass disabled: the pane carries a warning (`list` `warning`, `power_warning()`, pane warnings) that it is not running with full power. A project `env.CONNECTO_BRIDGE_SOCK` (or the other bridge variables) is prevented: the launch settings carry the same variables in `env` and outrank project and local `env`. Replies and other terminals' text are quoted after every kind of line end (CR, NEL, U+2028 and U+2029, VT, FF) becomes a newline, so a lone CR cannot leave the quote.
