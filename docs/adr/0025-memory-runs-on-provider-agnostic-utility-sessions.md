# 0025: Memory runs on provider-agnostic utility sessions

Status: Accepted (2026-11-14, user decision on spike S2). Extends ADR 0001; does
not change it. Spikes S1 and S2 of `docs/specs/project-memory.md` §9; raw
numbers in `docs/specs/project-memory.work/spikes.md`.

## Context

The development and dream cycles of project memory (spec §5.4, §5.5) and the
bootstrap (§5.9) need a model call: a prompt in, a text reply out. ADR 0001 keeps
both CLIs as TUIs in PTYs: no `claude -p`, no `codex exec`, no MCP, no
app-server. The utility runner (`connecto-session/src/utility.rs`) already runs
a hidden Claude session in a PTY with `--tools ""`, `--setting-sources user`,
`--strict-mcp-config`, no bypass flag and no bridge, in the project folder,
pastes a prompt, and takes the reply from the `Stop` hook. It only makes
terminal titles today, and only for Claude.

The first plan required a memory run to have no tools at all, enforced by the
launch, and made Codex depend on proving such a launch. Spike S2 showed Codex
0.167.1 cannot start with an empty tool set (below). The user then decided that
there is no tool-free requirement.

## Decision

1. All LLM work Connecto does for itself (titles, the memory cycles, future
   uses) goes through Connecto utility sessions. A utility session is a normal
   agent CLI session with a precise goal, hidden in a PTY, not a registry
   terminal, with no bridge. It may read project files when the goal needs it.
2. Utility sessions are provider-agnostic: each CLI adapter offers a utility
   launch (model, effort, read-only hardening, turn end). Claude and Codex are
   both working providers for both memory cycles in this release; a future CLI
   plugs in the same way. `capability()` reports a provider unavailable only for
   real causes (CLI not installed, not logged in), with the reason.
3. The working folder is the project folder, as for titles, with the project's
   own CLI profile. No separate memory work folder.
4. Protection:
   - The memory is protected by the host: the model only proposes structured
     operations; the host validates and publishes them (spec §5.7). No utility
     session writes to the store.
   - Each adapter launches the session read-only as far as the CLI allows while
     it can still read files. Claude: `--tools "Read,Grep,Glob"`, no edit, write
     or shell tool, no bypass or permission-mode flag. Codex: `--sandbox
     read-only`, `--ask-for-approval never`, `-c mcp_servers={}`,
     `-c web_search="disabled"`. This is best-effort hardening per adapter, not a
     condition to be a provider. An adapter test attests each adapter's launch
     and the absence of a bridge.
   - No bridge, not in the registry, no events into memory capture: a utility
     session cannot call `connecto memory` and cannot feed its own output back.
   - The prompt states the goal, that the project must not be modified and no
     tests or commands run, and that the session should work from the packet and
     not open paths the user excluded. These are instructions, not guarantees.
5. Trust. Running in the project folder does not mean the CLI trusts it: the
   bootstrap can start before any normal session of that CLI was opened in the
   project, and S1 measured a hidden session blocking on Claude's trust screen.
   The runner detects a CLI's trust screen from the screen text, before it types,
   and ends the run at once with a typed, user-visible reason: "Claude has not
   been trusted for this project folder yet: open a Claude terminal in the
   project once and accept" (the same for Codex). Such a run is **held**
   (`RunOutcome::Held`), not failed: it does not use up the batch's attempts and
   is not retried in a loop. The reason shows in `connecto memory status` and in
   the UI. The work is tried again later: a development batch after 15 minutes,
   or a minute after a terminal of the project is used again; a scheduled dream
   after 5 minutes; an initial build after 15 minutes. A held "Dream now" is not
   retried by itself.
   Connecto never answers the trust prompt for the user and never edits the CLI's
   configuration to mark a folder trusted.
6. One fresh session per run (per part of a development batch, per part of the
   dream, per packet of the initial build), closed after the reply. No session
   is reused. At most two memory runs are in flight across the app.
7. Arrival check: the `UserPromptSubmit` hook carries at most `MAX_EVENT_TEXT`
   (64 KiB) of the prompt. Longer prompts arrive whole (measured to 300,000
   characters); the runner proves it from the session log, whose user record
   holds the full text inside Claude's `<pasted_content>` wrapper (Codex: the
   user message in its log, or the `notify` input when not shrunk). For memory
   runs the check is exact: the arrived text must equal the sent one, except for
   whitespace (titles keep the looser `arrived_whole`). When the runner cannot
   tell whether a prompt was taken, it does not type it again.
8. Deadlines per job, not the fixed 60 s of titles: 120 s for a development
   run, 600 s for a dream session, 15 minutes for an initial-build session,
   always within the cycle's own time limit (S1: model time 1 to 4 s for inputs up
   to 300 K characters, plus about 1.3 ms per output character on haiku; one run
   had no `Stop` for 25 min during a network outage).
9. Reply: from the turn-end hook (Claude `Stop`, up to 256 KiB), else from the
   session log, which is written a few milliseconds to about 120 ms after the
   hook; the runner retries the read for up to 2 s. For memory runs the session
   log is always read, and the reply is accepted only from a turn the log marks
   complete (Claude `stop_reason` `end_turn` or `stop_sequence`, so a turn cut at
   the token limit is refused; Codex `task_complete`). A reply that starts with
   `API Error:`, a log error record or an aborted turn is an error, not model
   output. Codex's `notify` fires twice per prompt (first for its own
   title-generation turn): the runner matches the turn it submitted.

## The trade-off accepted

Spike S2 measured that Codex has no launch with an empty tool set. With
`-s read-only -a never` the model could run `printf hi > probe.txt`; the Seatbelt
sandbox refused the write, but the command ran, and about a hundred remote app
tools (`mcp__codex_apps__*`, acting on ChatGPT-side state) and
`mcp__codex_tui__create_worktree` were still offered. Turning off 21 features
still left `functions.exec`, `request_user_input` and the agent-spawning tools.
Claude can drop its edit, write and shell tools, but a reading session still
reads anything the user can.

So "a memory run does not modify the project" is enforced by the launch only as
far as each CLI allows: by the OS sandbox for local writes under Codex, by the
tool list under Claude, and **for a CLI without a read-only mode, by the prompt
alone.** What stays guaranteed regardless of provider is the memory itself:
nothing reaches the store except operations the host validated.

Redaction and exclusions hold on two different sides, and only on one of them:

- **(a) What the host controls.** The packets the program assembles for a run,
  and everything the memory stores, publishes, exports or serves through
  `connecto memory`: these are redacted by the host (`connecto_redact::redact`
  with the known secrets) and respect the user's exclusions. The model's reply is
  redacted before it is stored.
- **(b) What a utility session does on its own.** A session that reads a project
  file with its own tools sends the raw content to the model, and the CLI writes
  the raw tool output into its own session log in the provider's profile, before
  and outside any host redaction. Bootstrap exclusions do not bind those reads.
  This is exactly what a normal terminal session of that CLI in the project can
  do, and it is an **accepted limit**. The prompt asks the session to prefer the
  packet and not to open excluded paths; that is an instruction, not a guarantee.

## Consequences

- Each run costs about 3.2 s of wall time for a trivial prompt on Claude (0.3 s
  to the session, 0.65 s to the composer, 1.25 s paste and settle, about 1.2 s
  model) plus about 17,000 input tokens of Claude Code's own system prompt and
  the user's instructions, cached. Codex was not timed.
- `--setting-sources user` loads the user's own `CLAUDE.md` and rules into every
  memory run; the prompt must say the memory task wins.
- Transcripts of memory runs go to the profile's session folders under the
  project's slug, next to the user's own sessions. They hold the redacted packet
  and, raw, anything the session read with its own tools (see the trade-off).
  Export and delete of memory do not touch them; the delete confirmation in the
  app and the user documentation (`docs/project-memory.md`) say so.
- The refused-folder and wrong-profile problems a separate work folder had (S1)
  do not arise. The trust screen still can, in a project the CLI has never been
  trusted in; it is handled as a held run (decision 5).
- Titles move to the same per-adapter launch; their behaviour does not change,
  except that, by the user's decision of 2026-11-14 that every provider serves
  every utility feature, a title can now run on Codex (`UtilityModel::Codex`).

**Reverse if** a memory run is seen to modify a project, or a CLI adapter cannot
offer a read-only launch and the user wants that provider withheld.
