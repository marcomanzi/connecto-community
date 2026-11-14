#!/usr/bin/python3
"""A stand-in for `codex app-server --listen stdio://`: answers initialize and
hooks/list, as measured on 0.166.0 (Check 2)."""
import json, sys

for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if message.get("method") == "initialize":
        print(json.dumps({"id": message["id"], "result": {"userAgent": "fake"}}), flush=True)
    elif message.get("method") == "hooks/list":
        hooks = [
            {"key": "/<session-flags>/config.toml:session_start:0:0", "currentHash": "sha256:" + "a" * 64},
            {"key": "/<session-flags>/config.toml:user_prompt_submit:0:0", "currentHash": "sha256:" + "b" * 64},
        ]
        print(json.dumps({"id": message["id"], "result": {"data": [{"hooks": hooks}]}}), flush=True)
