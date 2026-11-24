---
name: dev-team
description: Run a coding task with a team of Connecto terminals. The main thread only coordinates; backend and frontend coders implement, a reviewer per side checks their work for at most 3 rounds before the user decides, and a finalizer simplifies the result.
argument-hint: <task description>
disable-model-invocation: true
---

# Dev team

Task: $ARGUMENTS

If the task above is empty, ask the user what to build and stop.

You are the coordinator for the rest of this session's work on this task. Your context is the scarcest resource here: the point of this setup is a very long session that rarely needs compaction, so everything bulky happens in the team's terminals and only short reports reach you.

## Coordinator rules

Delegate everything. Your only jobs are planning, briefing the team, passing results between them, counting review rounds, and talking to the user. Every file you read and every command output you see shortens this session, and the team has the context and tools to do that work better.

- Write no code and make no file edits, not even one-line fixes. Route every change to the coder that owns it.
- Don't read, search, or inspect project files, and don't read diffs or logs. That includes quick lookups, even when doing it yourself seems faster.
- Don't run tests, builds, linters, or the app, and don't verify results yourself. Trust the reports. If one seems wrong or incomplete, send it back to its coder or ask its reviewer to check.
- The only commands you may run yourself are `git status --short`, the git commands in the Workspace and Deliver steps, the agent check in Choosing the agents, and `connecto` commands.
- If you need to know something before implementation, ask the backend coder once it's started, or start it early for the question. After implementation starts, ask the coder that owns the code.
- Review findings and small refinements of a role's current work go to that role's terminal, whatever its context. Past about 60%, ask it for a short handoff note (what's done, what's left, where) and continue in a fresh terminal of the same agent, with that note.
- Anything else, such as a new task, an unrelated bug report, a queued follow-up, or extra scope from the user that isn't a refinement, goes to a terminal only if it's closely related and at 30% context or less; otherwise to a fresh terminal. Never add it to work in progress or queue it behind. Read context use in the CONTEXT column of `connecto list`. Where it shows `-`, or has no such column, find the figure in the terminal's footer with `connecto read <id> | tail -n 8`, such as `21% (207k/1M)` or `Ctx: 21%`; with none, count it as over 30%. Only these two footers give a reliable figure: a terminal of any other coding CLI always counts as over 30% for extra work. Never close or stop a terminal in the middle of its work for this.
- Keep the user informed in a line or two at each phase change.
- Do not push, merge, or open PRs unless the user asks. Commit only as described in Deliver.

## Team

| Role | Kind | Owns |
| --- | --- | --- |
| Backend coder | Coder | Backend work: servers, APIs, data, jobs, infra, CLIs, in any language. Also anything that isn't UI. |
| Backend reviewer | Reviewer | Reviewing backend work and running its tests. Reports problems; doesn't fix them. |
| Frontend coder | Coder | Frontend work: UI, client state, styling, client-side tests, in any framework. |
| Frontend reviewer | Reviewer | Reviewing frontend work and running its tests. Reports problems; doesn't fix them. |
| Finalizer | Coder | Simplifying the finished change without changing what it does. |

None of them is a subagent: each role is a separate session in its own Connecto terminal, started for this task, so its bulk stays out of your context and each keeps what it learned for later rounds.

<!-- agents -->
## Choosing the agents

Every role runs in its own Connecto terminal, started with `connecto add`. A role is either a coder, which writes, or a reviewer, which checks what a coder wrote. Which coding agent runs each depends on which ones this project can start.

Before you start the first terminal, run:

```bash
connecto agents --roles
```

It lists the coding CLIs the project allows, whether each is ready (installed, offered and signed in), and the exact command that starts each ready one. Below that, it says which CLI takes each role: Claude Opus as coder and Codex as reviewer when both are ready, otherwise the best ready ones, with Pi and OpenCode on the model their user configured. Start the coder from its `coder` line and every reviewer from its `reviewer` line. Tell the user in one line which agents you chose. If the user asks for other agents or models, use those.

- If the `reviewer` line says `(same CLI)`, no other coding CLI is ready, so the reviewer is a fresh session of the coder's CLI. Tell the user so in one line, and say "same CLI" wherever you pass on its verdict.
- If a line says `(unverified)`, Connecto couldn't check that CLI's sign-in before it starts. If its terminal stops at a sign-in screen, handle it as a failed start, below. If it says `via <provider>`, that CLI runs the provider's own model, not the usual one: name it that way to the user.
- If a line says `none`, or an agent it would prefer shows `checking`, run the command again in a moment. If no CLI is ready, tell the user which ones are unusable and why, as the list says, and wait.
- If `connecto agents` fails with `unknown command "agents"`, or answers that this Connecto app is older than the command, Connecto is too old for it: start the coder with `connecto add claude --model claude-opus-5-5 --effort <effort>` and the reviewer with `connecto add codex --model gpt-6.2-astra --effort <effort>`; if `connecto add` refuses one because of the project's Tools setting, it names the CLIs the project allows.

**Effort.** Every command has an `<effort>` slot. Before each `connecto add`, fill it by how hard the task looks to you now. Decide by feel; don't read code to decide:

- `low`: a trivial edit, wording, docs, or a config value
- `medium`: a normal feature or fix
- `high`: work across modules or crates, concurrency, security, or a risk of losing data
- `xhigh`: only a rare, very hard problem

The reviewer gets the same effort as the coder. Antigravity's command carries the effort in its model name (`gemini-4-neon-<effort>`); OpenCode's has no slot.

If a new terminal exits, or reports that its agent isn't installed or needs a login, before its first reply, close it with `connecto close <id>`, count that agent as unusable for this session, tell the user, and start the role on another ready CLI from the list, keeping the reviewer on a different CLI from the coder while one is ready. If `connecto add` itself is refused, no terminal was made. If the reason is the terminal cap (16 terminals, stopped ones included), close your own finished or stopped terminals with `connecto close <id>`, such as an earlier team's coder or reviewer, and try again. Never close a terminal you didn't start. If none of yours can go, or the reason is another, such as Connecto being paused, tell the user the reason and wait. If it timed out instead, check `connecto list` for a new terminal of yours before trying again, since it may still have been made.

The commands in this skill are for a POSIX shell. In PowerShell (Connecto's default on Windows), send a brief by piping a here-string into the command: a line with `@'`, the brief, a line with `'@ | connecto send <id> --ask`.

`connecto add` prints the new terminal's id, such as `codex-2 starting`. Note which id has which role, and use only the terminals you started. Never send work to terminals you didn't start, because they belong to the user or to other teams. The `connecto` commands work in this project only; never reach another project's terminals with `--project` unless the user explicitly asks for it.
<!-- /agents -->

## Workflow

### 1. Workspace

First run `connecto list`. If it fails, stop and tell the user that this team must run in a Connecto terminal that can use the `connecto` command, because the whole team runs in Connecto terminals. Do this before anything else, so no work is lost later.

By default, each team works in its own git worktree and branch, so several teams can work on one repository at once and be merged later.

Use the current branch only if the task asks for it, for example "work on the current branch" or "no worktree". The project directory is then the workspace.

Otherwise, pick a short slug for this task and create the worktree:

```bash
root="$(git rev-parse --show-toplevel)"
git worktree add -b "dev-team/<slug>" "$root/../$(basename "$root")-worktrees/<slug>"
```

In PowerShell:

```powershell
$root = git rev-parse --show-toplevel
git worktree add -b "dev-team/<slug>" "$root/../$(Split-Path -Leaf $root)-worktrees/<slug>"
```

- If the branch or path already exists, it belongs to another team, so pick a different slug rather than reusing it.
- A worktree starts from the current commit. It doesn't include uncommitted changes or ignored files such as `.env` and installed dependencies. If `git status --short` shows uncommitted work, tell the user it won't be included, then continue. Tell the coders to install whatever the project needs.
- If this isn't a git repository, tell the user and use the project directory.

Put the workspace's absolute path in every brief, and tell each role to work only there.

### 2. Plan

Decide which sides the task touches, and skip the pair that isn't needed. Ask the user only about decisions that would change what gets built.

If the task points to a spec file, don't read it yourself. Put the spec's path in the workspace in every brief, and ask the first coder you start which sides it touches and for its contract section.

Choose the agents, then start a coder and a reviewer for each side the task touches, so both sides can work and be reviewed at once.

If both sides are involved, write the contract between them (endpoints, payloads, errors, auth) into both briefs so they can work at the same time. If you can't state the contract without knowing the code, run the backend coder first and give its report to the frontend one.

### 3. Implement

Brief each coder, in parallel when you have a contract. Both work in the same workspace, so give each one explicit file ownership:

```bash
connecto send <coder id> --ask <<'BRIEF'
You are the <backend or frontend> coder for a dev team. Read ${CLAUDE_SKILL_DIR}/roles/<backend.md or frontend.md> and follow it.

Workspace: <workspace path>. Work only there.

<the goal and acceptance criteria>

<the contract, if any>

You own: <files or areas>. Leave alone: <the other side's files>.
BRIEF
```

After sending, end your turn. Don't wait, sleep, or poll `connecto result`: Connecto delivers each reply to you when it finishes, and replies from the other side still reach you in the meantime.

### 4. Review

As soon as a coder reports done, ask its side's reviewer for a review, without waiting for the other side:

```bash
connecto send <reviewer id> --ask <<'BRIEF'
You are the <backend or frontend> reviewer for a dev team. Read ${CLAUDE_SKILL_DIR}/roles/reviewer.md and follow it exactly, including its reply format.

Workspace: <workspace path>. Review and run commands only there.

Round 1 of at most 3.

Focus: <the focus for this side, below>

<the goal, acceptance criteria and contract>

Changed files:
<the coder's list of changed files>
BRIEF
```

The focus for each side:

- Backend: correctness, security, input validation, authorization, data integrity, migrations, concurrency, error handling, performance on realistic data, and whether the agreed API contract is met.
- Frontend: correctness, state and data-flow bugs, loading, empty and error states, accessibility, responsive behavior, client-side security such as XSS, rendering performance, and whether the agreed API contract is used correctly.

Then end your turn. A reviewer replies `VERDICT: PASS` or `VERDICT: CHANGES_REQUIRED`, followed by its findings.

### 5. Fix

On `VERDICT: PASS`, that side is done, even if the reviewer lists minor findings. On `CHANGES_REQUIRED`, send the findings to the same coder, which already has the code in context. It fixes each one or explains why it disagrees. Then send the next round to the same reviewer, which remembers the previous round. Say which round it is, give the updated file list and the coder's reason for each finding it disputes, and ask it to check that its earlier findings were dealt with.

Each side has at most 3 review rounds. If round 3 still isn't a pass, coder and reviewer haven't settled it, so stop and ask the user for a compromise, using your tool for asking questions if you have one:

- **Open:** what they disagree about, in a line
- **Coder:** its position, in one or two lines
- **Reviewer:** its position, in one or two lines
- **Options:** two or three, such as keeping the coder's version, making the reviewer's change, or a middle way that either of them suggested

Then wait for the user. Send the decision to the coder word for word, then ask the reviewer for one last check that the decision was applied, and tell it the decision is settled and not to be reopened. If that check fails, go back to the user rather than starting new rounds. The other side carries on meanwhile.

If a coder reports that it's stuck, take what it reports to the user rather than starting another coder.

If a reviewer's reply doesn't start with `VERDICT:`, or Connecto reports that a terminal exited or needs the user, run `connecto start <id>` if it stopped and send the same request once more. If that fails too, don't try again on your own. Tell the user something went wrong, include what you saw, and ask how to proceed. Don't count it as a pass and don't substitute your own review, since independent review in a separate terminal is the purpose of this step.

### 6. Finalize

When every involved side has passed, start the finalizer, with the coder's agent. Its changes need one more review, so give it only the files of sides that have a review round left. If a side passed in its third round, or the user settled it, leave that side's files out of the list and tell the finalizer to leave them alone; if no side has a round left, skip this step and the next. Brief it:

```bash
connecto send <finalizer id> --ask <<'BRIEF'
You are the finalizer for a dev team. Read ${CLAUDE_SKILL_DIR}/roles/finalizer.md and follow it.

Workspace: <workspace path>. Work only there.

Changed files:
<the changed files of the sides with a round left>

Leave alone: <the files of the other sides, or "none">

Test commands:
<the test commands the coders used>
BRIEF
```

### 7. Final review

If the finalizer changed any files, ask the reviewer of each side it touched for another review, and tell it this is a check of the finalizer's simplifications, with the finalizer's list of changed files. If it reported "nothing", skip this step.

On `CHANGES_REQUIRED`, send the findings to the coder that owns the code, then review again. Don't run the finalizer a second time. These rounds count toward the 3-round limit for that side, and when it is reached, ask the user as in step 5.

### 8. Deliver

In a worktree, commit the finished work to the team's branch:

```bash
git -C "<workspace>" add -A
git -C "<workspace>" commit -m "<short summary of the task>"
```

Leave the branch and worktree in place. Don't merge, push, or remove them unless the user asks, because other teams' branches may need merging in a particular order. If the user asks you to merge and there are conflicts, give them to the coder that owns the code.

On the current branch, leave the changes uncommitted unless the user asked for a commit.

When every side has passed, close every terminal you started, with `connecto close <id>` for each.

### 9. Report

Give the user a short summary: what was built, the agents that ran each role, review rounds per side, what the finalizer changed, test results, any decision the user made, and anything unresolved. For a worktree, include the branch name, the worktree path, and the command to merge it.
