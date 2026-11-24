#!/usr/bin/env bash
# Install (or refresh) the logged-in release Mac's tag watcher.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
state="$HOME/Library/Application Support/ConnectoRelease"
label=studio.connecto.release
domain="gui/$(id -u)"
plist="$HOME/Library/LaunchAgents/$label.plist"
umask 077
mkdir -p "$state" "$(dirname "$plist")" "$HOME/Library/Logs/ConnectoRelease"
for file in release.local relay-token.local update-signing-key.local; do
  test -s "$here/$file" || { echo "Missing $here/$file" >&2; exit 1; }
done
if [ ! -d "$state/repository/.git" ]; then
  git clone -q "$(git -C "$here" remote get-url origin)" "$state/repository"
fi
install -m 700 "$here/watch-tags.py" "$state/watch-tags.py"
for file in release.local relay-token.local update-signing-key.local; do
  install -m 600 "$here/$file" "$state/$file"
done
cat > "$state/run" <<'SH'
#!/bin/bash
set -eu
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
set -a
source ./release.local
set +a
export CONNECTO_UPDATE_SIGNING_KEY_FILE="$PWD/update-signing-key.local"
CONNECTO_RELAY_TOKEN="$(cat relay-token.local)"
export CONNECTO_RELAY_TOKEN GIT_TERMINAL_PROMPT=0
exec /usr/bin/python3 -u ./watch-tags.py "$@"
SH
chmod 700 "$state/run"
"$state/run" --baseline
/usr/bin/python3 - "$plist" "$state/run" "$label" <<'PY'
from pathlib import Path
import plistlib, sys
out, command, label = sys.argv[1:]
log = str(Path.home() / "Library/Logs/ConnectoRelease/watcher.log")
with open(out, "wb") as f:
    plistlib.dump(dict(Label=label, ProgramArguments=["/usr/bin/caffeinate", "-i", command],
                       StartInterval=60, RunAtLoad=True, StandardOutPath=log, StandardErrorPath=log), f)
PY
if launchctl print "$domain/$label" >/dev/null 2>&1; then launchctl bootout "$domain/$label"; fi
launchctl enable "$domain/$label"
launchctl bootstrap "$domain" "$plist"
