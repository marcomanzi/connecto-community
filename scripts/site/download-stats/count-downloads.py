#!/usr/bin/env python3
"""Count Connecto downloads from Caddy's download log and write stats.json.

Reads every log file Caddy keeps for the download files (the current one and
its rolled copies, plain or gzip), counts each completed download, and writes

    {"total": N, "last7days": M, "updates": U, "updated": "<UTC ISO time>",
     "since": "<date>", "by_version": {"0.1.7": n, ...}}

into the site's downloads folder, where the landing page reads it. The count is
recomputed from the logs every run, so running it twice changes nothing.

What counts as one download: a GET of /downloads/connecto-*.dmg or .zip that
was answered 200 with (nearly) the whole file sent, from a user agent that does
not call itself a bot. Partial (206) and cached (304) answers, HEAD requests and
aborted transfers are not counted. A download by Connecto's own updater (user
agent `Connecto-Updater/<version>`) is an existing install updating itself:
it is counted apart, in `updates`, and never in `total` or `last7days`. The log holds no IP address (see
caddy-snippet.caddy), and nothing here needs one.

Standard library only. Usage:

    count-downloads.py --logs DIR --downloads DIR [--out FILE]
"""

import argparse
import datetime as dt
import gzip
import json
import os
import re
import sys
import tempfile

FILE = re.compile(r"^/downloads/(connecto-(\d+\.\d+\.\d+)-[a-z0-9_.-]+\.(?:dmg|zip))$")
UPDATER = re.compile(r"^Connecto-Updater/")
BOT = re.compile(r"bot|crawl|spider|slurp|preview|monitor|curl/|wget/|python-requests|go-http-client|headless", re.I)
# A transfer this close to the file's size counts as complete.
COMPLETE = 0.99


def log_files(folder):
    for name in sorted(os.listdir(folder)):
        if name.startswith("connecto-downloads") and (name.endswith(".log") or name.endswith(".log.gz")):
            yield os.path.join(folder, name)


def lines(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt", encoding="utf-8", errors="replace") as handle:
        yield from handle


def counted(entry, sizes):
    """(version, by_updater) for a completed download, or None when this log
    entry is not one."""
    request = entry.get("request") or {}
    if request.get("method") != "GET" or entry.get("status") != 200:
        return None
    match = FILE.match((request.get("uri") or "").split("?", 1)[0])
    if not match:
        return None
    agent = " ".join((request.get("headers") or {}).get("User-Agent") or [])
    updater = bool(UPDATER.match(agent))
    if not agent or (BOT.search(agent) and not updater):
        return None
    full = sizes.get(match.group(1))
    sent = entry.get("size")
    if full and isinstance(sent, int) and sent < full * COMPLETE:
        return None
    return match.group(2), updater


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--logs", required=True, help="the folder holding connecto-downloads*.log")
    parser.add_argument("--downloads", required=True, help="the site's downloads folder")
    parser.add_argument("--out", help="where to write (default: <downloads>/stats.json)")
    args = parser.parse_args()
    out = args.out or os.path.join(args.downloads, "stats.json")

    sizes = {}
    for name in os.listdir(args.downloads):
        path = os.path.join(args.downloads, name)
        if os.path.isfile(path):
            sizes[name] = os.path.getsize(path)

    now = dt.datetime.now(dt.timezone.utc)
    week_ago = (now - dt.timedelta(days=7)).timestamp()
    total = last7 = updates = 0
    first = None
    by_version = {}
    for path in log_files(args.logs):
        for line in lines(path):
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            found = counted(entry, sizes)
            if found is None:
                continue
            version, by_updater = found
            ts = entry.get("ts")
            if not isinstance(ts, (int, float)):
                continue
            if by_updater:
                updates += 1
                continue
            total += 1
            by_version[version] = by_version.get(version, 0) + 1
            if ts >= week_ago:
                last7 += 1
            first = ts if first is None else min(first, ts)

    stats = {
        "total": total,
        "last7days": last7,
        "updates": updates,
        "updated": now.replace(microsecond=0).isoformat().replace("+00:00", "Z"),
        "since": dt.datetime.fromtimestamp(first, dt.timezone.utc).date().isoformat() if first else None,
        "by_version": dict(sorted(by_version.items())),
    }
    # Written beside the target and renamed, so a reader never sees half a file.
    folder = os.path.dirname(os.path.abspath(out))
    handle, temp = tempfile.mkstemp(prefix=".stats-", dir=folder)
    with os.fdopen(handle, "w", encoding="utf-8") as tmp:
        json.dump(stats, tmp, separators=(",", ":"))
        tmp.write("\n")
    os.chmod(temp, 0o644)
    os.replace(temp, out)
    print(f"downloads: {total} in all, {last7} in the last 7 days, {updates} updates -> {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
