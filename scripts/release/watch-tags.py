#!/usr/bin/env python3
"""Poll origin for new tags from launchd. All checkouts belong to the watcher."""
import fcntl
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

state = Path.home() / "Library/Application Support/ConnectoRelease"
repo = state / "repository"
ledger = state / "tags.json"


def run(*args, output=False, env=None):
    result = subprocess.run(args, cwd=repo, env=env, check=True, text=True,
                            stdout=subprocess.PIPE if output else None)
    return result.stdout.strip() if output else None


def save(records):
    temp = ledger.with_suffix(".tmp")
    temp.write_text(json.dumps(records, indent=2, sort_keys=True) + "\n")
    temp.replace(ledger)


def numbers(version):
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError(f"invalid release version {version!r}")
    return tuple(map(int, version.split(".")))


def release(tag, commit):
    run("git", "checkout", "--force", "--detach", commit)
    version = run("bash", "-c", 'source scripts/release/common.sh && version', output=True)
    expected = numbers(version)
    if re.fullmatch(r"v?[0-9]+\.[0-9]+\.[0-9]+", tag) and tag.lstrip("v") != version:
        raise ValueError(f"{tag} points to workspace version {version}")
    dist = state / "builds" / commit
    dist.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, CONNECTO_DIST=str(dist), CONNECTO_VERSION=version, CONNECTO_LINUX="1")
    url = "https://connecto.studio/downloads/update.json"

    def live_version():
        live = run("curl", "-fsS", "--max-time", "60", url, output=True)
        return numbers(json.loads(json.loads(live)["signed"])["version"])

    current = live_version()
    if current > expected:
        return "superseded"
    if current == expected and not (dist / "publishing").exists():
        raise ValueError(f"{version} already exists on the site; release versions must be unique")
    notes = run("git", "for-each-ref", "--format=%(contents:subject)", TAG_REFS + "/" + tag, output=True)
    if subprocess.run(["docker", "info"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
        run("docker", "desktop", "start", "--timeout", "180")
    run("scripts/release/build-tag.sh", notes or f"Connecto {version}", env=env)
    fetch()
    if run("git", "rev-parse", f"{TAG_REFS}/{tag}^{{commit}}", output=True) != commit:
        raise ValueError(f"{tag} moved during its build")
    if live_version() > expected:
        return "superseded"
    # Publish the current main site's content with the tagged artifacts.
    run("git", "checkout", "--force", "-B", "main", "origin/main")
    (dist / "publishing").touch()
    run("scripts/release/release.sh", "site", env=env)
    run("scripts/release/release.sh", "publish", env=env)
    return "published"


TAG_REFS = "refs/connecto-release/tags"

# A tag starting with this deploys only the site, as it is at the tagged
# commit: no version, no build, no update feed.
SITE_TAG = "site_v"


def site_release(tag, commit):
    run("git", "checkout", "--force", "--detach", commit)
    # The deploy runs this commit's script, which refuses a site without
    # main's latest download list.
    run("scripts/release/release.sh", "deploy-site", commit)
    return "published"


def fetch():
    run("git", "fetch", "--quiet", "--no-tags", "--prune", "origin",
        "+refs/heads/main:refs/remotes/origin/main", f"+refs/tags/*:{TAG_REFS}/*")


def main():
    lock = (state / "lock").open("a")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    fetch()
    tags = run("git", "for-each-ref", "--sort=version:refname", "--format=%(refname:strip=3)",
               TAG_REFS + "/", output=True).splitlines()
    if sys.argv[1:] == ["--baseline"]:
        if not ledger.exists():
            save({tag: {"status": "pre-existing"} for tag in tags})
        return
    records = json.loads(ledger.read_text())
    failed = False
    for tag in tags:
        record = records.get(tag, {})
        if record.get("status") in ("pre-existing", "published", "superseded"):
            continue
        if record.get("retry", 0) > time.time():
            continue
        commit = run("git", "rev-parse", f"{TAG_REFS}/{tag}^{{commit}}", output=True)
        try:
            if record.get("commit", commit) != commit:
                raise ValueError(f"{tag} moved since the first attempt")
            records[tag] = dict(record, commit=commit, status="building")
            save(records)
            print(f"Building {tag} at {commit}", flush=True)
            if tag.startswith(SITE_TAG):
                status = site_release(tag, commit)
            else:
                status = release(tag, commit)
            records[tag] = {"commit": commit, "status": status}
            print(f"{tag}: {status}", flush=True)
        except (OSError, subprocess.CalledProcessError, ValueError, KeyError) as error:
            failed = True
            print(f"{tag}: {error}", file=sys.stderr, flush=True)
            attempts = record.get("attempts", 0) + 1
            records[tag] = {"commit": record.get("commit", commit), "status": "failed",
                            "error": str(error), "attempts": attempts,
                            "retry": time.time() + min(3600, 60 * 2 ** min(6, attempts - 1))}
        save(records)
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
