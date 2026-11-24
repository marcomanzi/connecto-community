"""A stand-in model server for the real-OpenCode test: just enough of the
OpenAI chat completions API (streamed) for OpenCode's `openai-compatible`
provider.

Usage: mock_openai.py <log file>. It listens on a free local port and prints
that port on its first line. Every request is logged as one JSON line (the
system prompt and the last message). A last user message holding `USE_BASH`
is answered with one bash tool call, so OpenCode asks for permission; one
holding `SLOW` is answered after six seconds; every other message is answered
with "Hello from the mock model."
"""
import http.server
import json
import sys
import time

LOG = sys.argv[1]


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps({"data": [{"id": "m1", "object": "model"}]}).encode())

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        messages = body.get("messages", [])
        last = messages[-1] if messages else {}
        system = "\n".join(
            m.get("content") if isinstance(m.get("content"), str) else json.dumps(m.get("content"))
            for m in messages
            if m.get("role") == "system"
        )
        with open(LOG, "a") as fh:
            fh.write(json.dumps({"system": system, "last": last}) + "\n")
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()

        def chunk(delta, finish=None):
            data = {
                "id": "c1",
                "object": "chat.completion.chunk",
                "created": int(time.time()),
                "model": "m1",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }
            self.wfile.write(b"data: " + json.dumps(data).encode() + b"\n\n")
            self.wfile.flush()

        text = json.dumps(last.get("content"))
        # A slow answer, so a turn at work can be seen on the screen.
        if "SLOW" in text and last.get("role") == "user" and body.get("tools"):
            time.sleep(6)
        # Only a title request has no tools: it is answered with text.
        if "USE_BASH" in text and last.get("role") == "user" and body.get("tools"):
            # What the agent's shell sees of OpenCode's folders.
            call = {
                "command": 'printf "%s|%s|%s" "${XDG_CONFIG_HOME-unset}" "${XDG_DATA_HOME-unset}" "${XDG_STATE_HOME-unset}" > xdg.txt',
                "description": "Write the XDG folders",
            }
            chunk(
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "bash", "arguments": json.dumps(call)},
                        }
                    ],
                }
            )
            chunk({}, "tool_calls")
        else:
            chunk({"role": "assistant", "content": "Hello from "})
            chunk({"content": "the mock model."})
            chunk({}, "stop")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
