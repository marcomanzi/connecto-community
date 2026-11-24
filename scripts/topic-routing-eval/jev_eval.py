"""Ask Jev the topic-routing question for labelled cases and score the answers.

Usage:
    python3 -I jev_eval.py --self-test
    python3 -I jev_eval.py --reviewed-dataset <cases.json> <labels.json> [model]

`--reviewed-dataset` is required for a live run. The redaction helper runs
outside the app, so it does not know the values stored in Connecto's vault: a
stored credential with an ordinary-looking value and no telltale label would
pass. Only send a case file you have reviewed (or sanitized) yourself.

The script needs a Connecto secret named JEV_API_KEY, set up for
https://api.typesafe.ai with an `Authorization: Bearer` header. It calls Jev
through `connecto secret url`, so it never sees the key.

cases.json comes from extract.py. labels.json is a list of
{"id", "label": "continues"|"new", "sure": bool}.

Before anything leaves the machine, every whole text goes through Connecto's
own redactor (`cargo run -p connecto-redact --example redact_texts`, run from
this repository), then through extra patterns, and only then is clipped, as
spec 4.2 requires. A case whose redaction cannot run is not sent and counts as
"stay". Run --self-test first: it checks that synthetic secrets in both the
history and the new message never reach the request body.

Scoring follows the runtime: a failed call or an answer that is not one of the
two choices with a confidence in [0, 1] counts as "stay". It reports the
warm/cold rule of spec 4.3 (moves, precision and recall of moves) and the "new"
threshold sweep, over all labels and over the labels marked sure. The spec's
bar is precision >= 95%. With only a handful of true "new" cases, even a perfect
score is weak evidence: the report prints the counts so that is visible.
"""

import json
import math
import os
import re
import subprocess
import sys
import urllib.request

REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
TIMEOUT_SECONDS = 3  # the runtime's limit, spec 4.4

CHOICES = ("continues", "new")
QUESTION = {
    "type": "choice",
    "instructions": (
        "The state lists the user's earlier requests in one coding-agent "
        "conversation, oldest first, then NEW MESSAGE. Does the new message "
        "continue that work (refers to, corrects, extends, verifies, approves "
        "or answers something in it), or is it an independent task a fresh "
        "agent could do from the message alone?"
    ),
    "criteria": {
        "continues": "The message needs this conversation: it continues, corrects, extends, verifies or approves the earlier work.",
        "new": "An independent task that a fresh agent could do well from the message alone.",
    },
}

# A second layer after connecto_redact: common secret shapes, then any long
# unbroken token that looks random.
SECRET_PATTERNS = [
    re.compile(r"sk-[A-Za-z0-9_-]{16,}"),
    re.compile(r"gh[pousr]_[A-Za-z0-9]{20,}"),
    re.compile(r"github_pat_[A-Za-z0-9_]{20,}"),
    re.compile(r"xox[abposr]-[A-Za-z0-9-]{10,}"),
    re.compile(r"AKIA[0-9A-Z]{16}"),
    re.compile(r"AIza[0-9A-Za-z_-]{35}"),
    re.compile(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{12,}"),
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
    re.compile(r"(?i)\b(password|passwd|secret|token|api[_-]?key)\s*[:=]\s*\S+"),
    re.compile(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
]
LONG_TOKEN = re.compile(r"\b[A-Za-z0-9+/_=-]{32,}\b")


def looks_random(token):
    if not (re.search(r"[0-9]", token) and re.search(r"[A-Za-z]", token)):
        return False
    counts = {c: token.count(c) for c in set(token)}
    entropy = -sum(n / len(token) * math.log2(n / len(token)) for n in counts.values())
    return entropy > 3.5


def redact(text):
    for pattern in SECRET_PATTERNS:
        text = pattern.sub("[redacted]", text)
    return LONG_TOKEN.sub(
        lambda m: "[redacted]" if looks_random(m.group(0)) else m.group(0), text
    )


def production_redact(texts):
    """Connecto's redactor on whole texts: a list of the same length, None where
    it could not run."""
    done = subprocess.run(
        ["cargo", "run", "-q", "-p", "connecto-redact", "--example", "redact_texts"],
        input=json.dumps(texts),
        capture_output=True,
        text=True,
        cwd=REPO,
        check=True,
    )
    out = json.loads(done.stdout)
    if not isinstance(out, list) or len(out) != len(texts):
        raise RuntimeError("the redactor answered with the wrong shape")
    return out


def redacted_case(case):
    """The case with every text redacted whole, or None when any text could not
    be redacted."""
    texts = list(case["previous"]) + [case["message"]]
    try:
        out = production_redact(texts)
    except (subprocess.CalledProcessError, ValueError, RuntimeError, OSError):
        return None
    if any(not isinstance(text, str) for text in out):
        return None
    out = [redact(text) for text in out]
    return {**case, "previous": out[:-1], "message": out[-1]}


def state_of(case):
    """Built from an already redacted case: clips only."""
    lines = [f"- {p[:500]}" for p in case["previous"]]
    kept, size = [], 0
    for line in reversed(lines):
        size += len(line)
        if size > 8000:
            break
        kept.append(line)
    earlier = "\n".join(reversed(kept))
    return f"EARLIER REQUESTS:\n{earlier}\n\nNEW MESSAGE:\n{case['message'][:2000]}"


def body_of(model, case):
    return json.dumps(
        {"model": model, "state": state_of(case), "questions": {"topic": QUESTION}}
    ).encode()


def parse(answer):
    """(choice, confidence) when the answer is valid, else None."""
    topic = answer.get("answers", {}).get("topic") if isinstance(answer, dict) else None
    if not isinstance(topic, dict):
        return None
    if topic.get("type") != "choice":
        return None
    choice, confidence = topic.get("choice"), topic.get("confidence")
    if choice not in CHOICES or isinstance(confidence, bool):
        return None
    if not isinstance(confidence, (int, float)):
        return None
    if not 0 <= confidence <= 1:
        return None
    return choice, float(confidence)


def ask(base, model, case):
    case = redacted_case(case)
    if case is None:
        print("a case was withheld: redaction could not run", file=sys.stderr)
        return None
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/systemone",
        data=body_of(model, case),
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
            return parse(json.load(response))
    except Exception as err:  # noqa: BLE001 - a failed call is a "stay", reported
        print(f"{case['id']}: {err}", file=sys.stderr)
        return None


def moves(answer, idle):
    """The spec 4.3 rule. No valid answer: stay."""
    if answer is None:
        return False
    choice, confidence = answer
    if idle < 60:
        return choice == "new" and confidence >= 0.75
    return not (choice == "continues" and confidence >= 0.75)


def report(title, rows):
    truth = sum(1 for r in rows if r["label"] == "new")
    print(f"\n{title}: {len(rows)} cases, {truth} labelled new, "
          f"{sum(1 for r in rows if r['answer'] is None)} without a valid answer")
    for name, pick in (
        ("spec rule (warm/cold)", lambda r: r["moved"]),
        *(
            (f"new >= {t}", lambda r, t=t: r["answer"] is not None
             and r["answer"][0] == "new" and r["answer"][1] >= t)
            for t in (0.6, 0.75, 0.85, 0.95)
        ),
    ):
        moved = [r for r in rows if pick(r)]
        right = sum(1 for r in moved if r["label"] == "new")
        precision = f"{right}/{len(moved)}" if moved else "n/a"
        recall = f"{right}/{truth}" if truth else "n/a"
        print(f"  {name}: moved {len(moved)}, precision {precision}, recall {recall}")
    for side, test in (("warm", lambda r: r["idle"] < 60), ("cold", lambda r: r["idle"] >= 60)):
        part = [r for r in rows if test(r)]
        moved = [r for r in part if r["moved"]]
        wrong = sum(1 for r in moved if r["label"] != "new")
        print(f"  {side}: {len(part)} cases, moved {len(moved)}, wrongly moved {wrong}")


def self_test():
    secret = "sk-ant-api03-" + "A1b2C3d4E5f6G7h8J9k0L1m2N3o4P5q6"
    random = "Zq8" + "x7Lp2Vn9Rt4Wm6Ks1Hd3Fg5Jb0Yc8Ne"
    password = "hunter2-" + "correct-horse"
    url = "https://deploy:" + "s3cr3tPassw0rd" + "@example.com/x"
    case = {
        "id": "t",
        "previous": [
            f"use the key {secret} please",
            f'config {{"password": "{password}"}}',
            "x" * 600 + f" {secret}",
        ],
        "message": f"deploy to {url} with Bearer {random} and {secret}",
    }
    clean = redacted_case(case)
    assert clean is not None
    body = body_of("jev-1.14.0", clean).decode()
    for value in (secret, random, password, "s3cr3tPassw0rd"):
        assert value not in body, (value, body)
    assert parse({"answers": {"topic": {}}}) is None
    assert parse({"answers": {"topic": {"choice": "maybe", "confidence": 0.9}}}) is None
    assert parse({"answers": {"topic": {"choice": "new", "confidence": 1.5}}}) is None
    assert parse({"answers": {"topic": {"choice": "new", "confidence": True}}}) is None
    assert parse({"answers": {"topic": {"type": "noul", "choice": "new", "confidence": 0.9}}}) is None
    assert parse({"answers": {"topic": {"choice": "new", "confidence": 0.8}}}) is None
    assert parse({"answers": {"topic": {"type": "choice", "choice": "new", "confidence": 0.8}}}) == ("new", 0.8)
    assert moves(None, 120) is False and moves(None, 5) is False
    assert moves(("new", 0.8), 5) and not moves(("new", 0.7), 5)
    assert moves(("continues", 0.7), 90) and not moves(("continues", 0.8), 90)
    print("self-test ok")


def main():
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return
    args = sys.argv[1:]
    if not args or args[0] != "--reviewed-dataset":
        sys.exit(__doc__)
    args = args[1:]
    data = json.load(open(args[0]))
    cases = {c["id"]: c for c in (data["cases"] if isinstance(data, dict) else data)}
    labels = json.load(open(args[1]))
    model = args[2] if len(args) > 2 else "jev-1.14.0"
    self_test()
    base = subprocess.run(
        ["connecto", "secret", "url", "JEV_API_KEY"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    rows = []
    for label in labels:
        case = cases.get(label["id"])
        if case is None:
            continue
        answer = ask(base, model, case)
        rows.append(
            {
                "label": label["label"],
                "sure": label.get("sure", True),
                "answer": answer,
                "idle": case["idle_min"],
                "moved": moves(answer, case["idle_min"]),
            }
        )
    report("all labels", rows)
    report("labels marked sure", [r for r in rows if r["sure"]])


if __name__ == "__main__":
    main()
