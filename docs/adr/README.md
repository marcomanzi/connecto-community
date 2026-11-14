# Architecture decision records

These record the main design decisions.

| ADR | Decision | Status |
| - | - | - |
| [0001](0001-pty-only-transport.md) | Both CLIs run as TUIs in PTYs; delivery by bracketed paste | Accepted for this slice |
| [0002](0002-threads-no-async.md) | Threads and bounded channels, no async runtime | Accepted for this slice |
| [0003](0003-hardened-seatbelt-profile.md) | Hardened Seatbelt profile; write boundary covers indirect routes | Accepted (superseded by 0013) |
| [0004](0004-exact-device-rules.md) | Exact per-session TTY rules; agents get no new PTYs | Accepted (superseded by 0013) |
| [0005](0005-structured-ack-and-capture.md) | Claude hooks and Codex `notify` for ack and capture | Accepted for this slice, amended after T-003 |
| [0006](0006-user-click-breaks-loops.md) | Every relay hop needs a user click | Accepted for this slice (superseded by 0015 for agents) |
| [0007](0007-private-copies-from-local-binaries.md) | Private copies come from local signed binaries | Accepted for this slice |
| [0008](0008-login-outside-sandbox.md) | Login runs outside the sandbox with no agent | Accepted, amended by 0009 (superseded by 0013) |
| [0009](0009-auth-paths-trust-no-session-writable-state.md) | Status runs in the sandbox; every login gets a new profile path | Accepted, revised after T-001 (superseded by 0013) |
| [0010](0010-hold-egui-after-flood-rerun.md) | Hold egui: reversal rule not met on the final build; earlier stalls unexplained | Accepted for this slice, final |
| [0011](0011-hidden-window-runs-no-ui-pass.md) | A hidden window runs no UI pass; nothing may depend on one | Accepted for this slice |
| [0012](0012-refuse-launch-on-outside-hardlinks.md) | Refuse launch when a hard link may lead outside the project, and when the scan is incomplete | Accepted, implemented in `2f13dddc`, Tester-verified (V-34) (superseded by 0013) |
| [0013](0013-sandbox-removed.md) | The sandbox is removed; sessions run directly with bypass flags; Claude cross-session messaging off | Accepted |
| [0014](0014-terminals.md) | Up to eight terminals, a saved list, one profile reservation table, start tickets | Accepted |
| [0015](0015-agent-actor.md) | Agents are an actor of their own and may type at once; Pause is a full brake | Accepted, supersedes 0006 |
| [0016](0016-bridge-socket-and-connecto-command.md) | One bridge socket and a `connecto` shell command | Accepted |
| [0017](0017-remote-sessions.md) | Remote sessions over iroh, authorized by SSH keys, in `connecto-net` | Accepted, first slice built (no UI) |
| [0019](0019-plain-shell-terminals.md) | Plain shell terminals, and the terminal-kind model | Accepted |
| [0020](0020-system-config-by-default.md) | New projects use the system configuration; isolated is advanced | Accepted |
| [0021](0021-default-automations.md) | Six default automations, off, seeded once and never over the user's own | Accepted |
| [0022](0022-mutual-pairing.md) | Pairing by two codes, both ways: each machine types the other's code, names it, and saves one record | Accepted, replaces the one-sided pairing of 0017 |
| [0023](0023-send-readiness-first-send-only.md) | Readiness gates only the first send to a terminal; later sends are typed whatever the screen shows | Accepted as a known limitation; fix sized, not built |
