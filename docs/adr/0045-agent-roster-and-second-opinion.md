# 0045: Teams take their agents from `connecto agents --roles`; a second opinion skill

Status: Accepted (2026-11-24), built in `crates/connecto-core/src/roster.rs`
(the choice and the text), `crates/connecto-session/src/bridge/roster.rs` (the
bridge op, the Tools check), `crates/connecto-app/src/roster_host.rs` (ready or
not, per CLI), `crates/connecto-hook` (`Op::Agents`, the command), and the
built-in package (version 10: `tools/agents/variant-*.md`, the
`second-opinion` skill). Tested with unit tests and the bridge test
`agents_lists_what_the_tools_setting_allows_and_picks_only_ready_ones`. Not yet
tried through a rebuilt Connecto. The research behind it is
`research_notes/cli-combination-skills/proposal.md`.

## Context

The team skills chose their agents from a table baked into each package
variant and a `command -v claude codex agy`. That could not follow a project
whose Tools setting is any set of CLIs (ADR 0044), did not know which CLI is
signed in, and never used Pi or OpenCode. The host already knows, per CLI,
whether it is installed, offered and signed in (the checks an Agentic Works
job's agent passes, ADR 0041); agents could not see it.

## Decision

- **`connecto agents [--roles]`** lists, for the caller's project, each coding
  CLI: `ready`, `unusable` (why), `checking`, or `not allowed` (left out by the
  Tools setting), its version, and for a ready one the command that starts it.
  With `--roles` it adds the CLI and command for each role: `coder`,
  `reviewer`, `second-opinion`. Reading only, so allowed under Pause; this
  project only (no `--project`).
- **Allowed is decided by the bridge** with `tool_refusal`, the check every
  `connecto add` passes, whatever the app says; ready by the app, as a job's
  agent is: settings (installed, offered, not switched off), then the look
  (Pi's providers, the sign-in in the project's profile). The looks run side
  by side, one per project, CLI and profile at a time (a call that comes
  while one runs waits for it, so a retry never starts a second check), are
  kept 30 s, and one not back within 20 s of the call shows as `checking`.
  A look still running after 60 s is stale: the next call starts a new one,
  and a stale look that answers late changes nothing. The live look threads
  of each key are counted apart from the cached answer, until each really
  ends: while two are alive no further look starts, so at most two threads
  per key ever exist (a CLI whose check hangs for good shows as hung).
  Versions come from the agent rows, not the Settings view, which would read
  the secret store first.
- **It only reads**, since it runs under Pause: a project's own Pi providers
  host is never started for it (its first check may save a moved LM Studio
  address), so such a Pi is `unverified` until its first Pi terminal or job
  starts the host. Antigravity's check sees only that `agy --version`
  answers, so it is never more than `unverified`. An unverified CLI is chosen
  only after every ready one, and its role line says `(unverified)`.
- **TogetherLink.** Claude set up for Together AI runs Together's model and
  refuses `--model`: it is shown as `claude via Together AI`, started without
  `--model`, and ranked after CLIs on the role's own model.
- **Roles.** Coder: Claude, Codex, Antigravity, Pi, OpenCode, the first ready.
  Reviewer: Codex, Antigravity, Claude, Pi, OpenCode, the first ready that is
  not the coder's CLI. Second opinion: Claude, Codex, Antigravity, Pi,
  OpenCode, the first ready that is not the asker's CLI. When no other CLI is
  ready, the role runs on the same CLI, marked `(same CLI)`, and the skills
  must say so in what they report.
- **Models.** Claude `claude-opus-5-5`, Codex `gpt-6.2-astra`, Antigravity
  `gemini-4-neon-<effort>` (the effort is in the model's name), Pi and OpenCode
  their configured default. Kept in `roster::start_args` only.
- **Effort is the coordinator's.** Each command has an `<effort>` slot (none
  for OpenCode). The skills give a four-line rubric (low: trivial or docs;
  medium: normal feature or fix; high: across crates, concurrency, security,
  data loss; xhigh: rare, very hard), decided by feel without reading code;
  the reviewer gets the coder's effort.
- **Skills.** The three teams' "Choosing the agents" block now runs the
  roster (same text in every variant, with a fallback for an older app).
  `second-opinion` is new: user-invoked only (`disable-model-invocation`,
  kept in the `.agents` copies too since Pi and Antigravity read it, and
  Codex `allow_implicit_invocation: false`), one read-only terminal, at most
  one follow-up, then closed; its label follows the terminal that answered,
  so an opinion from the asker's own CLI is always marked "same CLI". Every
  agent runs in a visible terminal; no one-shot runs (`codex review`,
  `agy -p`). An older `connecto` command (`unknown command "agents"`) or app
  is recognized: teams fall back to fixed agents, a second opinion to a
  fresh session of the asker's own CLI.

## Consequences

- A team in a project of Codex and Pi, say, now starts Codex and Pi instead
  of being refused at `connecto add`.
- A signed-out CLI is skipped before a terminal is made for it; one that
  still fails at start is handled by the skill as before.
- Model names live in one Rust table; changing one needs a Connecto release,
  not a package update.
- `connecto add` still accepts a signed-out CLI; refusing it there is left
  for later. So are `parallel-attempts` and `bulk-work`.
