# 0037: Packages declare the tools they need; Game Dev (Godot) is built in

Status: Accepted, built in `connecto-hook/src/requires.rs` (the schema),
`connecto-packages/src/requires.rs` (detection), the packages engine and facade,
the Project Settings dialog (`connecto-ui/src/project_settings.rs`) and the
remote asks (`connecto-net/src/project_panels.rs`). Tested with fake searches
and a fake host. It has not been tried end to end on a machine without Godot.
The research behind it is `research_notes/gaming-package/research.md`.

## Context

Some skills are useless without a program on the machine: a game engine, a
3D tool, a CLI. Until now a package was only files copied into a project, so
a project could take a skill that could never work. The user wanted a package
to say what it needs. When a project picks the package, Connecto should
either install what is missing (once the user confirms) or explain how to
install it, and the package must stay off if the user refuses. Connecto must
never accept a EULA or licence on the user's behalf.

The user also wanted a game development package built in. They asked for
**one engine only**, the best for building games with coding agents.

## Decision

1. **`requires` in `package.json` belongs to the whole package.** Each entry
   is one tool and has:
   - an `id` and a `name`, and optionally `why`;
   - `detect`: `programs` (plain names looked up on `PATH` and the usual
     install folders, such as `/opt/homebrew/bin` and Flatpak's exports);
     `paths` per system (absolute, `~/…`, or on Windows `%VAR%\…`);
     optionally `version: {flag, min, below}` (`below` is exclusive, so
     `"min": "4.7", "below": "5"` means any 4.x from 4.7 on);
   - `install`: one command line per system (`macos`, `windows`, `linux`);
   - `manual` (steps to follow by hand) and a `url` (`https://`);
   - `licence: true` when getting the tool means accepting a licence or EULA,
     or signing in.

   Checking per skill was considered and rejected. It would need per-project
   skill choices that are saved, sent to remote views and applied by the
   installer, all for little gain.

2. **Detection runs nothing a package chooses.** It looks for files. The
   only program it ever runs is the one it found, with one flag from a fixed
   list (`--version`, `-version`, `-v`, `-V`: each starts with `-`, so no
   program reads it as a file), in `/` (Windows: the system folder), with a
   15-second limit. Its output goes to a private temporary file, not a pipe,
   so a helper it leaves running cannot hold the probe or a reading thread,
   and on Unix the probe leads its own process group, which is ended when
   it is done. A relative `PATH` entry is made absolute first, so the
   program checked is the one run. It is run under the name it was found by, because
   a launcher such as Snap's tells its tools apart by name; whether it is
   trusted is decided by where that name really leads. Program names cannot be paths, and paths cannot climb out with
   `..`. A place whose real path (links resolved) is inside Connecto Home,
   Connecto's data or an open project is never found nor run: that is where
   packages, and what they install, are kept, so a package cannot point
   detection at a program it brought along. When only some versions will
   do, a place counts only once its version is read (the program ran and
   exited 0) and is in range. Every place is tried before the tool is said
   to be the wrong version, or one whose version cannot be read. Detection runs on a thread of its own when Connecto starts, when
   a package's `requires` changes, and when the user asks to check again.
   The view shows each tool as checking, found (with its version), missing,
   the wrong version, or a version that cannot be read.

3. **Install means a visible shell terminal, and only after a yes.** The
   dialog shows the exact command. "Install…" asks again ("Connecto will run
   this in a new shell terminal of this project"), and only "Run it" opens a
   shell terminal named "Install <tool>" that types the command once. The
   user sees it work and answers any `sudo` or Homebrew prompt there. The
   command always comes from the package on the machine that runs it, never
   from the window that asked. The request names the package that declares
   the tool (the added package or one it extends), the tool, and the command
   as the user saw it. The host runs its own command, and only if it is
   still exactly that text, so a package changed since the dialog was drawn
   runs nothing. A command travels to a remote window whole (up to the 600
   characters a manifest allows). A command that cannot be shown exactly
   (control or direction-changing characters) is refused in the manifest and
   not offered remotely.

4. **A licence tool gets manual steps only.** A package that gives
   `install` for a `licence: true` tool fails validation. The dialog says
   that Connecto never accepts terms for the user, and shows the steps and
   the link.

5. **Refused means not enabled.** `set_project_settings` refuses settings
   that add a package, or turn on a default package, while a tool it (or a
   package it extends) needs is missing, too old or still being checked.
   `set_default_package` refuses such a package too. The window does not
   save in that case: it opens the dialog, whose **Add** button stays off
   until every tool is found. **Cancel** leaves the package off. Because the
   view's answer may be old, the engine looks again for the tools of each
   package being added (and of what it extends) before it saves. If one is
   missing, the settings are not saved and the project's status shows why;
   that covers remote saves too. Making a package the default looks again
   the same way; if refused, the default stays what it was. A package
   the project already has is not checked again, so uninstalling a tool
   later does not lock the project's settings. The installer itself does not
   gate on tools, because at startup they are still being checked.

6. **It works remotely.** The settings answer carries each package's
   `requires` with its state. Two new project asks,
   `check-requirements {package}` and
   `install-requirement {package, requirement}`, pass the host's guard like
   every other ask. The host's own facade enforces point 5, so a remote
   save that would add a package with missing tools is refused there.

7. **Game Dev is a second built-in package (`game-dev`), for Godot only.**
   - **Why Godot:** it is the most agent-friendly full engine in the
     research. Scenes and resources are text. It has a real headless
     command line (`--import`, `--check-only`, `--export-release`,
     `--write-movie`), command-line tests (GdUnit4) and a maintained,
     authenticated MIT MCP server (godot-ai). It is free and MIT-licensed,
     needs no account and no EULA, and installs unattended with Homebrew,
     winget or Flatpak.
   - **Why not the others:** Unity needs a sign-in and EULA acceptance.
     Unreal needs the Epic Launcher, a EULA, binary assets and tens of GB.
     Phaser and Three.js are web-only frameworks rather than engines.
   - **What it needs:** Godot 4, from 4.7 (the version godot-ai requires) up
     to but not including 5, because the skills are written for Godot 4;
     Blender 4.2 or later; FFmpeg 4.4 or later (Ubuntu 22.04's). The skills'
     frame commands avoid `-fps_mode` (5.1 and later) and renumber the
     selected frames with `setpts=N/FRAME_RATE/TB`; they were run on 4.4.2
     and 9.0.2 and gave the same frames.
   - **Skills:** `game-design` (a one-page design and milestones checked by
     evidence), `godot-game` (the Godot workflow: start, check headless,
     GdUnit4, godot-ai, export) and `game-playtest` (evidence and a verdict,
     no fixes).
   - **Blender and FFmpeg are required (since package version 3).** Version 2
     left out Blender and other asset tools, because Godot does not need
     them and placeholder art is enough for the milestones. A real game
     built with it (the "squares" project) showed otherwise: every model came
     from Blender scripts run headless (`blender -b -P gen_*.py`, exported as
     `.glb`), and every playtest and visual review of motion pulled frames
     out of `godot --write-movie` videos with FFmpeg, which the skills
     already assumed. The user decided that without Blender the work is
     crippled, so both are checked and installed exactly like Godot, not
     offered as optional. Blender installs with Homebrew's cask, winget
     (`BlenderFoundation.Blender`) or Flatpak (`org.blender.Blender`).
     FFmpeg installs with Homebrew or winget (`Gyan.FFmpeg`). Linux has no
     Flatpak of the FFmpeg command line, so its command is one `sh -c` line
     that uses apt-get, dnf (`ffmpeg-free`, which Fedora ships), pacman or
     zypper, whichever is there, with `sudo` asking for the password in the
     visible terminal, and otherwise prints what to do by hand. Detection
     reads FFmpeg's `-version` from the word after `ffmpeg version` only,
     never from the compiler's version further down: Arch's `n7.1` reads as
     7.1, and a snapshot (`N-…`, or a date) has no version that can be read,
     so it does not count.
   - **Projects that turned version 2 on** are updated to version 3 even
     without the new tools, because only adding a package is gated. The
     project's status then warns of each tool an installed package needs
     and the last look did not find (worked out when the view is built, so
     it goes once the tool is found), and the skills check for Blender and
     FFmpeg before a step that uses them and ask the user to install what
     is missing. The warning says how to install it by hand: the first that
     fits in 400 bytes of this system's install command (ending the line),
     the manual steps, the link, or plain "install it by hand", which always
     fits because the names in it are capped. A remote Project Settings
     keeps 512 bytes of a line, older peers too, and nothing cut short is
     shown (Linux's FFmpeg gets the link). That works however the package came in
     (chosen, the default, or extended by another package, which has no
     Remove for it), then says to restart Connecto, which looks again at
     startup. A dialog for a package already on is not built.
   - **Still left out, and asked for when needed:** GdUnit4, godot-ai and
     the export templates (proposed with the user's OK, as before);
     downloaded art such as CC0 animation packs (the user's OK each time);
     Python with Pillow or numpy (a project virtual environment, since
     Homebrew's Python refuses a global `pip install`); and generated images,
     which come from Codex's own image generation and cannot be installed:
     the skill checks `connecto agents` and falls back to placeholder art.
   - **How it is built:** it has one form for every agent; the installer
     takes the tool folders a project's Tools setting allows. It is not the
     default. It cannot be edited, exported or deleted, and a Connecto Home
     folder named `game-dev` is not read. `tools/vendor_builtin.py` carries
     it along and makes its `.agents/skills` from `.claude/skills`.

8. **A shell terminal started with a first prompt types it once,** as one
   plain command line followed by Enter. Text with several lines or control
   characters is never typed.

## Consequences

- A package from someone else can still name any install command. The user
  sees that exact command and must say yes twice, and it runs where the user
  can watch and stop it.
- On Windows, the program names winget installs are not confirmed, so
  Godot is found on `PATH` or in winget's `Links` folder, if it is there.
  Otherwise the user installs it by hand and checks again. FFmpeg from
  winget is a portable zip behind a `Links` shim (the user's, or
  `%ProgramFiles%\WinGet\Links` for a machine-wide install), and both are
  listed. When winget cannot make the link it adds the package's folder to
  the saved `Path` instead, which a running app does not see; so each look
  builds its search again and, on Windows, adds the machine's and the
  user's `Path` as saved in the registry. Blender from winget is a machine-wide MSI in a folder named after
  its version (`Program Files\Blender Foundation\Blender 5.3`), and detection
  has no wildcards, so the package lists the folders from 4.2 to 6.0 (12,
  the most a list may hold). A later Blender needs that list updated.
- The Linux FFmpeg command differs by distribution and may give a version
  older than 4.4 on an old release; detection then says so.
- A user package with the id `game-dev` is hidden by the built-in one.
- New built-in packages are added by giving `BUILTIN_IDS` in `connecto-hook`
  another id, with a folder under `builtin/` of the same name. A test checks
  that the list stays in step.
