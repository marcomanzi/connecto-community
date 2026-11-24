# A playtest bot harness

One entry script that Godot runs with `-s`. Checked on Godot 4.7.2. It needs the `ErrorCatcher` class from `test-runner.md` in the `godot-game` skill.

Run it without a window, as the acceptance run:

`godot --headless --path . --fixed-fps 60 -s res://tests/playtest/bot.gd -- --seed=7`

and with a window when you want screenshots:

`godot --path . --fixed-fps 60 -s res://tests/playtest/bot.gd -- --seed=7 --shots=/abs/dir`

Judge a run as the test runner's is judged (see "Judge a run" in `test-runner.md`): it passed only if **Godot exited 0 and the last `PLAYTEST` line it printed is `PLAYTEST PASSED`**. Always launch it with a time limit, the command timeout of your tool or `timeout 600 godot ...`. `TIMEOUT_TICKS` counts physics ticks, so it ends a scenario that waits forever while the game runs, but not a game whose main thread is stuck; only the launching command can end that. A time limit reached, a game that quits on its own, a crash or a missing `PLAYTEST` line are all failures.

Two runs with the same seed must print the same numbers. If they do not, something in the game still depends on wall-clock time or an unseeded random generator: find it before trusting any result.

```gdscript
extends SceneTree
## Names no autoload: a script given to -s compiles before they exist.
## Reach them through the tree (root.get_node("Game")) or load a driver at run time.

const TIMEOUT_TICKS := 60 * 120 # two minutes of game time

var _catcher := ErrorCatcher.new()
var _ticks := 0
var _done := false


func _initialize() -> void:
	OS.add_logger(_catcher)
	physics_frame.connect(_tick)
	_play.call_deferred()


func _tick() -> void:
	_ticks += 1
	if _done:
		return
	var errors := _catcher.take()
	if not errors.is_empty():
		_end("engine/script error: " + str(errors))
	elif _ticks > TIMEOUT_TICKS:
		_end("timed out")


func _play() -> void:
	var shots := ""
	for arg in OS.get_cmdline_user_args():
		if arg.begins_with("--seed="):
			seed(int(arg.trim_prefix("--seed=")))
		elif arg.begins_with("--shots="):
			shots = arg.trim_prefix("--shots=")
	change_scene_to_file("res://scenes/main.tscn")
	await _wait(10)

	# The scenario: one step of the acceptance scenario after another.
	_key(KEY_D, true)
	await _wait(60)
	_key(KEY_D, false)
	await _wait(5)
	var game := root.get_node("Game")
	print("ticks=", _ticks, " score=", game.get("score"))
	if int(game.get("score")) < 90:
		_end("the player did not move right")
		return

	if shots != "":
		if DisplayServer.get_name() == "headless":
			_end("--shots needs a window: run without --headless")
			return
		await RenderingServer.frame_post_draw
		var path := shots.path_join("end.png")
		var saved := root.get_texture().get_image().save_png(path)
		if saved != OK:
			_end("could not save %s: %s" % [path, error_string(saved)])
			return
	_end("")


func _key(code: Key, pressed: bool) -> void:
	var event := InputEventKey.new()
	event.physical_keycode = code
	event.pressed = pressed
	Input.parse_input_event(event)


func _wait(ticks: int) -> void:
	for i in ticks:
		await physics_frame


func _end(failure: String) -> void:
	if _done:
		return
	_done = true
	# Errors logged since the last tick (the final checks included) still fail the run.
	var errors := _catcher.take()
	if failure.is_empty() and not errors.is_empty():
		failure = "engine/script error: " + str(errors)
	OS.remove_logger(_catcher)
	print("PLAYTEST PASSED" if failure.is_empty() else "PLAYTEST FAILED: " + failure)
	quit(0 if failure.is_empty() else 1)
```

## Growing it

- Keep the entry script this small and put the scenario in a driver loaded at run time (`load("res://tests/playtest/driver.gd").new()`), which may then use autoloads and game classes freely.
- Write helpers the way a player thinks: `walk_to(point)` holds the movement keys until the player arrives or a tick limit passes; `press(action)`; `wait_for_scene(path, ticks)`. Every wait has a limit and a failure message.
- Mouse clicks on UI go to the window: build an `InputEventMouseButton` with its position and call `root.push_input(event, true)`.
- After an action that takes time in the game (a harvest, a swing), wait its length in ticks before reading the result.
- Split long scenarios into named ones selectable from the command line (`-- --scenario=boss`), so a failure can be rerun alone.
- Log what a person would want to know afterwards: per fight, the time it took, hits taken and from whom, potions used.
