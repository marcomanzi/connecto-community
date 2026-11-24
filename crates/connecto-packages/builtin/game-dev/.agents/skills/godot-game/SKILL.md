---
name: godot-game
description: Build a game with Godot 4.7 (GDScript) - start the project, edit scenes and scripts, check them headless, run tests (a plain headless runner or GdUnit4), bring in Blender-made art, drive the editor through the godot-ai MCP server, and export. Use when the project has a project.godot, or the user mentions Godot, GDScript, .tscn or .tres files, or wants to start making a game.
---

# Godot games

Godot suits coding agents: scenes (`.tscn`), resources (`.tres`) and `project.godot` are text, and the engine has a headless command line for importing, checking, testing and exporting. Use GDScript with static types (`var speed: float = 200.0`, `func jump() -> void:`). C# needs the separate .NET build of Godot and the .NET SDK, so use it only if the user asks for it.

## Find Godot

Connecto checks that Godot 4.7 or a later 4.x is installed before this package is enabled. These instructions are for Godot 4; a Godot 5 is not used. Use the first of these that works, and write it down in the project's agent notes (see below):

- `godot` on the PATH (Homebrew on macOS links it there);
- `/Applications/Godot.app/Contents/MacOS/Godot` on macOS;
- `flatpak run org.godotengine.Godot` on Linux, for a Flatpak install.

Below, `godot` stands for whichever one you found. Check it with `godot --version`.

## Start a new game

1. If there is no `design/gdd.md`, use the `game-design` skill first.
2. Create `project.godot` in the project's root with `config_version=5`, the game's name under `[application]` (`config/name`) and its main scene (`run/main_scene`), plus folders such as `scenes/`, `scripts/`, `assets/` and `tests/`.
3. Run `godot --headless --path . --import` once, so Godot builds its import cache and gives the resources their UIDs.
4. Write the build, check, test, run and export commands below into the project's `CLAUDE.md` or `AGENTS.md`, with the Godot path you found, which files must not be touched (`.godot/`), and the art and audio conventions. Keep that file current as the game grows: a table of the exact commands (each copied from a run that worked), the folder layout, the conventions, and which folders belong to which session. It is the only memory the next session has.
5. Add `.godot/` to `.gitignore`. It is Godot's cache.
6. Keep everything that is not a game file out of the importer: put an empty `.gdignore` file in folders of art sources, generator scripts, screenshots and recordings, and if there are `.blend` files in the project set `import/blender/enabled=false` under `[filesystem]` in `project.godot`. Otherwise Godot tries to import them, and for `.blend` files it tries to start Blender.

## Work on the game

- Edit scripts freely. Edit `.tscn` and `.tres` as text only for small changes (a property, a node). For node trees, signals, materials and animation, use the editor through godot-ai (below) when it is set up.
- Never edit anything in `.godot/`.
- After adding or moving files, run `godot --headless --path . --import` again, so the UIDs stay consistent.
- Godot **ignores unknown command-line flags without a word**, so a typo does nothing. Copy the flags from this page.

## Structure that keeps the game testable

- **Rules apart from scenes.** Write the rules (inventory, damage, crop growth, crafting, save format) as plain classes that extend `RefCounted` and need no scene, so a unit test can call them. Scene scripts stay thin: they show the state and forward input.
- **Data as resources.** One `.tres` per item, enemy, attack or recipe, each with a stable id. The ids end up in save files, so never rename one. Every timing and cost a designer will tune lives in these files, not in code or in animation lengths.
- **Small components** under an actor (health, stamina, inventory...) instead of one large player script. Other systems reach the player through a group and use its components.
- **State that outlives a scene** (inventory, world flags, the current run) lives in an autoload, not in scene nodes.
- **Commit an action when the button is pressed.** An attack or tool use that lands later must keep the item, the target and the facing it had at the press. Reading them again at impact lets the player switch item or turn during the swing.
- **Models behind a small interface** such as `play(name, speed)`, so a placeholder and the finished model are interchangeable (see `team.md` in the `game-design` skill).

Before building a system that is new to you, read `gotchas.md` in this skill's folder: mistakes that cost a real project hours, each with its cause and its fix.

## Check every change

Run these after each change, before saying it works:

1. `godot --headless --path . --import`
2. Parse a script without running it: `godot --headless --path . -s res://path/to/script.gd --check-only`. This works only for a script that names no autoload: for one that does, it reports a false `Identifier not found`, because autoloads do not exist yet when it compiles. Check those scripts with the tests and a smoke run.
3. The tests (below).
4. A smoke run of the game itself: `godot --path . --quit-after 300 --log-file <path>`, then search the log for `ERROR`.

**Do not trust exit code 0.** Headless Godot can exit 0 after something failed: a script given to `-s` that does not compile prints `SCRIPT ERROR` and still exits 0. Read the output, a log written with `--log-file <path>`, and the test reports, and look for errors in them.

A project theme set in `project.godot` (`gui/theme/custom`) that uses textures fails to load on a clean import, because Godot loads it before the textures have been imported. Set the theme on the root node of each UI instead. Test a clean import now and then: copy the project without `.godot/` and run step 1 there.

## Tests

Two ways. Start with the first; it needs nothing installed.

### A plain runner

A test runner is about ninety lines of GDScript run with `godot --headless --path . -s res://tests/run_tests.gd`. `test-runner.md` in this skill's folder has one ready to copy, with what makes its result trustworthy: it fails when the engine logs any error (at startup, while a test is created or while it runs), when a test file does not extend its `TestCase`, and when it finds no test at all. Its last line is the verdict, `TESTS RESULT: PASS <n>` or `TESTS RESULT: FAIL <n>`. A run passed only if Godot exited 0 and printed that `PASS` line last; no verdict (a game that quit on its own, a crash, a hang) is a failure. Always run it with a time limit, your tool's command timeout or `timeout`: a test that blocks the main thread stops every timer inside Godot. A script given to `-s` cannot name an autoload or a class that uses one (it compiles before they exist); the same file shows how to load such code at run time.

### GdUnit4

Use it when the user wants its reports, mocks or editor panel. GdUnit4 v6 needs Godot 4.5 or later. It is installed into the project as the `addons/gdUnit4` add-on (from the Godot Asset Library in the editor, or from its GitHub repository). Ask the user before adding it.

- Write tests under `tests/`.
- Run them with the add-on's command-line runner, with the `GODOT_BIN` environment variable set to the Godot program: `./addons/gdUnit4/runtest.sh -a res://tests` on macOS and Linux, `addons\gdUnit4\runtest.cmd -a res://tests` on Windows.
- It writes HTML and JUnit reports. Read the JUnit report to see what failed; the exit code alone is not enough.

Docs: https://github.com/godot-gdunit-labs/gdUnit4/blob/master/documentation/doc/_advanced_testing/cmd.md

## Run the game and gather evidence

- Run it: `godot --path .` (add `--quit-after <frames>` to stop it by itself).
- Record a video, frame-exact: `godot --path . --write-movie out.avi --fixed-fps 60 --quit-after 600`. It needs a real display, so leave out `--headless`. On macOS keep the game window visible: a hidden window is not drawn.
- Pull frames out of the video to look at them: `ffmpeg -i out.avi -vf "select='between(n\,120\,125)',setpts=N/FRAME_RATE/TB" frame_%02d.png` (frames 120 to 125; `setpts` numbers them again, so none is repeated, on FFmpeg 4.4 and later).
- Pass your own options to a script after ` -- ` (`godot --path . -s res://tests/shots.gd -- --out=/abs/dir`) and read them with `OS.get_cmdline_user_args()`.
- Exact API docs for this Godot version: `godot --doctool <folder>` dumps the engine's API as XML. Read it instead of guessing an API from memory.
- The `game-playtest` skill says how to judge what you gathered.

## Art from Blender

Models, kits and characters made in Blender (by hand or by script) come in as `.glb`. `art-pipeline.md` in this skill's folder covers the route: where sources live, how to check an import, the importer's quirks, triangle budgets, characters and animation, and how to review art before showing it. Connecto checks for Blender and FFmpeg with Godot before this package is turned on, so use them without asking; a project that turned it on before version 3 may lack them, so check first as `art-pipeline.md` says. Downloading art (animation packs, models, textures), Python packages and generated images are covered there too: a download still needs the user's OK.

## The editor through godot-ai (optional)

[godot-ai](https://github.com/hi-godot/godot-ai) is an MIT-licensed MCP server for Godot 4.7 or later. It lets an agent edit scenes and nodes, run the game (`project_run`), read the editor and game logs (`logs_read`), take screenshots of the editor or the running game (`editor_screenshot`) and run tests (`test_run`). It is worth setting up for anything beyond scripts. Ask the user before installing it, and tell them:

- It needs `uv` (`uv --version`; on macOS `brew install uv`).
- It is an editor add-on: from the Asset Library, or from the GitHub release's `godot-ai-plugin.zip`, into `addons/godot_ai/`. The user enables it under Project > Project Settings > Plugins, then presses **Configure** in its dock to connect their agents.
- By default it writes the agents' MCP settings for the user's whole account, which changes other projects' settings too. Setting `godot_ai/mcp_client_scope` to `project` keeps them in this project.
- It sends anonymous usage counts by default.
- It listens on 127.0.0.1 ports 8000 and 9500. Port 8000 clashes with Unreal's MCP server.
- Never unzip a new version over an existing `addons/godot_ai/`: update it from its dock.
- GDScript is fully supported. C# is text only: no build and no compiler errors.

Without it, work from the command line as above.

## Export

1. The export templates must be installed (about 1.2 GB for 4.7.2): in the editor, Editor > Manage Export Templates. Ask the user before downloading them.
2. The presets live in `export_presets.cfg`. Make them in the editor (Project > Export) or as text.
3. Export with `godot --headless --path . --export-release "<preset name>" <absolute output path>`. `--export-debug` and `--export-pack` work the same way.
4. Check that the output file exists and read the log. Exit code 0 is not proof.

## Sources

Godot command line: https://docs.godotengine.org/en/stable/tutorials/editor/command_line_tutorial.html
