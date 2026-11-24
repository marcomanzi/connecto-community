#!/usr/bin/python3
"""A stand-in for Antigravity CLI (`agy`) in a PTY, for the launch tests.

It does what the documentation says `agy` does with hooks, and nothing more: it
reads the named entries of `~/.gemini/config/hooks.json`, runs each handler of
an event through `sh -c` with Antigravity's camelCase payload on stdin, and
reads the handler's answer as JSON. Its conversation is a `transcript.jsonl`
under `~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/`.

  --version            print the version and exit
  a typed line         one turn: two model calls (PreInvocation twice, the
                       same initialNumSteps), a tool step, then the reply
                       `REPLY <line>` and Stop. A line that says CANCEL stops
                       with USER_CANCELED and no reply.

Everything the hooks answered is written to `answers.log` in the working
folder, one JSON line per handler run.
"""
import json, os, subprocess, sys

if "--version" in sys.argv:
    print("1.5.11")
    sys.exit(0)

HOME = os.environ["HOME"]
CONV = "ec33ebf9-0cba-4100-8142-c61503f6c587"
APP = os.path.join(HOME, ".gemini", "antigravity-cli")
LOGS = os.path.join(APP, "brain", CONV, ".system_generated", "logs")
TRANSCRIPT = os.path.join(LOGS, "transcript.jsonl")
os.makedirs(LOGS, exist_ok=True)
steps = 0


def handlers(event):
    path = os.path.join(HOME, ".gemini", "config", "hooks.json")
    try:
        with open(path) as f:
            config = json.load(f)
    except (OSError, ValueError):
        return []
    found = []
    for name, spec in config.items():
        if not isinstance(spec, dict) or spec.get("enabled", True) is False:
            continue
        for handler in spec.get(event, []):
            if "command" in handler:
                found.append(handler["command"])
    return found


def fire(event, **extra):
    payload = {
        "conversationId": CONV,
        "workspacePaths": [os.getcwd()],
        "transcriptPath": TRANSCRIPT,
        "artifactDirectoryPath": os.path.join(APP, "brain", CONV),
        "modelName": "auto",
        "hookEventName": event,
        "cwd": os.getcwd(),
    }
    payload.update(extra)
    for command in handlers(event):
        out = subprocess.run(
            ["sh", "-c", command],
            input=json.dumps(payload).encode(),
            capture_output=True,
            timeout=30,
        )
        try:
            answer = json.loads(out.stdout.decode() or "null")
        except ValueError:
            answer = {"not_json": out.stdout.decode()}
        with open("answers.log", "a") as log:
            log.write(json.dumps({"event": event, "answer": answer}) + "\n")


def record(**step):
    global steps
    step["step_index"] = steps
    steps += 1
    with open(TRANSCRIPT, "a") as f:
        f.write(json.dumps(step) + "\n")


fire("SessionStart")
print("agy ready", flush=True)
for line in sys.stdin:
    prompt = line.strip()
    if not prompt:
        continue
    start = steps
    record(type="USER_INPUT", status="DONE", content=prompt)
    fire("PreInvocation", invocationNum=0, initialNumSteps=start, lastUserInput=prompt)
    if "CANCEL" in prompt:
        fire("Stop", executionNum=0, terminationReason="USER_CANCELED", fullyIdle=True,
             lastUserInput=prompt)
        continue
    record(type="PLANNER_RESPONSE", status="DONE", content="",
           tool_calls=[{"name": "run_command", "args": {}}])
    record(type="GENERIC", status="DONE")
    fire("PreInvocation", invocationNum=1, initialNumSteps=start, lastUserInput=prompt)
    record(type="PLANNER_RESPONSE", status="DONE", content="REPLY " + prompt, tool_calls=[])
    fire("Stop", executionNum=0, terminationReason="NO_TOOL_CALL", fullyIdle=True,
         lastUserInput=prompt)
    print("done: " + prompt, flush=True)
