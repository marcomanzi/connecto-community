# ADR 0001: PTY-only transport for the PoC

**Context.** ARCHITECTURE section 5 pairs each TUI with a structured channel, and Codex with an app-server. U-4 (a second client on one thread) is unproven, and the probe showed `thread/resume` failing from a second client.

**Decision.** Both CLIs run as plain TUIs in PTYs. Delivery is bracketed paste, a 400 ms settle, then Enter. No app-server, no MCP.

**Evidence.** R6: a multi-line paste became one user turn on both TUIs.

**Consequences.** One delivery path for both tools. Costs: Codex has no early ack and no turn-start signal, and text can be lost if the TUI is not ready (R7), so `Written` is never success.

**Reverse if** paste proves unreliable on real turns, or S2 needs a richer Codex channel.

**Amended by the bridge (ADR 0016).** Codex now also gets `SessionStart` and `UserPromptSubmit` hooks, registered on the launch line with `-c hooks.*` and trusted through `-c hooks.state`. Delivery is still by PTY paste, and `notify` stays the Codex turn end.
