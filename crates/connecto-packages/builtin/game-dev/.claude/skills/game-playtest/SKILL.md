---
name: game-playtest
description: Playtest a Godot game - run it with fixed seeds and scripted input, capture screenshots, video and logs, look at them, and judge the game against the design's acceptance criteria, filing bugs instead of fixing them. Use when the user asks to test or playtest the game, asks whether it works, or after a milestone or slice is built.
---

# Playtest: from "it compiles" to "it plays"

Passing a build or a test does not prove a game plays. A playtest runs the game, gathers evidence, looks at it, and compares it with the acceptance criteria in `design/gdd.md`. **The playtester reports; it does not fix.** Keep the two apart, so nobody grades their own work.

## 1. Make runs repeatable

- Seed every random generator from one place, with a fixed seed during tests.
- Use a fixed timestep for gameplay (`_physics_process`), and run with `--fixed-fps 60`, headless runs included.
- Count every wait in physics ticks, in the bot and in the game (hit-stop, cooldowns the bot depends on), never in wall-clock time. A bot that waits "0.3 seconds" plays differently on a loaded machine, and a failure cannot be replayed.
- Give each test process its own save file and never touch the player's real one. Playtests run in parallel, and with a shared slot they corrupt each other.
- Add an autoload, for example `DebugState`, that exposes the state a test needs (player position, health, score, current level) and can print it as one line of JSON.
- Drive the game with scripted input, not by hand.

## 2. A bot that plays like a player

The end-to-end check is a bot that replays the milestone's acceptance scenario from the title screen. `bot.md` in this skill's folder has a harness ready to copy: fixed ticks, real key events, a timeout, and failure on any logged error.

- **Real input only.** Send key and mouse events (`Input.parse_input_event`), so the input map, the menus and the focus are tested too. Move by walking. A bot that sets positions or calls game functions proves that the functions work, not that the game can be played.
- **Fail on any engine or script error and on a hang.** Exit code 0 means nothing by itself: a null call prints an error and the run goes on. Always launch the bot with a time limit (your tool's command timeout, or `timeout`): a stuck game stops every timer inside Godot.
- **Print one clear last line** (`PLAYTEST PASSED` or `PLAYTEST FAILED: <reason>`) and exit 1 on failure. A run passed only if Godot exited 0 and that last line is `PLAYTEST PASSED`; a missing line (the game quit on its own, a crash, the time limit) is a failure.
- **"Can it be finished" is a separate run from "is it fair".** For the structural run, a test-only assist may keep the bot alive (healing it, never changing the rules), while the damage it took is still logged per enemy. That table is what the user reads when they judge the balance by playing. Never let the assist into a run that checks difficulty.

## 3. Gather evidence

- **Logs**: run with `--log-file <path>` and read the whole log. Errors and warnings count, even when the exit code is 0. Headless Godot can exit 0 after something failed.
- **State**: the `DebugState` lines over the run.
- **Video**: first check that ffmpeg is there: `command -v ffmpeg` (PowerShell: `Get-Command ffmpeg`). A project that turned Game Dev on before version 3 may lack it: then tell the user to install it (the project's status in Project Settings says how on their system; Connecto finds it after a restart) and gather the other evidence without video. Record with `godot --path . --write-movie out.avi --fixed-fps 60 --quit-after 600` (no `--headless`: it needs a display, and on macOS a visible window). Pull the frames you need with ffmpeg (see `godot-game`).
- **Screenshots**: with godot-ai, `editor_screenshot` of the running game. Without it, a debug script can save the viewport: `get_viewport().get_texture().get_image().save_png("user://shot.png")`. A headless run renders nothing, so it has no screenshots.
- **Tests**: the output of the project's test runner, or the GdUnit4 JUnit report (see `godot-game`).

## 4. Look, then judge

- Open the screenshots and frames and look at them. Is the player on screen? Is the UI readable? Is anything missing, overlapping or the wrong size?
- Look for what only eyes find: flickering surfaces, effects at the wrong place, scenery covering the player, text overlapping at another window size, a fallback system font, things passing through each other.
- Judge motion on **consecutive frames**, not on stills: an animation pop or a prop that appears for two frames is invisible in a single screenshot.
- Check every acceptance criterion of the current milestone: met or not met, with the evidence (file and line, frame or screenshot). Where the criterion is a range, check both ends: a boss that dies too fast fails it as much as one that takes too long.
- Note the frame rate and anything that stutters, the first time each enemy or effect appears above all.

## 5. Do not trust a green test

Tests that pass for the wrong reason are common in games. Read the tests you are relying on, and look for:

- the bot teleporting or calling a function where the player would walk or press a key;
- a check that passes after one step of something that takes several (one hit on a rock that needs four);
- an assertion on an object that was already freed, or on a value read before the action finished;
- only an upper bound, or only a lower one;
- a wait long enough that the thing would have happened anyway.

To know that a regression test protects anything, break the behaviour for a moment and see the test fail. Report a test that cannot fail as a bug.

## 6. Play to break it

Running the scenario is half the job. Then play as someone who wants to cheat, through the same scripted input:

- mash one button through a fight; trade hits and heal instead of defending;
- attack, lock on or pick up through a wall;
- act with nothing left: zero stamina, an empty tool, a full inventory;
- change the selected item or turn around between the button press and the impact;
- die, pause, open a menu or leave during a transition between scenes;
- save and load around every gain: do items or money duplicate? does anything not come back?
- damage the save file: every field missing, of the wrong type, out of range, an unknown id. The file must be refused and the running game left untouched;
- complete an objective the wrong way (buy the thing you were meant to craft).

If the intended way to play is not also the most effective one, that is a finding, even when every test passes.

## 7. Report

- Write each bug to `playtest/bugs.md`: what happened, what was expected, the steps and seed to reproduce it, and the evidence.
- Reply with a short verdict: the criteria met and not met, the bugs found, and what to fix first.
- When other sessions are working in the project, say which failures come from files outside the change you were asked to judge, instead of counting them against it.
- Hand the fixes to whoever builds the game (with the `godot-game` skill), then playtest again.
