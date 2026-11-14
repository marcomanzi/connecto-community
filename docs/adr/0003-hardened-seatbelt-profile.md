# ADR 0003: hardened Seatbelt profile

**Context.** ARCHITECTURE section 7.2 rated Seatbelt writes as a hard boundary, on evidence from direct writes only. A profile based on `(allow default)` leaves services outside the sandbox reachable, and they write on the caller's behalf.

**Evidence.** R3: `open` on a bundle in a writable folder wrote a canary to a denied folder. A custom binary calling LaunchServices did the same, so denying `open` alone would not help. R4: base profile could connect to unix sockets, and `docker.sock` exists here. The hardened profile blocked each, and both CLIs completed tool turns under it.

**Decision.** Add `(deny lsopen)`, `(deny appleevent-send)`, `mach-lookup` denials for LaunchServices and Apple Events, and deny outbound unix sockets except mDNSResponder and the session's own hook socket. The launch canary includes an `open` escape attempt.

**Consequences.**
- ARCHITECTURE's "hard" rating is corrected: direct writes are denied, and tested indirect routes are blocked. **Absence of other routes is not claimed.** The `mach-lookup` list is a denylist, and TCP services on localhost stay reachable.
- Blocking unix sockets goes beyond the user's "no added network limits". It is a write-boundary control, and it removes Docker and the SSH agent from sessions. Disclosed to the PM.

**Next.** Replace the denylist with a `mach-lookup` allowlist. `sandbox-exec` remains deprecated.
