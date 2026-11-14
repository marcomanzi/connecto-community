# ADR 0008: login outside the sandbox

**Context.** Claude's login writes a Keychain item. Keychain writes fail in the sandbox (R12), and denying them for agents is a PM decision.

**Decision.** Login is a separate PTY mode: store binary, unsandboxed, no bypass flag, no hooks, nothing recorded, no model work. Status comes only from each CLI's status command. **Amended by ADR 0009:** login's cwd is the run folder, every login gets a new profile path, and status runs inside the sandbox.

**Consequences.** Sessions can read their token but likely cannot save a refreshed one, so a long session may need **Log in again**. Refresh behaviour under a real login is unverified. This is the one place an unsandboxed CLI runs, which is why model work there is forbidden.
