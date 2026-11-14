# ADR 0005: structured ack and capture

**Context.** Relaying a reply needs the reply's text. Scraping a TUI grid is fragile.

**Decision.** Claude: `UserPromptSubmit` for ack, `Stop.last_assistant_message` for capture, through `--settings`. Codex: `notify`, set on the command line, for both. Payloads enter as `Untrusted<T>` and reach only state and the capture slot.

**Evidence.** R8, R10. R9: Codex hooks raise a trust-review modal that swallowed pasted input, and `--dangerously-bypass-hook-trust` stays forbidden.

**Consequences.** Codex ack happens only at turn end. `notify` fires for hidden title threads, so events are accepted only when a rollout file exists for the `thread-id`. That rule depends on 0.166.0 behaviour, which is why it sits in the adapter and is version-gated. Codex readiness has no signal and uses screen text, as a warning only.

**Amended after T-003.** The hook channel authenticates the session's process tree, not the CLI, so an ack is the session's claim about itself. Ack must match the exact delivered text, which closes header-only matches. A process in the session can still forge the exact text. That is accepted because a false claim reaches only that session's row and capture slot, never the gate, and `Completed` is documented as reported, not proven.

**Amended by the bridge (ADR 0016).** A Claude `Stop` with background work running no longer means the turn is over, and Codex hooks report the prompt and its turn id. Replies up to 256 KiB are held whole; a longer or cut reply is never reported as a result.

**Superseded in part.** The version gate is gone: capabilities no longer depend on the CLI version, and the bridge is on for every version.
