# 0046: The bridge text adds to the user's own instructions on Codex and Pi

Status: Accepted (2026-11-24), built in `crates/connecto-adapter`
(`codex.rs`: `BRIDGE_TEXT_ENV`, `SESSION_START_CONTEXT_LIMIT`; `pi.rs` and
`pi_extension.mjs`: the `bridge` config and the `connecto-bridge` prompt
section), the Codex `SessionStart` hook (`crates/connecto-hook/src/codex.rs`,
`latest_connecto_message`, `SessionStart`) and Codex preflight
(`crates/connecto-session/src/codex_trust.rs`, `hook_handover`). Tested with
unit tests in those files, including
`the_bridge_text_never_replaces_the_users_own_instructions` (every agent,
new and resumed), `the_bridge_text_goes_through_the_session_start_hook_not_developer_instructions`,
`a_resume_skips_only_a_byte_identical_latest_message`,
`a_capability_turned_off_is_not_hidden_by_an_older_identical_text`,
`the_latest_message_is_read_backwards_and_bounded`,
`the_bridge_goes_off_with_an_earlier_helper_or_codex_hooks_off` and
`the_extension_adds_the_recap_as_a_prompt_section` (node, Pi 1.5 and 0.85
shapes), and with opt-in tests against the installed CLIs (`--ignored`):
`codex_real_bridge` (Codex 0.167.0, the real hook helper and a real trust
read: new, resumed twice, and memory turned on then off across resumes),
`the_installed_codex_reports_a_profile_that_turns_hooks_off`, and
`pi_real_recap` (Pi 1.5.2, and 0.85.1 with `PI_BIN`), each with a stand-in
model endpoint recording the requests. Revised twice after review the same
day; the second revision dropped bringing the user's instructions back into
threads started before this change. Not yet tried through a rebuilt
Connecto.

## Context

Every agent terminal gets Connecto's launch ("bridge") text, which explains
the `connecto` command. Claude, OpenCode and Antigravity already received it
beside the user's own instructions. Codex and Pi did not:

- Codex got it as `-c developer_instructions=<text>`, which replaces the
  `developer_instructions` the user configured (the profile's or
  `CODEX_HOME`'s `config.toml`, a project's `.codex/config.toml`, an active
  profile).
- Pi got it as `--append-system-prompt <file>`, which replaces the
  `APPEND_SYSTEM.md` Pi would load (a trusted project's `.pi/APPEND_SYSTEM.md`,
  otherwise the profile's `agent/APPEND_SYSTEM.md`). Pi 1.5.2's resource
  loader discovers that file only when no `--append-system-prompt` is given.

ADR 0043 had already moved the package recap off these two options for the
same reason and listed this as a known limitation.

Codex 0.167.0 fixes a thread's configured developer instructions when the
thread is created: on resume neither a changed `config.toml` nor a
`-c developer_instructions` reaches the model, only the thread's own first
developer message (checked with `codex exec resume` and a recording
endpoint). A hook's `additionalContext` is kept in the thread like any other
message, so every resume adds to what earlier starts left there.

## Decision

The bridge text takes the recap's routes. The user's files are never written.

- **Codex**: Connecto's `SessionStart` hook answers with the text as
  `additionalContext`, the bridge text first, then the recap (when the
  project has the built-in package). Codex 0.167 adds it as a developer
  message of its own, beside the configured `developer_instructions` and
  AGENTS.md. The text differs per terminal (it names it), so it cannot be
  compiled into the helper like the recap: the launch puts it in Codex's
  environment as `CONNECTO_CODEX_BRIDGE_TEXT`, which the hook process
  inherits. Nothing is added to the command line, and
  `-c developer_instructions` is not used at all any more.
  - Codex keeps about 2,500 tokens of a hook's context by default and drops
    the middle of a longer one (it writes "Warning: truncated output"); the
    bridge text with the memory and operations sections is over 2,000
    tokens, and the recap comes after it. The `SessionStart` hook is
    therefore registered with `additionalContextLimit=20000`. Codex 0.167.0
    reads that key only in this camelCase spelling (`additional_context_limit`
    is silently ignored); both were tried against the real CLI. The key is
    part of the hook definition and so of its trust hash, which preflight
    reads for these same values; the prompt hook keeps Codex's own limit.
  - **No copy per resume, never stale.** The bridge text and the recap go
    as one message. On `resume` only, the hook reads the rollout Codex names
    in `transcript_path` backwards from its end (at most the last 8 MiB, in
    256 KiB pieces, never the whole file) for the most recent message of
    Connecto's hook (a hook-context part that starts as a bridge text, the
    note below, or the recap does), stopping at a `compacted` record. It
    leaves the message out only when that latest message is the same, byte
    for byte (with or without the note). Otherwise (a different text: memory
    turned off, another terminal id; none found; a compaction after it; an
    unreadable rollout) it hands the message over with a first line saying
    it replaces any earlier Connecto instructions. Older messages are never
    compared, so an earlier identical text cannot hide a change. `startup`,
    `clear`, `compact` and `fork` always get the message (at `SessionStart`
    a fork's rollout does not yet hold the inherited history). An occasional
    extra copy is accepted; missing or stale instructions are not.
  - **Threads started before this change keep their old behaviour.** Codex
    keeps a thread's configured instructions for good, and those threads
    were started with the bridge text as `-c developer_instructions`, so on
    resume they still lack the user's own instructions, as before; they get
    the current bridge text after the old one. Bringing the user's text back
    from the hook was tried and dropped: reading it with
    `codex debug prompt-input` starts the user's MCP servers a second time
    and can take longer than the hook may, and very long instructions are
    cut by the hook's context limit. Every thread started from now on has
    both.
  - **Preflight** (per launch, after the trust read) turns the bridge off with
    a reason, as for Claude's project hooks, instead of letting a terminal
    silently go without its instructions when:
    - the stored hook helper does not hand them over: it is run as Codex runs
      it, with a probe text, and must answer with it. A helper from an earlier
      build (left in the store when the one beside the app is missing or
      could not replace it) prints nothing;
    - the terminal's configuration turns Codex's hooks off:
      `codex features list`, run with the terminal's `CODEX_HOME` (or the
      user's own) in the project (so a trusted project's `.codex/config.toml`
      counts), shows `hooks … false`. A state that cannot be read does not
      turn the bridge off.
- **Pi**: Connecto's extension adds the text as a prompt section
  (`connecto-bridge`) in `before_agent_start`, before the recap's section
  (`connecto-status-recap`), to the prompt Pi resolved itself, so the
  `APPEND_SYSTEM.md` Pi chose (by its own trust rules) and the project's
  context stay. It rides in the extension's `CONFIG` line as JSON, so it
  needs no other escaping and has no command-line limit. The handler runs
  before every run, a resumed session's included: Pi diffs the sections
  against the ones its transcript has. A Pi without prompt sections (0.85.1:
  the handler gets the resolved `systemPrompt`, `APPEND_SYSTEM.md` included,
  and may hand back a changed one; Pi starts from its own again each run)
  gets the same texts appended to that prompt, in the same tags. Connecto
  sets no lowest Pi version (any that prints a version is ready), so both
  shapes are handled. `--append-system-prompt` and the
  `connecto-pi-instructions.md` file are no longer used.
- Claude, OpenCode and Antigravity are unchanged.

## Consequences

- A Codex terminal without working hooks gets no bridge text, and is shown
  as bridge-off with the reason: trust that cannot be read, a helper that
  would not hand the text over, or `features.hooks = false`. The adapter
  never falls back to `developer_instructions`.
- The bridge text is an environment variable of the Codex process, so the
  commands the agent runs inherit it too. It holds instructions only, no
  secret, and is about 9 KB, within the per-string limits (128 KB on Linux,
  32,767 characters on Windows) the old `-c` argument had to meet as well.
- A Codex version that does not know `additionalContextLimit` cuts a long
  text in the middle instead of failing; one that changes the default or the
  spelling, or the rollout records the hook reads, would show in
  `codex_real_bridge`. When the rollout cannot be read the message is handed
  over again.
- An earlier, different bridge text stays in the thread; the newest one
  comes after it and says it replaces it.
- A thread started before this change still lacks the user's own
  instructions when resumed (see above).
- Preflight runs `codex features list` and the helper once per Codex launch
  (about 20 ms and a few ms).
- Not checked against the real CLI: whether a Codex subagent thread runs
  `SessionStart`, and the `clear` and `fork` sources (the review checked
  startup, resume, clear, fork and compaction by hand on 0.167.0).
