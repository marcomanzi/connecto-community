# Project operations

Written 2026-11-21. Design decisions and their reasons are in ADR 0031.

## 1. What it is for

People ask a project's agents for the same things again and again: start the
server, run the tests, commit, deploy, update the dependencies. Each such
request costs a full agent turn: the model reads its context, works out the
steps again and runs them. A **project operation** is that request written
down once, as a script, so it costs no model at all, or as a script that asks
a small, cheap model for the one part that needs judgement (a commit
message), or, when it cannot be scripted, as a short recipe the agent follows
instead of working the steps out again.

Operations come from three places:

1. **Discovery.** On demand from Project Settings > Operations, and suggested
   once a week, Connecto opens an agent terminal in the project with a fixed
   goal: read what the person asked for in this project, find the requests
   that repeat, and write a proposed operation for each.
2. **By hand.** A person or an agent writes the files (section 3).
3. Nothing else. Connecto never writes an operation's script itself.

An operation runs only after the person approved it, and only as approved: a
change to its files withdraws the approval (section 4).

## 2. Where it shows

- **Project row.** A new icon (a lightning bolt) left of the gear, on the
  active or hovered project row, when the project has at least one operation
  or proposal. It opens the **Operations** dialog: each approved operation
  with its description and a Run button, and the proposals waiting, with
  Review. A dot on the icon means proposals are waiting or a discovery is
  suggested.
- **Project Settings > Operations** (the gear). Find common operations (the
  discovery), the list of operations with their state, their files and their
  runs, Approve / Withdraw / Delete, the per-operation *Run without asking
  the agent* switch (Intercept), and the project's settings: hook mode (Off,
  Suggest) and the weekly suggestion (on, off).
- **Agents.** `connecto op list|show|run|llm` on the bridge (section 5), one
  launch paragraph when the project has approved operations, and a
  `[connecto]` line with a prompt that matches one (section 6).

## 3. Files

Each operation is a folder in the project:

```
.connecto/ops/<slug>/op.json
.connecto/ops/<slug>/run.sh       (kind script or script+llm)
.connecto/ops/<slug>/recipe.md    (kind recipe)
```

`<slug>` is 1 to 40 characters of `a-z`, `0-9` and `-`, starting with a letter
or digit. The folder `.connecto/ops/` is kept out of git: Connecto adds the
project-scoped pattern `/<git rev-parse --show-prefix>.connecto/ops/` to the
file `git rev-parse --git-path info/exclude` names (as the task board does,
ADR 0029) the first time it reads or prepares the folder. A person who wants
the operations shared removes that line and commits the folder; approvals
stay per machine either way.

`op.json` (unknown fields are an error, so a typo is not silently ignored):

```json
{
  "version": 1,
  "name": "Deploy",
  "description": "Builds the site and deploys it to production.",
  "phrases": ["deploy", "ship it to production", "push the site live"],
  "kind": "script",
  "danger": "deploys",
  "confirm": true,
  "args": [{ "name": "target", "description": "staging or production", "required": false }],
  "timeout_s": 900
}
```

- `name`: 1 to 60 characters, one line. `description`: 1 to 300 characters.
- `phrases`: 1 to 12 ways a person asks for it, 2 to 80 characters each. Used
  by the hook's local match (section 6).
- `kind`: `script`, `script+llm` or `recipe`. A `script+llm` script calls
  `connecto op llm` for its model step; the kind tells the person a model is
  involved.
- `danger`: `none` (reads only, such as running tests), `changes` (changes
  the working tree or local state, such as formatting or a local commit),
  `deploys` (reaches anything outside this machine: a push, a deploy, a
  publish, a message) or `deletes` (removes data that is not easily
  recovered).
- `confirm`: the operation asks the person before it runs. It is always true
  in effect for `deploys` and `deletes`, whatever the file says.
- `args`: 0 to 6 named positional arguments, passed to the script in order.
- `timeout_s`: 1 to 3600, default 600. The run is stopped after it.

`run.sh` is run with `/bin/bash` from the project folder, with
`CONNECTO_OP=<slug>` and `CONNECTO_PROJECT=<project folder>` set. At most
64 KiB. `recipe.md` is plain text the agent is handed, at most 8 KiB.

## 4. Approval and Connecto's own state

Connecto's state for a project lives in its data folder, never in the
repository: `<data>/ops/<project id>/state.json`, `runs.jsonl` and
`prompts.jsonl`.

- **Approval** is the SHA-256 of `op.json`, a zero byte, and the script or
  recipe. The dialog shows the whole script and sends the hash it showed with
  Approve and Run; a mismatch is refused (review again). The operation is *approved* while the files hash to the recorded
  value; *changed* when they do not (it does not run until approved again);
  *proposed* when no approval was ever recorded; *invalid* when its files do
  not pass the checks (the reason is shown).
- **Intercept** is kept per operation in `state.json`, set only by the person
  in the UI, and never read from `op.json`: an agent cannot turn it on.
- **Settings:** `hook` (`suggest` or `off`, default `suggest`), `weekly`
  (default on), the time of the last discovery and the snooze.
- **Runs** (`runs.jsonl`, last 500): slug, start, duration, exit code or the
  reason it did not finish, and who started it (`dialog`, `agent`,
  `intercept`).
- **Prompts** (`prompts.jsonl`, last 1000, each cut to 400 characters): the
  person's prompts in this project's terminals, as the hook saw them, with a
  count of which matched an operation. Prompts written by Connecto or by
  another agent (they start with `[connecto`) are not kept. They feed the
  discovery and the weekly suggestion. The log stays on this machine; but
  with a Jev key set, a prompt that matches an operation locally is sent to
  Jev together with the candidates' names, descriptions and phrases (§6).
- **Who can change it.** These files are outside the repository and no
  `connecto` command changes them, but agents run as the same user with no
  sandbox: one that sets out to can edit them, as it can any of the
  person's files (ADR 0031, trust boundary).

## 5. The `connecto op` command

```
connecto op list                          the approved operations of this terminal's project
connecto op show <slug>                   one operation: what it does, its arguments, its script
connecto op run <slug> [args...] [--yes]  run an approved operation here
connecto op llm [--model haiku|sonnet] <<'EOF'
                                          ask a small model; prints its reply
```

- The project is the caller terminal's project, from the process tree.
- `run` of a `script` or `script+llm` operation: the app checks that the
  operation is approved and unchanged and answers with what to run; the
  `connecto` command then runs `/bin/bash run.sh args...` itself, in the
  calling terminal, with its output on the caller's own screen (an agent sees
  it as its tool output), stops it and everything it started at
  `timeout_s`, and reports the exit code to the app (only that terminal
  can report it). `run` of a `recipe` prints the recipe for the agent to follow.
- An operation that asks first is refused without `--yes`. The reply tells
  the agent to ask the person and to run it again with `--yes` once they
  agreed. This is an instruction, not a lock: an agent can run any command
  anyway. Connecto itself never runs such an operation without a person's
  click (section 7).
- `llm` runs one utility session (ADR 0025) with no tools in the project
  folder: prompt on stdin (at most 32 KiB), reply on stdout, 120 s at most,
  Haiku by default. It is how a `script+llm` operation gets its message.
- Refused under Pause, like every writing command, except `list` and `show`.

## 6. The hook: Suggest and Intercept

The prompt hook (`UserPromptSubmit`, and the equivalents for Pi and
Antigravity) now hands the prompt to the drain. With hook mode `suggest` and
at least one approved operation in the project:

1. **Local match, free.** The prompt and each phrase are lowercased and split
   into words; a phrase scores the share of its words found in the prompt
   (a word matches a word that shares its first 5 letters, or is equal when
   shorter). Prompts longer than 60 words are skipped. An operation is a
   candidate at a score of 0.6 or more. Most prompts stop here.
2. **Jev, only on a candidate**, when the Jev key is set (Settings >
   Automations): one `choice` question with each candidate and `none`, the
   prompt as the state, within 3 seconds.
3. **What happens:**
   - Jev picked an operation (or, without Jev, the local score was 0.85 or
     more): the hook adds `[connecto] This request may match the project
     operation \`<slug>\`...`, telling the agent to check it with `connecto
     op show <slug>` and to run it with `connecto op run <slug>` instead of
     doing the steps itself. Only the slug appears, never text from the
     operation's files.
   - **Intercept** instead of Suggest when all of these hold: the person
     turned Intercept on for that operation; it does not ask first
     (`confirm` false, `danger` `none` or `changes`); it is approved and
     unchanged; Jev chose it with a confidence of 0.85 or more; the prompt
     is at most 12 words; the terminal is Claude 2.1.295 or later, launched
     when the project already had `.connecto/ops/` (its prompt hook is then
     registered with `onFailure: "block"` and `--holds`, so Claude blocks
     the prompt itself if the hook fails or times out; any other session
     gets Suggest). Connecto holds the decision and answers the hook with a
     ticket. The hook confirms it with `connecto op intercept <ticket>`: the
     app checks everything again and opens a ready shell, typing nothing
     (a failure leaves the prompt to the agent). The hook prints the block
     (Claude's `decision: block`) and then sends `connecto op go <ticket>`:
     the app answers at once and types the run. Each step has a short
     limit (10 s for the confirmation, 2 s for go, no go past 25 s), well
     inside the 60 s timeout Connecto sets on Claude's prompt hook, so
     Claude never drops a printed block for a slow hook. If go never comes
     or the shell cannot take it, nothing runs, and the Operations dialog
     says the operation did not start and to check the terminal before
     asking again (Connecto cannot see whether Claude kept the block).
     Holds and prepared shells expire after 10 s. Notices waiting for the
     agent stay for its next prompt.
   - Jev set up but failing or late: nothing is added (the local match
     alone is only for a project without a Jev key).
   - Jev said `none`, or the score was below the bar: nothing is added.
4. Codex, Pi and Antigravity get Suggest only: their prompt hooks cannot stop
   a prompt (Codex 0.167 blocks only in `PreToolUse`), so Intercept is never
   tried there.

Every match, suggestion and intercept is counted in `prompts.jsonl`.

## 7. Running from the dialog

Run asks for the operation's arguments (if it has any) and, when it asks
first, for a yes; then it opens a shell terminal in the project, named
`op <slug>`, selects it and types `connecto op run <slug> --ticket <ticket>`
once the shell is ready. The ticket stands for that run: the files' hash the
person saw, the arguments, where it came from, and the terminal that may
claim it, within two minutes and once; everything is checked again when it
is claimed. An intercept and a recipe run the same way.
The output is on screen; the terminal stays open when the run fails, so the
person can read why, and has a Fix with an agent button in the dialog that
opens the project's agent terminal with a short note about the failure.
An operation that asks first shows a confirmation in the dialog before that.
A `recipe` operation opens an agent terminal of the project's first agent
tool with the recipe as its first prompt.

## 8. Discovery

Find common operations (Project Settings > Operations, or the weekly
suggestion) does this:

1. Connecto writes a digest to `.connecto/ops/.discovery/prompts.md`: the
   person's prompts from `prompts.jsonl` and from the project's closed
   terminals (the same reader as memory's initial build, which only takes
   messages recognised as typed by a person), from the last 90 days, at most
   200 KiB. The same words within two minutes count once (one prompt found
   in both logs); exact repeats are grouped with their count, the most
   repeated first, then the most recent.
2. It opens a Claude terminal (Codex when the project has no Claude) named
   `Find operations`, with the discovery goal as its first prompt: the file
   format of section 3, the digest's path, what to look at in the repository
   (README, package scripts, Makefile, CI, deploy configuration), and these
   rules: propose only requests seen at least twice or clearly routine; at
   most 8 per run; never overwrite an existing operation (write a new slug,
   or skip); a script must be safe to run twice, must fail loudly (`set
   -euo pipefail`), must not contain a secret (use `connecto secret run`),
   and is tried only if its `danger` is `none`; end with a short table of
   what it proposed and why.
3. The proposals show in Project Settings > Operations for the person to
   review.

**Weekly suggestion.** With `weekly` on, the project's Operations icon and
tab show a suggestion when seven days have passed since the last discovery
(or since the project was first seen) and at least 20 prompts were recorded
since then. Later hides it for three days; Find starts the discovery. No
discovery ever starts by itself.

## 9. What is measured

For each operation: runs, failures, average duration, and how often the hook
suggested or intercepted it. The tab shows them. No token or cost figure is
shown: the CLIs report none reliably (as for memory), and an avoided agent
turn is an estimate, so the count of turns avoided (intercepts) and of
agent runs (`connecto op run` from an agent) is shown instead.

## 10. Out of scope

- Operations that run on a schedule or on events.
- Shared approvals across machines.
- Intercept on Codex, Pi and Antigravity.
- Windows: `run` needs `/bin/bash`; on Windows it is refused with a reason
  (the rest works).
