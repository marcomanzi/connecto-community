#!/usr/bin/python3
"""A stand-in for Claude Code or Codex in a PTY, for the bridge tests.

It reports through the real hook helper, takes pasted text and Enter like a TUI,
and answers each prompt. Behaviour comes from words in the prompt:

  SLEEP <n>     stay in the turn for n seconds
  BACKGROUND    end the turn with background work, then report it done by itself
  NEEDS         raise a notification
  RUN <cmd>     run a shell command (the agent using `connecto`) and put its output in the reply
  NOREPLY       end the turn with no reply text
  BG1           end the turn leaving background work b1 running (no follow-up turn)
  MERGE         a background task's notification is queued into this turn, as Claude
                does: its hook fires and the log gets an attachment record, and the
                turn's one Stop ends both

Boot behaviour comes from files in the working folder, per terminal:

  boot-delay-<id>   seconds to spend starting (MCP servers) before it is ready
  boot-exit-<id>    a message to print, then exit with code 3, before it is ready
  boot-screen-<id>  text to show while it boots (a blocking prompt, say)

Everything it saw is written beside it: typed.log (one JSON per prompt), keys.log,
context-<n>.txt (what the helper printed for each prompt).
"""
import json, os, select, shutil, subprocess, sys, termios, time, tty

TOOL = os.environ.get("FAKE_TOOL", "claude")
ME = os.environ.get("CONNECTO_TERMINAL", "x")
HELPER = shutil.which("connecto-hook") or "connecto-hook"
HOOKED = os.environ.get("FAKE_HOOKS", "1") == "1"
n_prompts = 0
thread = "11111111-aaaa"


def hook(event, payload, context=False):
    if TOOL == "claude":
        args = [HELPER, "claude", "--event", event]
    else:
        args = [HELPER, "codex-hook", "--event", event]
    if context:
        args.append("--context")
    payload = dict(payload, hook_event_name=event)
    out = subprocess.run(args, input=json.dumps(payload).encode(), capture_output=True)
    return out.stdout.decode()


def notify(turn, inputs, reply):
    payload = {"type": "agent-turn-complete", "thread-id": thread, "turn-id": turn,
               "input-messages": inputs, "last-assistant-message": reply}
    subprocess.run([HELPER, "codex", json.dumps(payload)])


inputs_seen = []


def transcript(entry):
    """Claude's transcript: one JSON per line, under the profile."""
    base = os.environ.get("CLAUDE_CONFIG_DIR")
    if TOOL == "claude" and base:
        with open(os.path.join(base, "t.jsonl"), "a") as log:
            log.write(json.dumps(entry) + "\n")


def raw_drain(tag, read=True):
    """Ask the bridge for the inbox directly, as the helper does. With read off
    the socket is closed at once, so the host's write fails."""
    import socket, struct
    body = json.dumps({"v": 1, "op": "drain", "event": "UserPromptSubmit"}).encode()
    frame = struct.pack(">I", 1 + len(body)) + bytes([0]) + body
    sock = socket.socket(socket.AF_UNIX)
    sock.connect(os.environ["CONNECTO_BRIDGE_SOCK"])
    sock.sendall(frame)
    if not read:
        sock.close()
        return
    data = b""
    while True:
        chunk = sock.recv(65536)
        if not chunk:
            break
        data += chunk
    with open("drain-%s-%s.txt" % (ME, tag), "wb") as out:
        out.write(data)


def run_turn(prompt):
    global n_prompts
    n_prompts += 1
    if "DRAINDROP" in prompt:
        raw_drain("x", read=False)
        time.sleep(0.5)
    if "DRAINTWICE" in prompt:
        import threading
        workers = [threading.Thread(target=raw_drain, args=(str(i),)) for i in range(2)]
        for w in workers:
            w.start()
        for w in workers:
            w.join()
    turn = "turn-%d" % n_prompts
    transcript({"type": "user", "message": {"role": "user", "content": prompt}})
    with open("typed-%s.log" % ME, "a") as log:
        log.write(json.dumps({"prompt": prompt}) + "\n")
    print("PROMPT-SEEN: " + prompt.replace("\n", " | "), flush=True)
    sys.stdout.write("\r")
    ctx = hook("UserPromptSubmit", {"prompt": prompt, "turn_id": turn, "session_id": thread}, context=True)
    with open("context-%s-%d.txt" % (ME, n_prompts), "w") as out:
        out.write(ctx)
    inputs_seen.append(prompt)
    words = prompt.split()
    for i, word in enumerate(words):
        if word == "SLEEP":
            time.sleep(float(words[i + 1]))
    if "INTERRUPT" in words:
        # An interrupted Claude turn fires no hook at all; only the transcript says so.
        transcript({"type": "user", "message": {"role": "user", "content": "[Request interrupted by user for tool use]"}})
        return
    if "NEEDS" in words or "NEEDSLONG" in words:
        hook("Notification", {"notification_type": "permission_prompt"})
        # NEEDSLONG stays in the turn after asking, as a tool does while it waits.
        time.sleep(3 if "NEEDSLONG" in words else 0.3)
    reply = "echo: " + prompt.strip().splitlines()[-1][:60]
    if "RUN" in words:
        cmd = prompt.split("RUN", 1)[1].strip().splitlines()[0]
        res = subprocess.run(cmd, shell=True, capture_output=True, text=True)
        with open("runs-%s.log" % ME, "a") as log:
            log.write(json.dumps({"cmd": cmd, "code": res.returncode, "out": res.stdout, "err": res.stderr}) + "\n")
        reply = "ran: exit=%d" % res.returncode
    if "LOSE" in words:
        # The turn-end frame is refused (a wrong token), so it never arrives.
        env = dict(os.environ, CONNECTO_HOOK_TOKEN="wrong")
        subprocess.run([HELPER, "claude", "--event", "Stop"], input=b'{"hook_event_name":"Stop","session_id":"s"}', env=env)
        return
    if "NOREPLY" in words:
        reply = None
    if "MERGE" in words and TOOL == "claude":
        note = "<task-notification><task-id>b1</task-id>done</task-notification>"
        transcript({"type": "attachment", "attachment": {"type": "queued_command", "prompt": note}})
        hook("UserPromptSubmit", {"prompt": note, "session_id": thread}, context=True)
        transcript({"type": "assistant", "message": {"role": "assistant", "stop_reason": "end_turn", "content": []}})
    if TOOL == "claude":
        payload = {"session_id": thread, "background_tasks": []}
        if reply is not None:
            payload["last_assistant_message"] = reply
        if "BACKGROUND" in words or "BG1" in words:
            payload["background_tasks"] = [{"id": "b1"}]
        hook("Stop", payload)
        if "BACKGROUND" in words:
            time.sleep(10.0)
            bg = "<task-notification><task-id>b1</task-id>done</task-notification>"
            hook("UserPromptSubmit", {"prompt": bg, "session_id": thread}, context=True)
            time.sleep(0.2)
            hook("Stop", {"session_id": thread, "background_tasks": [], "last_assistant_message": "background finished"})
    else:
        notify(turn, inputs_seen, reply)


def keys(raw):
    with open("keys-%s.log" % ME, "a") as log:
        log.write(json.dumps(raw.decode(errors="replace") if isinstance(raw, bytes) else raw) + "\n")


if __name__ == "__main__":
    # Pasted text waits in the composer. Enter submits it.
    fd = sys.stdin.fileno()
    tty.setraw(fd, termios.TCSANOW)
    if os.path.exists("boot-delay-%s" % ME):
        # The screen is up and takes pastes while the tool loads its servers.
        sys.stdout.write("\x1b[?2004h")
        if os.path.exists("boot-screen-%s" % ME):
            sys.stdout.write(open("boot-screen-%s" % ME).read().strip() + "\r\n")
        sys.stdout.flush()
        time.sleep(float(open("boot-delay-%s" % ME).read()))
    if os.path.exists("boot-exit-%s" % ME):
        print(open("boot-exit-%s" % ME).read().strip() + "\r", flush=True)
        sys.exit(3)
    if HOOKED:
        hook("SessionStart", {"session_id": thread, "transcript_path": os.path.join(os.environ.get("CLAUDE_CONFIG_DIR", os.environ.get("CODEX_HOME", "/nonexistent")), "t.jsonl")})
    sys.stdout.write("\x1b[?2004h")
    print("FAKE-READY\r", flush=True)
    buf = b""
    pasted = None
    text = []
    while True:
        r, _, _ = select.select([fd], [], [], 0.2)
        if not r:
            continue
        data = os.read(fd, 65536)
        if not data:
            break
        buf += data
        progress = True
        while progress and buf:
            progress = False
            if pasted is not None:
                k = buf.find(b"\x1b[201~")
                if k < 0:
                    pasted += buf
                    buf = b""
                    break
                pasted += buf[:k]
                buf = buf[k + 6:]
                text.append(pasted.decode(errors="replace").replace("\r", "\n"))
                with open("pasted-%s.log" % ME, "a") as log:
                    log.write("%d\n" % len(text[-1]))
                print("PASTED %d\r" % len(text[-1]), flush=True)
                pasted = None
                progress = True
                continue
            if buf.startswith(b"\x1b[200~"):
                buf = buf[6:]
                pasted = b""
                progress = True
                continue
            ch, buf = buf[:1], buf[1:]
            progress = True
            if ch == b"\r":
                prompt = "".join(text)
                text = []
                if "SWALLOW" in prompt:
                    pass  # a menu took it: no hook, no turn
                elif prompt.strip():
                    run_turn(prompt)
                else:
                    keys(b"<enter>")
            else:
                keys(ch)
