# ADR 0004: exact per-session TTY rules

**Context.** PM decision `oq16-runtime-write-exceptions` forbids wildcard TTY or descriptor rules that let one session write into another. An open read matters too: reading another session's slave could take keystrokes meant for it.

**Decision.** The profile is generated after the PTY opens. It allows only `(literal <own tty>)`, denies read and write on every other `/dev/ttys*`, and limits descriptors to `^/dev/fd/[0-9]+$`. Codex runs with `features.unified_exec_tty=false`.

**Evidence.** R13: both TUIs run; reads and writes of a victim PTY were denied; `/dev/stdout`, `/dev/fd/1` and process substitution work. R14: `openpty` fails under these rules.

**Alternatives.** Allowing `/dev/ptmx` with a slave wildcard restores PTY tools and breaks the requirement.

**Consequences.** Agents cannot allocate PTYs (`script`, `ssh -t`, Python `pty`). Codex says so cleanly. Denying other TTYs is a read limit on devices, justified as session isolation, not as a change to the reads-open decision on files.
