"""Extract topic-routing evaluation cases from Claude transcripts.

Usage: python3 -I extract.py <out.json> [projects-glob] [days]

A case is a message the user typed while the conversation's context was at
least 80k tokens (docs/specs/topic-routing.md 4.1), with the user's earlier
requests as history, all kept whole (jev_eval.py redacts them and only then
clips them). A case's id is the transcript's own message uuid, so labels stay
attached to the same message whatever the filters do. Only messages sent in the last `days` days (by their own
timestamp) become cases; older ones still count as history.

Messages that another agent or Connecto delivered (`[connecto ...]`,
`[<name> inbox]`), tool results, slash and shell commands and interruptions are
neither cases nor history: the runtime would never see them (spec 5.2). The
output keeps them out but counts them.

The output holds private conversation text: keep it outside the repository.
"""

import datetime as dt
import glob
import json
import os
import re
import sys

MIN_CONTEXT = 80000
PATTERN = sys.argv[2] if len(sys.argv) > 2 else "~/.claude/projects/*/*.jsonl"
DAYS = int(sys.argv[3]) if len(sys.argv) > 3 else 30

DELIVERY = re.compile(r"^\[(connecto\b|[\w.-]+ inbox\])", re.IGNORECASE)
NOT_TYPED = ("<", "/", "!", "Caveat:", "[Request interrupted")


def text_of(message):
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        if any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
            return None
        return "\n".join(
            b.get("text", "")
            for b in content
            if isinstance(b, dict) and b.get("type") == "text"
        )
    return None


def kind(text):
    """'typed', 'delivery' or None (not a message at all)."""
    if not text or not text.strip():
        return None
    start = text.lstrip()
    if DELIVERY.match(start):
        return "delivery"
    if start.startswith(NOT_TYPED):
        return None
    return "typed"


def when(stamp):
    return dt.datetime.fromisoformat(stamp.replace("Z", "+00:00"))


def main():
    since = dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=DAYS)
    cases, deliveries, files, no_uuid = [], 0, 0, 0
    for path in sorted(glob.glob(os.path.expanduser(PATTERN))):
        files += 1
        prompts, context, last = [], 0, None
        for line in open(path, errors="ignore"):
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            if entry.get("isSidechain"):
                continue
            if entry.get("type") == "assistant":
                usage = entry.get("message", {}).get("usage")
                if usage:
                    context = (
                        usage.get("input_tokens", 0)
                        + usage.get("cache_read_input_tokens", 0)
                        + usage.get("cache_creation_input_tokens", 0)
                    )
                    last = entry.get("timestamp")
                continue
            if entry.get("type") != "user" or entry.get("isMeta"):
                continue
            text = text_of(entry.get("message", {}))
            k = kind(text)
            if k is None:
                continue
            stamp = entry.get("timestamp")
            recent = stamp is not None and when(stamp) >= since
            if k == "delivery":
                if recent and context >= MIN_CONTEXT:
                    deliveries += 1
                continue
            if recent and context >= MIN_CONTEXT and prompts and not entry.get("uuid"):
                no_uuid += 1
            elif recent and context >= MIN_CONTEXT and prompts:
                idle = (when(stamp) - when(last)).total_seconds() / 60 if last else 0.0
                cases.append(
                    {
                        "id": entry["uuid"],
                        "session": os.path.basename(path)[:-6],
                        "context": context,
                        "idle_min": round(idle, 1),
                        "previous": prompts[-16:],
                        "message": text,
                    }
                )
            prompts.append(text)
    out = {
        "params": {
            "glob": PATTERN,
            "days": DAYS,
            "min_context": MIN_CONTEXT,
            "files": files,
            "deliveries_over_threshold": deliveries,
            "skipped_without_uuid": no_uuid,
        },
        "cases": cases,
    }
    with open(sys.argv[1], "w") as handle:
        json.dump(out, handle, indent=1)
    print(
        f"{len(cases)} typed messages over {MIN_CONTEXT} tokens in the last {DAYS} days "
        f"({deliveries} agent deliveries left out) from {files} files matching {PATTERN}"
    )


if __name__ == "__main__":
    main()
