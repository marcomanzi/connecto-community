# 0043: Every agent ends a task with a status recap, from the built-in package

Status: Accepted (2026-11-24), built in `crates/connecto-packages`
(`builtin/*/.claude/rules/connecto-status.md`, `tools/vendor_builtin.py`,
built-in package version 9), `crates/connecto-session/src/package_text.rs`
(`STATUS_RECAP`, `for_launch`), the adapters (`LaunchCtx::package_text`;
Codex in `codex.rs`, Pi, OpenCode, Antigravity) and the Codex
`SessionStart` hook (`crates/connecto-hook/src/codex.rs`). Tested with unit
tests in those files, including
`a_project_with_the_earlier_builtin_gets_the_status_recap_on_update`,
`the_package_instructions_reach_every_agent_but_claude_at_launch`,
`the_recap_goes_through_the_session_start_hook_not_the_command_line`,
`the_recap_goes_through_the_extension_not_the_append_flag`,
`the_extension_adds_the_recap_as_a_prompt_section` (node) and the opt-in
`pi_real_recap` test (the installed Pi, `--ignored`); checked
against the installed Claude Code, Codex 0.167 and Antigravity. Not yet tried
through a rebuilt Connecto.

## Context

The user wants every agent, in every project that uses Connecto's built-in
package, to end a task that changed (or tried to change) something with a
short recap: Done / Committed-pushed (branch + hash, local or pushed) / Not
done or blocked / Waiting on user (yes/no + what). It must be always on, not a
skill an agent may skip, and must not overwrite the project's own
instructions.

## Decision

- **Claude** reads it from `.claude/rules/connecto-status.md`, a normal file
  of the built-in package. Claude Code loads every `.claude/rules/*.md` like
  `CLAUDE.md`. The project's `CLAUDE.md` is never written.
- **The other agents** get the same text at launch. It is compiled into
  Connecto from that file (`include_str!`), never read from the project, and
  passed only when the project's `.connecto/packages.lock` lists the built-in
  package (a lockfile or folder that is a link counts as none). It is not
  passed to utility sessions. Each tool gets it through its own channel, all
  of which add to the project's AGENTS.md rather than replace it:
  - Pi: Connecto's Pi extension adds it as a prompt section
    (`connecto-status-recap`) in `before_agent_start`, to the prompt Pi
    resolved itself, so the project's (trusted) or profile's (untrusted)
    `APPEND_SYSTEM.md` stays. `--append-system-prompt` is not used for it:
    that flag replaces the discovered `APPEND_SYSTEM.md` (checked on Pi
    1.5.2 with a stand-in model endpoint recording the request);
  - OpenCode: `instructions` in `OPENCODE_CONFIG_CONTENT`;
  - Antigravity: the launch-text file the `SessionStart` hook hands over as
    a system message;
  - Codex: Connecto's `SessionStart` hook answers with `additionalContext`.
    The text is compiled into the hook helper too; the launch only sets
    `CONNECTO_CODEX_RECAP=1`, so no file a session could rewrite is read.
    The hook command, and so its trust hash, is unchanged. Codex 0.167
    adds the answer as a developer message of its own, beside the
    configured `developer_instructions` and AGENTS.md (checked in session
    rollouts). It never goes into `-c developer_instructions` (which
    replaces the configured value) and adds nothing to the command line.
- The project's AGENTS.md is never written: a managed block there was tried
  and dropped (auto-commits could take the user's edits, a new AGENTS.md
  hides CLAUDE.md from Pi and OpenCode, and marker damage could lose text).

## Consequences

- A Codex terminal whose hooks are not registered (hooks off, or their trust
  could not be read) does not get the recap: Codex 0.167 has no other
  channel that adds to its configured instructions.
- Known limitations from before, not changed here (one separate task): when
  the bridge is on, Connecto's `-c developer_instructions=<bridge text>`
  replaces the developer instructions configured in Codex, and Pi's
  `--append-system-prompt <bridge text>` replaces the `APPEND_SYSTEM.md` Pi
  would load. The bridge text could take the same routes as the recap (the
  Codex hook, a Pi extension section). Done in ADR 0046, which also gives
  Codex's `SessionStart` hook an `additionalContextLimit` (so its trust hash
  changes once).
- The Pi and Antigravity launch files in the run folder are written by
  Connecto under the user's account, like every launch file; Codex's recap
  no longer depends on one.
- Custom packages cannot add launch-time instructions; they can ship
  `.claude/rules/` files for Claude.
- The Codex hook may fire again on a resumed session; the recap is then
  repeated, which is harmless.
