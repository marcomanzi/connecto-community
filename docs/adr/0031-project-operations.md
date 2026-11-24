# 0031: Project operations are approved scripts in the project, matched to prompts locally first and by Jev second

Status: Accepted (2026-11-21). Spec: `docs/specs/project-operations.md`.
Decided overnight while the person was away, on the defaults they had agreed
to (operations under `.connecto/ops` out of git; Suggest by default, Intercept
per operation). The smaller defaults below were chosen as the safe option and
are listed so they can be revisited.

## Context

Routine requests (start the server, commit, deploy) each cost a full agent
turn. A script costs nothing; a script with one call to a small model costs
little. Connecto already has the pieces: agent terminals with a first prompt,
utility sessions (ADR 0025), a prompt hook that adds context (ADR 0016), Jev
for cheap classification (ADR 0021), shell terminals (ADR 0019) and a file
kept in the project out of git (ADR 0029).

## Decision

1. **Files in the project, state in the data folder.** An operation is
   `.connecto/ops/<slug>/op.json` plus `run.sh` or `recipe.md`, written by
   an agent or a person. `.connecto/ops/` is added to `info/exclude`.
   Approvals, the Intercept switches, settings, runs and the prompt log live
   in `<data>/ops/<project id>/`, outside the repository and outside every
   interface an agent is given (`connecto op` has no verb that approves or
   switches anything on).
2. **Approval is a hash of what the person saw.** The dialog sends the hash
   of the files it showed (the whole script, never a cut) with Approve and
   with Run; the app refuses when the files on disk no longer have it. Any
   later change withdraws the approval until the person approves again.
3. **The command runs the script, the app decides.** `connecto op run` asks
   the app, which checks the approval and answers with what to run; the
   command runs it in the caller's own terminal so the output is where the
   caller is. This bends ADR 0016's "the command has no logic" by one step
   (spawn and wait), which is the only way the output reaches an agent's tool
   result without a pipe through the app.
4. **Matching costs nothing until it is likely.** A local word match on the
   operation's phrases comes first; Jev is asked only about the candidates,
   within 3 s. Without a Jev key, only a strong local match (0.85) suggests,
   and nothing is ever intercepted.
5. **Suggest is text with the slug only.** Like memory (spec D6), the
   `[connecto]` line holds Connecto's own words and the slug, never text from
   the operation's files.
6. **Intercept is narrow.** Only Claude (its `UserPromptSubmit` can block;
   Codex 0.167 blocks only in `PreToolUse`, Pi and Antigravity cannot), only
   operations the person switched on, that do not ask first, at Jev
   confidence 0.85 and for prompts of 12 words or fewer.
7. **Discovery is a visible agent terminal**, started only by the person (or
   by their click on the weekly suggestion), with the person's prompts
   gathered into a digest first, so it reads a few KB instead of whole
   transcripts.
8. **No scheduler.** The weekly suggestion is computed when the view is read
   (seven days and 20 prompts since the last discovery); nothing runs by
   itself.

## Defaults chosen (safe side)

| Setting | Default | Why |
|---|---|---|
| Hook mode | Suggest | Agreed. Suggest adds one line; it cannot do harm. |
| Intercept | Off for every operation | It skips the model; the person opts in per operation. |
| `deploys` / `deletes` | Always ask, whatever `confirm` says | Agreed. |
| Intercept limits | Jev ≥ 0.85, ≤ 12 words, Claude only | A long prompt is rarely only the operation. |
| Local candidate / suggest-without-Jev | 0.6 / 0.85 | Few false suggestions without Jev. |
| Jev wait in the hook | 3 s, then nothing | A prompt must not wait on a slow network. |
| Weekly suggestion | On, 7 days and 20 prompts, Later = 3 days | A suggestion only; nothing starts by itself. |
| Discovery agent | Claude, else Codex; at most 8 proposals | Bounded output to review. |
| Script limits | 64 KiB, timeout 600 s (max 3600) | Bounded. |
| `connecto op llm` | Haiku, no tools, 120 s, 32 KiB prompt | Cheapest Claude; mirrors titles. |
| Prompt log | Local only, 1000 prompts, 400 chars each, no `[connecto` lines | Enough for discovery, small. |
| Windows | `run` refused (needs `/bin/bash`) | Not built for Windows yet. |

## Consequences

- **Trust boundary.** Agents run as the same user as Connecto with no
  sandbox (ADR 0013). An agent that sets out to can edit
  `<data>/ops/…/state.json` directly and mark an operation approved or
  switch Intercept on, just as it can edit any of the person's files or run
  any command. Approval therefore does not defend against a hostile agent;
  it would gain nothing it does not already have (Intercept only runs, as
  the person, a script the agent could run itself). What approval does
  guarantee: Connecto never runs, from the dialog or the hook, files that
  changed since the person reviewed them through Connecto, and no agent
  gets an approval through any interface Connecto gives it. A boundary
  against same-user agents would need the sandbox back and is out of scope.
- Files are read, deleted and written relative to folders already open,
  never following a link (`openat`, `unlinkat`, `renameat` with
  `O_NOFOLLOW`, through `nix`, no `unsafe`): from the project folder down,
  `.connecto`, `ops` and the operation's folder must be real folders and its
  files regular files, so swapping one for a link at any moment cannot send
  a read, a deletion or the digest outside the project. Windows keeps the
  path checks (it does not run scripts).
- A script runs as its own process group, which gets the terminal while it
  runs (Ctrl-C and typed input reach it); the signals `connecto` receives are
  passed on to it; at its timeout the whole group gets TERM, then KILL two
  seconds later (a process it moved to a session of its own is left
  alone). Anything it left running after a normal end keeps running (a
  server started in the background).
- Every run Connecto starts for the person (the dialog, an intercept, a
  recipe) is a ticket: the files' hash, the arguments, where it came from
  and the terminal that may claim it, for two minutes and once, all checked
  again at the claim. Runs are written as `running` before the script is
  handed out and finished by a later line, so one that never reports, or
  outlives the app, shows as `not finished`.
- Intercept is a handshake in three steps, so a prompt is never both sent
  to the agent and run. The drain says the hook can take one (`blocks_on_hook_failure`; an
  older hook cannot and gets a suggestion). The app holds the decision and
  answers with a ticket. The hook confirms (`op intercept`): the app checks
  everything again and opens a shell, ready, with nothing typed; any
  failure here leaves the prompt to the agent (the app gives the shell
  8 s, inside the hook's 10 s wait). The hook prints the block, then says
  go (`op go`); the app answers go at once and types the claim from a
  thread. Every step has its own limit (6 s for the drain, 10 s for the
  confirmation, 2 s for go, and go is not sent at all more than 25 s
  after the helper process started: the clock starts at its entry, the
  connection for go waits only until then, and the deadline is checked
  again just before the write that starts the run), and the adapter sets
  the prompt hook's timeout to 60 s explicitly. The helper therefore exits long before Claude would
  time it out and drop the block it printed, which would leave the prompt
  with the agent while the operation runs. If the hook stops between the
  block and go, or the shell cannot take the line, nothing runs. Connecto
  cannot tell whether Claude kept the block, so the Operations dialog says
  only that the operation did not start and to check the terminal before
  asking again (a maintenance tick settles prepared shells and holds after
  10 s, even when no prompt follows). What remains is Claude dropping a
  block from a hook that exited normally within its timeout, or a machine
  so stalled that more than 30 s pass between the helper's last check and
  Claude reading its output. Both are closed by the gate: the drain says
  it can hold a prompt only from a helper registered with `--holds`, which
  the Claude adapter writes, together with `"onFailure": "block"` on the
  `UserPromptSubmit` hook, only for Claude 2.1.295 or later (checked in
  the 2.1.328 binary: `UserPromptSubmit` honours it; it is ignored only on
  Setup, SessionStart, Stop, SubagentStop, TaskCompleted and TeammateIdle)
  and only in a project that had `.connecto/ops/` at launch. Claude then
  blocks the prompt itself when the helper fails or times out. The cost:
  in such a project, a broken helper blocks every prompt until it is
  fixed, so it is limited to projects with operations. An older or unknown
  Claude, or a session started before the project had operations, gets
  Suggest. With a Jev key set, a
  Jev failure or timeout adds nothing. A Pause refuses the confirmation and
  the go.
- **From a paired machine (2026-11-22).** A remote viewer's approval counts
  the same as one made on the host (the person's decision): a paired peer
  already has full access to the account (ADR 0017), so this adds no power,
  only the review. The peer is sent both files whole and approves the hash
  it computes over them; the host checks it against its files as it checks
  the dialog's, and records the action through the remote guard (ADR 0017,
  "Operations").
- Windows has no folder handles here: deleting an operation and writing
  the discovery digest are refused there (and scripts do not run).
- An approved operation can still do anything its script does: approval is
  the person's review, not a sandbox. `danger` is the agent's own label;
  Connecto forces asking for `deploys` and `deletes` but cannot verify the
  label.
- `--yes` on an operation that asks first is an instruction to agents, not a
  lock: an agent can run any command anyway.
- No token saving is promised. Runs and intercepts are counted so the effect
  can be seen; the memory benchmark is a reminder that it may be small.
