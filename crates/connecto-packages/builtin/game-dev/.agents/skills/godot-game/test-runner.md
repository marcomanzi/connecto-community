# A test runner without add-ons

Three small files under `tests/`. Checked on Godot 4.7.2; the `Logger` class they use needs Godot 4.5 or later. After adding them run `godot --headless --path . --import` once, so the `class_name`s are known.

Run: `godot --headless --path . -s res://tests/run_tests.gd`, always with a time limit (see "Judge a run" below). It fails on any failed test, on an engine error at startup, on a test file that does not extend `TestCase`, and when it finds no test. Its last line is the verdict: `TESTS RESULT: PASS <tests>` or `TESTS RESULT: FAIL <failures>`. Write the command and the rule for judging it into the project's agent notes.

## Judge a run

A run passed only if **Godot exited 0 and the last `TESTS RESULT:` line it printed is `TESTS RESULT: PASS <n>`**. Anything else is a failure: exit code 1, a `FAIL` line, no verdict line at all, a time limit reached.

- **Always launch it with a time limit**, the command timeout of your tool, or `timeout 600 godot ...` where the system has it. A test that blocks the main thread (an endless loop, a long `OS.delay_msec`) stops every timer inside Godot, so only the command that launched Godot can end that run. A run ended by the time limit is a failure, and so is the test that was running.
- **A missing verdict is a failure even with exit code 0.** Game code that calls `get_tree().quit()` during a test (a Quit button, a game-over screen) ends Godot with exit code 0 before the runner can judge anything. A crash or a kill also leaves no verdict.
- Keep the output in a file to read it afterwards: `godot --headless --path . -s res://tests/run_tests.gd > test.log 2>&1; echo "exit $?"`, then `grep '^TESTS RESULT:' test.log | tail -n 1`.

## `tests/error_catcher.gd`

Exit codes and assertions miss most real failures: a null call or a failed load prints an error and the run goes on. This collects them, so the runner (and a playtest bot) can fail on them.

```gdscript
class_name ErrorCatcher
extends Logger
## Collects engine and script errors (not warnings), so a headless run can fail on them.

var errors: Array[String] = []
var _mutex := Mutex.new()


func _log_error(function: String, file: String, line: int, code: String, rationale: String,
		_editor_notify: bool, error_type: int, _backtraces: Array[ScriptBacktrace]) -> void:
	if error_type == Logger.ERROR_TYPE_WARNING:
		return
	_mutex.lock() # errors can come from other threads
	errors.append("%s (%s:%d in %s)" % [rationale if not rationale.is_empty() else code, file, line, function])
	_mutex.unlock()


func take() -> Array[String]:
	_mutex.lock()
	var out := errors.duplicate()
	errors.clear()
	_mutex.unlock()
	return out
```

## `tests/test_case.gd`

```gdscript
class_name TestCase
extends RefCounted

var failures: Array[String] = []
var expected_errors: Array[String] = []


## For a test that provokes an engine error on purpose (substring match).
func expect_error(substring: String) -> void:
	expected_errors.append(substring)


func fail(message: String) -> void:
	failures.append(message)


func assert_true(value: bool, message: String = "expected true") -> void:
	if not value:
		fail(message)


func assert_eq(actual: Variant, expected: Variant, message: String = "") -> void:
	if typeof(actual) != typeof(expected) or actual != expected:
		fail("%s expected <%s> got <%s>" % [message, expected, actual])
```

## `tests/run_tests.gd`

```gdscript
extends SceneTree
## godot --headless --path . -s res://tests/run_tests.gd
## Runs every test_* method of every res://tests/unit/test_*.gd (a TestCase).

const TEST_DIR := "res://tests/unit"
const YIELD_TIMEOUT_SECONDS := 300.0

var catcher := ErrorCatcher.new()
var _finished := false


func _initialize() -> void:
	OS.add_logger(catcher) # now, so errors while the autoloads start are caught too
	# Ends a test that awaits something that never comes. It cannot end a test that
	# blocks the main thread: that is what the time limit of the launching command is for.
	create_timer(YIELD_TIMEOUT_SECONDS, true, false, true).timeout.connect(
			_finish.bind(0, ["the run did not finish in %d seconds" % YIELD_TIMEOUT_SECONDS]))
	process_frame.connect(_run, CONNECT_ONE_SHOT) # one frame later, the autoloads are ready


func _run() -> void:
	var passed := 0
	var failed: Array[String] = []
	for error in catcher.take():
		failed.append("startup: " + error)
	var files := Array(DirAccess.get_files_at(TEST_DIR))
	files.sort()
	for file: String in files:
		if not (file.begins_with("test_") and file.ends_with(".gd")):
			continue
		var script := load(TEST_DIR.path_join(file)) as GDScript
		if script == null or not script.can_instantiate() or not _is_test_case(script):
			failed.append("%s: could not load, or does not extend TestCase" % file)
			continue
		for method in script.get_script_method_list():
			var method_name: String = method["name"]
			if not method_name.begins_with("test_"):
				continue
			var case: TestCase = script.new() # errors in its _init() count for this test
			await case.call(method_name)
			for error in catcher.take():
				if not case.expected_errors.any(func(s: String) -> bool: return error.contains(s)):
					case.fail("runtime error: " + error)
			if case.failures.is_empty():
				passed += 1
			for failure in case.failures:
				failed.append("%s::%s: %s" % [file, method_name, failure])
		for error in catcher.take():
			failed.append("%s: error: %s" % [file, error])
	if passed + failed.size() == 0:
		failed.append("no test found: a test is a test_* method in %s/test_*.gd" % TEST_DIR)
	_finish(passed, failed)


func _is_test_case(script: Script) -> bool:
	while script != null:
		if script == TestCase:
			return true
		script = script.get_base_script()
	return false


func _finish(passed: int, failed: Array) -> void:
	if _finished:
		return
	_finished = true
	OS.remove_logger(catcher)
	for failure in failed:
		print("FAILED: ", failure)
	print("Tests: %d passed, %d failed" % [passed, failed.size()])
	# The verdict, always the last line. No such line means the run did not finish.
	print("TESTS RESULT: PASS %d" % passed if failed.is_empty() else "TESTS RESULT: FAIL %d" % failed.size())
	quit(0 if failed.is_empty() else 1)
```

A test is a file `tests/unit/test_<topic>.gd` that `extends TestCase`, with methods named `test_*`. They may use `await` and autoloads:

```gdscript
extends TestCase

func test_stack_merges() -> void:
	var bag := Inventory.new(4)
	bag.add(&"wood", 3)
	bag.add(&"wood", 2)
	assert_eq(bag.count(&"wood"), 5)
```

## Rules that keep it honest

- **A script given to `-s` compiles before the autoloads exist.** If it names one (or a class that does), Godot prints `Identifier not found`, loads nothing and exits 0. So the runner above names no autoload, and loads the tests with `load()` after the first frame. Do the same for a playtest: a small entry script that calls `load("res://tests/playtest/driver.gd").new()` at run time, with the real work in the driver.
- **Tests never touch the player's save.** Make the save path something a run can override, and give each test process its own file (for example `user://test_saves/test_<process id>.json`, deleted at exit). With one shared slot, two playtests running at once corrupt each other, and a test run wipes the developer's own game.
- **The runner's own timeout covers only tests that yield.** `YIELD_TIMEOUT_SECONDS` ends a test that awaits a signal that never comes and prints a `FAIL` verdict. Keep it shorter than the launching command's time limit, so that this case gets a readable verdict.
- **Turn off time effects in unit tests.** Hit-stop and slow motion change `Engine.time_scale` for everything that runs after them.
- **See a new test fail once.** Break the thing it protects for a moment and run it. A test that still passes is checking something else (see the `game-playtest` skill).
