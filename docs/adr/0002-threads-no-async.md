# ADR 0002: threads and bounded channels, no async runtime

**Context.** ARCHITECTURE section 6.2 uses tokio for actors, adapters and network work. This slice has two sessions and no network client.

**Decision.** One actor thread per session plus PTY, hook and audit threads, all on bounded `crossbeam-channel` queues.

**Consequences.** Fewer dependencies, simpler shutdown, and no way for async work to stall input. tokio returns when Jev or the app-server arrives. Session crates expose handles and messages rather than threads, so that change stays local.
