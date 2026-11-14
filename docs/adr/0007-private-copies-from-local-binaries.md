# ADR 0007: private copies from local signed binaries

**Context.** ARCHITECTURE 12.2 proposed downloading pinned versions. The PoC needs copies now, and U-17 is unproven.

**Decision.** Copy the installed binaries, read-only from the shared install, into the store. Verify code signature, Team ID and SHA-256. Approved by PM decision `oq16-runtime-write-exceptions`.

**Consequences.** Same bits as the installed version, no network, shared installs unchanged. Versions are whatever is installed at provisioning, so the adapter version table can refuse them. Not distribution: this is a local copy for the user's own use, and how terms apply to a public release stays unresolved. Download-based acquisition stays the plan for setup flows.
