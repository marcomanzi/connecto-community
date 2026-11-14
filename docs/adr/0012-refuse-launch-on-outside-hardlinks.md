# ADR 0012: refuse launch when a hard link may lead outside the project

**Context.** Seatbelt checks writes by path, so a project file hardlinked to an outside file can be written through, and the outside file changes (R21, T-002). The first design only warned. The PM ruled that a warning does not meet the write boundary the user chose, and that README disclosure is not authorization (decision `hardlink-project-write-boundary`).

**Finding.** The old comment said links inside the project "cannot be told apart cheaply". That was wrong. A file's link count is known, so if the scan finds as many links inside the project as the file has, none is outside. Source review also found two ways the old scan failed open: `entries.flatten()` dropped read errors, and `validated(...).unwrap_or_default()` turned a failed check into a clean result.

**Decision.**

1. **The test.** For every non-directory entry with more than one link, count the links found in the project by `(dev, ino)`. The file is confined only if that count equals its link count. Anything else refuses the launch.
2. **Fail closed.** Refuse if the 200,000-entry limit is reached, or if any folder, entry or metadata read fails. Refuse if the project fails validation.
3. **One enforcement point.** The check runs in `start_verified`, after `validated` and before any session folder is created. Every agent launch passes through it, so no caller can skip it.
4. **Early check.** The pane runs the same function first, so a user is not asked to log in to a project that would be refused. This is for the user's benefit. It is not the control.
5. **No bypass.** There is no confirm click, flag or setting. Login is not refused, because it runs no agent and does not use the project as its working folder.
6. **Scope.** Only the project is scanned. A link from the project into the session's own profile or temp counts as outside, which errs towards refusal. If a real CLI makes such links in normal use, that false refusal comes back to the Architect. The rule would then be widened to count that tool's own profile, and no further.

**Consequences.**

- Some real projects are refused: those over 200,000 entries, those with package-store links, and local `git clone` copies with hardlinked objects. The trial uses a new empty folder, so it is unaffected.
- A concurrent writer can cause a false refusal during the scan, for example the other session creating or deleting files. Trying again clears it.
- A refused pane still offers Log in. It runs no agent and its Confirm step states the reset, but the login is wasted until the project changes. Hiding it is a next step, not part of this slice.
- From a refused pane, the way out is Choose, which needs the other pane stopped first.
- The per-entry read error branch fails closed by source review. No test reaches it, because forcing one needs a seam.

**Residual limits. This is a launch check, not a lifetime guarantee.**

- A process outside the sandbox can create an outside link after the scan, or between the scan and launch. Writing through that link would then succeed. The next launch refuses it.
- A session can link files between its own permitted roots while running, for example from its profile into the shared project, where the other session can write. That needs the first session's cooperation and reaches no file outside the two sessions' roots. The next launch of either session refuses it.
- The scan does not cover the profile or session temp. A session cannot link an outside file there, and login writes only to a new, empty profile.
