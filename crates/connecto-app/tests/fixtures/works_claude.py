#!/usr/bin/env python3
"""Deterministic project CLI for Agentic Works integration tests."""
import json
import os
import pathlib
import subprocess
import sys
import time

if "--version" in sys.argv:
    print("2.1.318 (fixture)")
    sys.exit(0)
if sys.argv[1:2] == ["auth"]:
    print(json.dumps({"loggedIn": True, "authMethod": "fixture"}))
    sys.exit(0)

prompt = sys.argv[-1]
terminal = os.environ["CONNECTO_TERMINAL"]
session = "11111111-aaaa"
log = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"]) / (terminal + ".jsonl")

def hook(event, **fields):
    payload = dict(fields, hook_event_name=event, session_id=session)
    subprocess.run(["connecto-hook", "claude", "--event", event],
                   input=json.dumps(payload).encode(), check=True)

def record(entry):
    with log.open("a") as stream:
        stream.write(json.dumps(entry) + "\n")

hook("SessionStart", transcript_path=str(log))
record({"type": "user", "message": {"role": "user", "content": prompt}})
# EARLY-ASK: the agent asks before Connecto hears its turn began, so the
# terminal still looks idle to Connecto while that turn is on.
if "EARLY-ASK" not in prompt:
    hook("UserPromptSubmit", prompt=prompt)
MARK = "CONNECTO-RUN:"
# A plan for this very terminal, left by the test before a restart.
PLAN = pathlib.Path("plans") / (terminal + ".json")
if PLAN.is_file() or MARK in prompt:
    # Run `connecto` commands as an agent would, keep what each printed, and
    # answer with the plan's reply (or the usual one).
    if PLAN.is_file():
        plan = json.loads(PLAN.read_text())
        PLAN.unlink()
    else:
        plan, _ = json.JSONDecoder().raw_decode(prompt[prompt.index(MARK) + len(MARK):])
    runs = []
    for run in plan.get("runs", []):
        done = subprocess.run(["connecto"] + run["args"], input=run.get("stdin", "").encode(),
                              capture_output=True)
        runs.append({"args": run["args"], "code": done.returncode,
                     "out": done.stdout.decode(errors="replace"),
                     "err": done.stderr.decode(errors="replace")})
    with open("runs-" + terminal + ".tmp", "w") as stream:
        json.dump(runs, stream)
    os.replace("runs-" + terminal + ".tmp", "runs-" + terminal + ".json")
    time.sleep(plan.get("hold", 0))
    answer = plan.get("reply", {"answer": 42})
elif "ASK-USER:" in prompt:
    # A job that misses a piece of data: ask the user, end the turn, and
    # answer with what the next prompt's reply says (ADR 0040).
    import termios, tty
    question = prompt.split("ASK-USER:", 1)[1].split("\n", 1)[0].strip()
    # Asked once, as soon as the agent runs: never asked again.
    asked = subprocess.run(["connecto", "works", "ask", question], capture_output=True)
    if "EARLY-ASK" in prompt:
        # The user answers meanwhile; then Connecto hears the turn, which
        # ends with a guess.
        time.sleep(4)
        hook("UserPromptSubmit", prompt=prompt)
    with open("asked-" + terminal + ".json", "w") as stream:
        json.dump({"code": asked.returncode, "out": asked.stdout.decode(errors="replace"),
                   "err": asked.stderr.decode(errors="replace")}, stream)
    # WRONG-FIRST: the asking turn ends with valid JSON anyway (a guess).
    waiting = (json.dumps({"answer": "unknown"}) if "WRONG-FIRST" in prompt
               else "Asked the user; waiting for the answer.")
    record({"type": "assistant", "message": {"role": "assistant", "stop_reason": "end_turn",
                                            "content": [{"type": "text", "text": waiting}]}})
    hook("Stop", background_tasks=[], last_assistant_message=waiting)
    if "EARLY-ASK" in prompt:
        # Idle a while before the next prompt, as a CLI between turns.
        time.sleep(2)
    # The next prompt: pasted (bracketed), then Enter.
    fd = sys.stdin.fileno()
    tty.setraw(fd, termios.TCSANOW)
    sys.stdout.write("\x1b[?2004h> ")
    sys.stdout.flush()
    buf = b""
    while True:
        buf += os.read(fd, 65536)
        # Enter, or a newline when it came while the terminal was not raw.
        if buf.count(b"\x1b[200~") == buf.count(b"\x1b[201~") and buf.endswith((b"\r", b"\n")):
            break
    woke = buf.replace(b"\x1b[200~", b"").replace(b"\x1b[201~", b"").decode(errors="replace").strip()
    record({"type": "user", "message": {"role": "user", "content": woke}})
    hook("UserPromptSubmit", prompt=woke)
    got = subprocess.run(["connecto", "works", "answer"], capture_output=True)
    reply = got.stdout.decode(errors="replace")
    with open("answered-" + terminal + ".json", "w") as stream:
        json.dump({"woke": woke, "code": got.returncode, "out": reply,
                   "err": got.stderr.decode(errors="replace")}, stream)
    if "BACKGROUND-FIRST" in prompt:
        # The turn that read the answer stops with valid interim JSON while
        # a background task still runs (no turn ended yet); the task reports
        # back later and that turn ends with the final JSON.
        interim = json.dumps({"answer": "interim"})
        record({"type": "assistant", "message": {"role": "assistant", "stop_reason": "end_turn",
                                                "content": [{"type": "text", "text": interim}]}})
        hook("Stop", background_tasks=[{"id": "b1"}], last_assistant_message=interim)
        time.sleep(4)
        note = "<task-notification><task-id>b1</task-id>done</task-notification>"
        record({"type": "user", "message": {"role": "user", "content": note}})
        hook("UserPromptSubmit", prompt=note)
    if "SILENT-AFTER" in prompt:
        # The turn after the answer (after its background work, with
        # BACKGROUND-FIRST) ends with no reply at all.
        hook("Stop", background_tasks=[])
        time.sleep(100)
        sys.exit(0)
    if "---\n" in reply:
        answer = {"answer": reply.split("---\n")[1].strip()}
    else:
        answer = {"answer": "missing", "why": reply.strip()}
elif "Help me define" in prompt:
    answer = {"instructions": "Review {{prompt}}", "schema": {"type": "object", "required": ["answer"]}}
else:
    with open("dispatch.jsonl", "a") as stream:
        stream.write(json.dumps({"args": sys.argv[1:], "prompt": prompt}) + "\n")
    if "wait-for-cancel" in prompt:
        time.sleep(100)
    answer = {"answer": 42}
record({"type":"assistant", "message":{"role":"assistant","stop_reason":"end_turn",
                                      "content":[{"type":"text","text":json.dumps(answer)}]}})
hook("Stop", background_tasks=[], last_assistant_message=json.dumps(answer))
time.sleep(100)
