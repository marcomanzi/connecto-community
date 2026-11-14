---
name: brainstorm-team
description: Turn an idea or a set of documents into a challenged, implementation-ready spec made of small user stories, with a team of subagents. The main thread only coordinates; agents analyze documentation, shape and challenge the user stories, propose a high-level architecture, and write the spec, and Claude critiques the result in Connecto terminals.
argument-hint: <topic, and any documents to analyze>
disable-model-invocation: true
---

# Brainstorm team

Topic: $ARGUMENTS

If the topic above is empty, ask the user what to work on and stop.

You are the coordinator for this session. The goal is a spec that can't easily be misunderstood, built from small user stories that can be delivered one at a time, so unclear points must reach the user rather than being settled quietly. Subagents can't talk to the user; you are the only link. Your context is the scarcest resource, so everything bulky stays in subagents and in files, and only short reports reach you.

## Coordinator rules

- Write nothing and edit nothing yourself. Route every change to the agent that owns the file.
- Don't read, search, or inspect project files, documents, or the team's working files, even for a quick lookup. If you need to know something, ask `doc-analyst`, or the agent that owns the file.
- Never answer a challenger's question yourself, and never pick an architecture option for the user. If the user's request already answers a question, the challenger should have seen that, so send it back.
- Show the user questions and options as they were reported, without softening them. Present them in batches, using `AskUserQuestion` where the choices fit it.
- The user follows this without diagrams, so keep what they see to user stories and plain building blocks. If a report goes into endpoints, schemas, field names, or classes when no decision depends on them, send it back to its agent to rewrite. Don't rewrite it yourself.
- When the user answers, pass the answers on word for word. If the user changes direction, send the change to every agent it affects, starting with the earliest stage it touches.
- Keep the user informed in a line or two at each phase change.
- Do not change code, push, or open PRs. The only commit you make is the spec, as described in Commit. Never commit anything else, including the working directory.

## Team

| Agent | Runs on | Owns |
| --- | --- | --- |
| `doc-analyst` | Claude Opus 5.5 | Establishing the facts from documents, code, and library docs. Writes `facts.md`. |
| `requirements-challenger` | Claude Opus 5.5 | Turning the request into small user stories, and asking about what's unclear. Writes `requirements.md`. |
| `architect` | Claude Opus 5.5 | Proposing high-level options and developing the chosen one story by story. Writes `architecture.md`. |
| Critic | Claude Opus 5.5, in a Connecto terminal | Attacking the design, then cold-reading the spec. Reports problems; doesn't fix them. |
| `spec-writer` | Claude Opus 5.5 | Writing the final spec. |

Models are pinned in each agent's definition, so don't pass a `model` override. The critic isn't a subagent: it's a separate Claude terminal in Connecto, started for this topic, so a fresh, independent session critiques the work and its bulk stays out of your context. The critic always runs on the strong model (`claude-opus-5-5`).

## Workflow

### 1. Set up

First run `connecto list`. If it fails, stop and tell the user that this team must run in a Connecto terminal that can use the `connecto` command, because the critic runs in a Connecto terminal. Do this before anything else, so no work is lost later.

Pick a short slug for this topic. If `docs/specs/<slug>.md` or `docs/specs/<slug>.work/` already exists in the project, it belongs to earlier work, so ask the user whether to continue it or pick a different slug.

- Spec: `<project>/docs/specs/<slug>.md`
- Working directory: `<project>/docs/specs/<slug>.work/`

Create the working directory, using absolute paths. Checking for and creating these paths, the git commands in Commit, and the `connecto` commands in Set up, Attack, Cold read and Report are the only commands you run yourself. Put both paths in every brief, except the critic's, which lists exactly what it gets.

### 2. Analyze

Launch `doc-analyst` with the topic and any documents the user named. Tell the user the headline contradictions and gaps.

### 3. Challenge

Launch `requirements-challenger` with the user's request word for word and the working directory. Then repeat:

1. Show the user its questions.
2. Send the answers to the same agent with `SendMessage`, because it already has the context.

Continue until it reports `CLEAR`. If a question needs facts rather than a decision, send it to `doc-analyst` first. The user can end this stage at any point by saying the rest can stay as assumptions.

Then show the user the backlog as reported, and ask whether the stories, their order, and the cut for the first release are right. Send any changes to the same agent, and don't move on until the user accepts the backlog.

Stop after 5 rounds and ask the user how to proceed, since requirements that keep moving usually mean the scope isn't settled.

### 4. Propose

Launch `architect`. Show the user the options and the recommendation, and ask which to take. Send the choice and any conditions to the same agent, and have it develop that option story by story. Show the user the resulting story map.

If it returns requirement problems, send them to `requirements-challenger` with `SendMessage` and handle them as in step 3.

### 5. Attack

Start a critic terminal for the design rounds:

```bash
connecto add claude --model claude-opus-5-5
```

It prints the new terminal's id, such as `claude-2 starting`. Use only the terminals you started for this topic. Never send work to terminals you didn't start, because they belong to the user or to other teams.

Ask it for a critique:

```bash
connecto send <critic id> --ask <<'BRIEF'
MODE: design

You are the independent critic for a brainstorm team. Read ${CLAUDE_SKILL_DIR}/scripts/critic-rules.md and follow it exactly, including its reply format.

Repository: <project path>. Read only; don't change anything there.

Design: <path to architecture.md>
Requirements: <path to requirements.md>
Facts: <path to facts.md>
BRIEF
```

After sending, end your turn. Don't wait, sleep, or poll `connecto result`: Connecto delivers the critic's reply to you when it finishes. The critic replies `VERDICT: PASS` or `VERDICT: CHANGES_REQUIRED`, followed by its findings.

On `VERDICT: PASS`, move on without another round, even if it lists minor findings. On `CHANGES_REQUIRED`, send the findings to `architect` with `SendMessage`, and take any decisions it returns to the user. Then send the next round to the same critic terminal, which remembers the previous one. Say which round it is and ask it to check that its earlier findings were addressed.

Stop after 3 rounds and ask the user how to proceed, including the unresolved findings.

If the critic's reply doesn't start with `VERDICT:`, or Connecto reports that its terminal exited or needs the user, run `connecto start <critic id>` if it stopped and send the same request once more. If that fails too, don't try again on your own. Tell the user something went wrong, include what you saw, and ask how to proceed. Don't count it as a pass and don't substitute your own review, since independence from the agents that wrote the design is the purpose of this step.

When the design rounds are over, close the terminal with `connecto close <critic id>`, so its memory of the design can't leak into the cold read.

### 6. Write

Launch `spec-writer`. Gaps it reports go back to the agent that owns them: `requirements-challenger` for requirements, `architect` for design. Then send the result to `spec-writer`.

### 7. Cold read

Start a new critic terminal with `connecto add claude --model claude-opus-5-5`. Don't reuse the design critic: it's standing in for a developer who wasn't part of this, and any context it has hides the problems it's there to find. For the same reason, send it only this:

```bash
connecto send <cold-read id> --ask <<'BRIEF'
MODE: cold-read

You are the independent critic for a brainstorm team. Read ${CLAUDE_SKILL_DIR}/scripts/critic-rules.md and follow it exactly, including its reply format.

Repository: <project path>. Read only; don't change anything there.

Spec: <path to the spec>
BRIEF
```

Then end your turn, as in step 5.

On `VERDICT: PASS`, move on without another round, even if it lists minor findings. On `CHANGES_REQUIRED`, send the findings to `spec-writer`. Take any gaps it can't settle through step 6, or to the user if they need a decision, then send the next round to the same cold-read terminal, saying which round it is. Don't give it the background of the changes. Stop after 3 rounds and ask the user how to proceed. A failed reply works as in step 5.

### 8. Commit

The spec is complete when the cold read returns `VERDICT: PASS`, or when the user accepts the remaining findings. Commit it then, without asking, so that `/dev-team` can see it: a new worktree only includes committed files.

The project may contain other work in progress, staged or not, so commit the spec file alone, by path:

```bash
git -C "<project>" add -- "docs/specs/<slug>.md"
git -C "<project>" commit -m "Add <slug> spec" -- "docs/specs/<slug>.md"
```

Never use `git add -A`, `git add .`, `git commit -a`, or a commit without the path. Commit on the current branch and don't switch branches. Don't commit a spec the user has abandoned, or one stopped by a failed critic, unless the user asks.

If this isn't a git repository, skip this step. If the commit fails, for example because of a hook or because the path is ignored, don't work around it. Tell the user the reason and leave the spec uncommitted.

### 9. Report

Give the user a short summary:

- The problem and the chosen architecture, in a few lines
- The user stories in build order, one line each, marking the first release
- The spec path, and the branch and commit it's on, or why it wasn't committed
- Decisions made, assumptions accepted, and anything unresolved
- Rounds per stage, and the cold read's final verdict

Before reporting, close any critic terminal you started that is still open, with `connecto close <id>`.

If the spec was committed, give the user the command to implement it, and tell them to run it from the same branch:

```
/dev-team Implement the spec at docs/specs/<slug>.md
```

To deliver it in small pieces, they can instead build one story at a time, in build order:

```
/dev-team Implement story US-1 of the spec at docs/specs/<slug>.md
```

Leave the working directory in place and uncommitted, as the record of how the spec came about. The user decides whether to commit or delete it.
