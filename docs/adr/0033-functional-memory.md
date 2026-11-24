# 0033: Functional memory is a folder of Markdown in the repository, updated by a utility session when main advances and committed on its own

Status: Accepted (2026-11-22), built in
`crates/connecto-app/src/functional_memory.rs` (watcher, prompts, writing,
commit), `crates/connecto-core/src/functional_memory.rs` (types, limits),
the `functional-memory` project asks (`crates/connecto-net/src/project_panels.rs`,
`hosting.rs`, `manager.rs`), the `functional_memory` peer op
(`connecto-flutter-app/native/src/lib.rs`) and a card under Project
Settings > Project Memory. Tested with temporary git repositories and a
fake model; not yet run against a real model on a real project.

## Context

Voice calls on the phone (ADR 0028) gather requirements for a new feature
before a coding agent is started. To ask informed questions ("the export
already has a CSV option; should this follow it?") the call needs to know
what the project already does. Project memory (ADR 0024) holds what agents
learned while working, in a store on the host, not a description of the
product. Reading the code during a call is too slow and too large for a
conversation model's context.

The person decided: the description lives in the repository, in
`.functionalMemory/`; it is updated when main advances, incrementally from
the diff; the update runs as a utility session (ADR 0025); the result is a
separate commit touching only that folder; a peer can read it and ask for
an update; it can be updated by hand and its status shown.

## Decision

1. **A folder of plain Markdown at the repository root.** `README.md` (the
   index), `overview.md`, `architecture.md`, `features.md`, `flows.md`,
   `glossary.md` on a full build; the model may merge or add files. Top
   level only, plain names (`[A-Za-z0-9][A-Za-z0-9._-]*.md`), at most 15
   documents, 32 KiB each, 63 KiB together (the target given to the model
   is 30 KB). `state.json` is Connecto's own: `{"commit", "updatedMs"}`,
   the main commit the files describe. It is in git, so a clone carries it
   and every machine agrees on what is described.
2. **The trigger is main moving.** A watcher looks at every open project
   every 20 s: the repository's top folder, then `refs/heads/main`, else
   `master`. `state.json` is read from main's committed tree (`git
   cat-file blob main:.functionalMemory/state.json`), never the working
   tree: what main describes is the same whatever is checked out, and
   files written but not committed never count. An update is due when
   main is at a commit other than that one and `git diff --name-only <described> <main>` names a
   path outside `.functionalMemory/`. So Connecto's own commit, and a
   person's edit of the files alone, start nothing (no update loop). It
   starts once main has been still for 90 s (a burst of merges is one
   update). Deciding to run and claiming the run happen under one lock,
   so the watcher and Update now never both start one, and there is one
   run at a time per repository: two projects in one repository never
   write its folder together (the second is deferred). A project with no memory is
   not built at start up, only on the first move of main seen while the
   app runs, or by hand: opening Connecto must not start a full build in
   every project.
3. **Read-only session, the host writes.** ADR 0025's read-only launch
   (Claude `Read,Grep,Glob`; Codex `--sandbox read-only`) cannot write, and
   a hidden session with write tools would stop on a permission prompt. So
   the session is asked to *reply* with the files in fixed blocks
   (`<<<FILE name.md>>>` … `<<<END FILE>>>`, `<<<DELETE name.md>>>`,
   `<<<NO CHANGES>>>`); the host checks every name and limit, and writes
   only inside the folder. This is stronger than telling a session to edit
   only that folder: it cannot write anywhere. A full build reads the
   project with its tools; an update gets the commit list, the change list
   and the diff (up to 96 KiB, else only the list and it reads the files)
   and the current files, and gives back only the files that change.
   New purpose `UtilityPurpose::FunctionalMemory`: read-only, strict
   prompt-arrival check. The model is chosen per project in Project
   Memory (kept with the switch in `<data>/functional-memory/<id>.json`),
   among the models of the CLIs that can run utility sessions in the
   project (installed and signed in; asked by the watcher every ten
   minutes, never by the window): Claude's chooser list without
   `opusplan`, and Codex's own model list (its cache) or the chooser's.
   The default is Claude `sonnet`, else Codex `gpt-6.2-sol` (else the
   first Codex model), at effort `medium`. A chosen model whose CLI can no
   longer run there falls back to the default until it can again. Only an
   offered model can be chosen, also from a peer. Antigravity is not a
   choice yet: it has no utility launch (`UtilityTool`), and its models
   (`gemini-4-neon-high`, …, from `agy models`) wait for one. Deadlines:
   15 min for a full build, 10 min for an update.
4. **Its own commit, nothing else.** `git add -A -- .functionalMemory`,
   then `git commit --only -- .functionalMemory` with the message
   `Functional memory: update to <short hash of the described commit>`.
   Whatever else is staged or modified stays as it was (tested). Before
   the run and again before writing: main must be checked out (`HEAD` is
   `refs/heads/<main>`), and no merge, rebase, cherry-pick, revert or
   bisect in progress; otherwise the update is **deferred** and tried
   again on the next look. Pause defers too; a request by hand that is
   deferred stays asked for. A reply with no change still moves
   `state.json` on, in a commit of that file alone. The folder itself must
   be as `HEAD` has it (nothing staged, changed, untracked or ignored
   there: `git status --untracked-files=all --ignored -- .functionalMemory`
   empty), checked before the run and
   again just before writing: the person's own changes there are never
   written over, staged or committed, the update waits for them instead.
   Just before writing, `HEAD` must still be the commit the run started
   from: a commit made while the model worked (a correction to these very
   files) is never undone by a reply written from before it; the reply is
   dropped and the next look starts again from the new main.
   So when writing or the commit fails (signing, a lock), `HEAD` is exactly
   what was there before: the folder's index entries are reset to it, the
   files checked out from it, and files new to it removed; nothing of the
   update lands in the person's next commit. Each file is written to a new
   temporary file created exclusively in the folder and renamed over the
   name, so a link there is replaced, never written through. Reads (for
   the prompt and for peers) open the folder and each file relative to
   folder handles with no link followed (`openat`, `O_NOFOLLOW`), one
   file at a time, never reading more than what the caps leave. Names that differ only in case are
   refused (they are one file on a Mac's disk).
5. **Git is run as the person's own, with the repository's programs off.**
   Unlike project memory's confined git (ADR 0024, `memory_git.rs`), this
   writes the person's repository, as `connecto-packages`'s commit does,
   so it runs the person's `git` with `-c core.hooksPath=/dev/null -c
   core.fsmonitor=false`, `--no-verify`, `--no-ext-diff --no-textconv` on
   diffs, `GIT_TERMINAL_PROMPT=0`, and 30 s per command. Commit signing
   follows the person's configuration (a passphrase prompt times out and
   fails the update).
6. **Peers read it through a project ask.** `{"ask":"functional-memory"}`
   answers `{"files":[{"path","text"}],"truncated","status":{"state",
   "commit","updatedMs","message","auto"}}`, the index first;
   `{"ask":"functional-memory-update"}` starts an update and answers the
   same. Both go through the remote guard like every project ask (audited,
   refused while paused). The peer checks names, cleans text and applies
   the caps again. The phone's op is `functional_memory` /
   `functional_memory_update` with `{host, project}`; a host from before
   answers "not an ask this machine knows", which the app takes as no
   memory. Its consumers on the phone: Gemini Live calls (ADR 0034), all
   of it in Gemini's instructions; and the on-device call (ADR 0028), all
   of it for the Mac's model, README.md and overview.md for the phone's
   own models, and the project's check told to start from the folder.
7. **By hand and status in the window.** Project Settings > Project Memory
   shows a card: the state (not built, up to date with the commit and when,
   waiting, updating, deferred with why, failed with why, off), Update now,
   and "Update when main advances" (on by default; kept per project in
   `<data>/functional-memory/<project id>.json`, outside the repository).
   A project viewed from another machine shows the same card under its
   Project Memory tab, with the host's status (the `functional-memory`
   ask, asked again every 5 s, every second while it runs) and Update now
   sent to the host as `functional-memory-update`; the switch and the
   model are changed on the host with `functional-memory-set` (`{"auto":
   bool}`, `{"model": {"provider","model"}}` or `{"model": null}` for the
   default), checked there as the window's are.

## Consequences

- The window's status never runs git: it shows what the watcher (a first
  look 2 s after start, then every 20 s) and the runs last read.

- Connecto makes commits on the person's main branch by itself. They are
  small, separate and named, so they are easy to see, revert or squash;
  the switch turns them off per project. Nothing is pushed.
- A project whose `.functionalMemory` is ignored by git gets the files but
  the commit fails, shown as failed.
- The description is only as good as the model and the code it reads; it
  is context for questions, not truth. A wrong file is fixed by editing it:
  that edit alone starts no update, and later updates keep it unless the
  code changes it.
- Each update costs one utility session; the debounce and the
  "only the folder changed" rule keep that to one per real change of main.
- A subfolder project uses the repository's top folder; two projects in
  one repository share one memory (and may both see it due; the second
  finds main already described and does nothing).
- No `connecto` CLI verb in this version: the window and the phone are
  the two ways to start one by hand.
