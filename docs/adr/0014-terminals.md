# ADR 0014: up to eight terminals, a saved list, one reservation table

**Context.** Connecto ran two fixed terminals. The bridge needs agents to add and close terminals (R12, R13, R16), and several terminals of one tool share one login profile (A23). `LIVE_PROFILES` claimed nothing while a session launched, so two logins, or a start landing on a generation a login was about to replace, could overlap.

**Decision.**

- A registry thread owns eight slots, the terminal ids and `<data>/projects/<id>/terminals.json`. Add, remove, start, close and choosing a project all pass through it. Nothing on the UI thread touches the disk: the UI sees `TerminalsControl` in `connecto-core` (`view()`, `add(tool)`, `remove(id)`), and a failure shows in `TerminalsView::last_error`.
- Ids are `^[a-z0-9-]{1,32}$`, never reused (`next` counters only move forward), and are the display name. `SessionRef.session` stays a slot number.
- The cap is eight listed terminals, stopped ones included. Add and remove write the list first and change nothing if the write fails. A change of `desired` (stop, close, exit, start) never waits on the disk: the process change happens, `desired` changes in memory, and a failed write marks the list unsaved with a retry every 30 s. App shutdown sets a flag first, so exits during a quit are not saved as stopped.
- An unusable list is moved to `terminals.json.bad` and the default two are used with a visible warning. A list that cannot be read, or moved aside, puts the project in read-only list mode for that run: it is never written.
- `LIVE_PROFILES` becomes a reservation table keyed by tool root, under one mutex. A session reserves the current generation before it launches, a login rotates the generation only with nothing running or launching, and a claim stops counting when its process ends, so a stopped terminal kept for reading blocks nobody. Two logins, or a start during a login, are refused.
- Every start carries a ticket taken when it is asked for. A Stop bumps `stop_seq` first, and a start checks its ticket before it spawns and again after it publishes the session, so a Stop during preflight, or between spawn and publication, always ends with nothing running.

**Alternatives.** One thread per terminal for the list, and a lock around launch. The first gives no ordering between add and remove. The second blocks the UI and a Stop.

**Consequences.** The reservation is per app process, as before. Two copies of the app on one project are not covered. Tab strip, README and the UI follow in `connecto-ui`.

**Changed after the first manual run.** An agent's `close` now stops the terminal and removes it, the same as the tab strip's remove button: the registry removes it once its process has ended, with the rules `remove` already had (never the last terminal, never when the list cannot be written, the new list written first). A close that cannot remove a terminal still stops it and says why. The Gate admission, the Pause epoch, the attempt checks and the self-close delay are unchanged.
