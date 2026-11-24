#!/usr/bin/env bash
# One release, start to finish, from the release Mac.
#
#   scripts/release/release.sh all <version> "<release notes>"
#
# runs every step below in order. Each step can also be run on its own, so a
# release that stops half way is picked up where it stopped:
#
#   bump <version>     set the version in Cargo.toml, both Cargo.locks and the docs,
#                      commit and push main
#   site              write site/assets/releases.js from dist/ (sizes and
#                      SHA-256s), commit and push main
#   feed "<notes>"     write and sign dist/update.json
#   publish            upload the files, check them on the server, deploy the
#                      site, and upload update.json last
#   deploy-site <ref>  deploy only the site, as it is at <ref> (a commit or tag
#                      that has the latest download list); no build, no feed.
#                      A tag named site_v... does this on the release Mac.
#
# CONNECTO_MAC_ONLY=1 releases without the Windows zip (when it cannot be built
# yet): site, feed and publish skip it, the site keeps its current Windows
# download, and Windows users get no update. Once the zip is in dist/, run
# site, feed and publish again without it.
#
# Settings come from the environment, or from scripts/release/release.local
# (git-ignored, sourced if present):
#   CONNECTO_SIGN_IDENTITY, CONNECTO_NOTARY_PROFILE   macOS signing (required:
#       macOS releases are always signed and notarized)
#   CONNECTO_DEPLOY_HOST      ssh target of the web server, e.g. root@host
#   CONNECTO_DEPLOY_KEY       ssh key for it
#   CONNECTO_DEPLOY_ROOT      site folder on the server (default /var/www/connecto)
#   CONNECTO_COMMIT_EMAIL     author email of the release commits (optional)
#
# The tag watcher (watch-tags.py, build-tag.sh) builds every file of a release
# on this Mac; `all` hands the new tag to it.
#
# Needs: cargo, rsync, ssh. The release is cut from main, with
# a clean working tree that matches origin/main.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Exported, so the build scripts this one runs see them too.
if [ -f "$here/release.local" ]; then set -a; source "$here/release.local"; set +a; fi
source "$here/common.sh"
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

DEPLOY_ROOT="${CONNECTO_DEPLOY_ROOT:-/var/www/connecto}"
cd "$ROOT"

need() { [ -n "${!1:-}" ] || die "$1 is not set (environment or scripts/release/release.local)"; }
ssh_opts() { printf 'ssh -i %s -o IdentitiesOnly=yes' "$CONNECTO_DEPLOY_KEY"; }
remote() { ssh -i "$CONNECTO_DEPLOY_KEY" -o IdentitiesOnly=yes "$CONNECTO_DEPLOY_HOST" "$@"; }
current_version() { printf '%s\n' "${CONNECTO_VERSION:-$(version)}"; }

clean_main() {
  [ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die "not on main"
  [ -z "$(git status --porcelain --untracked-files=no)" ] || die "the working tree has changes; commit or stash them first"
  git fetch -q origin main
  [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "main is not the same as origin/main; pull or push first"
}

commit_push() {
  git add "$@"
  git diff --cached --quiet && { log "nothing to commit"; return; }
  git ${CONNECTO_COMMIT_EMAIL:+-c user.email="$CONNECTO_COMMIT_EMAIL"} commit -q -m "$COMMIT_MSG"
  git push -q origin main
}

step_bump() {
  local new="${1:?usage: release.sh bump <version>}"
  [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not a version: $new"
  clean_main
  local old; old="$(current_version)"
  [ "$old" != "$new" ] || { log "already at $new"; return; }
  log "version $old -> $new"
  sed -i '' "s/^version = \"$old\"/version = \"$new\"/" Cargo.toml
  cargo update -q -w
  # The companion's own lock names these crates' versions too, and release
  # builds use --locked: bring it along.
  cargo metadata -q --format-version 1 --offline \
    --manifest-path connecto-flutter-app/native/Cargo.toml >/dev/null
  local f
  for f in site/docs/install.html site/docs/updates.html; do
    sed -i '' "s/${old//./\\.}/$new/g" "$f"
  done
  VERSION="$new"
  COMMIT_MSG="Version $new" commit_push Cargo.toml Cargo.lock connecto-flutter-app/native/Cargo.lock site/docs/install.html site/docs/updates.html
}

files() {
  if [ -f "$DIST/done-chat-windows" ]; then
    printf '%s\n' "connecto-$VERSION-chat-windows-x86_64.zip" "connecto-$VERSION-chat-windows-x86_64.zip.sha256"
  fi
  if [ "${CONNECTO_LINUX:-}" = 1 ]; then
    printf '%s\n' "connecto-$VERSION-linux-x86_64.tar.gz" "connecto-$VERSION-linux-aarch64.tar.gz"
  fi
  [ "${CONNECTO_MAC_ONLY:-}" = 1 ] || printf '%s\n' "connecto-$VERSION-windows-x86_64.zip"
  printf '%s\n' \
    "connecto-$VERSION-macos-aarch64.dmg" \
    "connecto-$VERSION-macos-x86_64.dmg" \
    "connecto-$VERSION-macos-universal.dmg" \
    "connecto-$VERSION-macos-aarch64.app.zip" \
    "connecto-$VERSION-macos-x86_64.app.zip"
}

check_dist() {
  VERSION="$(current_version)"
  local f
  for f in $(files); do
    [ -f "$DIST/$f" ] || die "missing dist/$f"
    grep -q "  $f\$" "$DIST/SHA256SUMS" || die "dist/SHA256SUMS has no line for $f"
  done
  (cd "$DIST" && for f in $(files); do grep "  $f\$" SHA256SUMS; done | shasum -a 256 -c --quiet) \
    || die "a file in dist/ does not match dist/SHA256SUMS"
  log "dist/ has every file of $VERSION, checksums match"
}

step_site() {
  clean_main
  check_dist
  local out=site/assets/releases.js
  local prev; prev="$(printf '%s\n' "$VERSION" | awk -F. '{printf "%d.%d.%d", $1, $2, $3-1}')"
  python3 - "$out" "$DIST" "$VERSION" "$prev" "${CONNECTO_MAC_ONLY:-}" <<'PY'
import os, re, sys
out, dist, ver, prev, mac_only = sys.argv[1:]
src = open(out).read()
sums = dict(reversed(l.split()) for l in open(os.path.join(dist, "SHA256SUMS")) if l.strip())
cards = [
    ("Windows · x86_64", "windows-x86_64.zip", "Windows 10 (1809) or later"),
    ("macOS · Apple silicon", "macos-aarch64.dmg", "macOS 11 or later"),
    ("macOS · Intel", "macos-x86_64.dmg", "macOS 10.15 or later"),
    ("macOS · Universal (both)", "macos-universal.dmg", "macOS 10.15 or later"),
]
if os.getenv("CONNECTO_LINUX") == "1":
    for arch in ("x86_64", "aarch64"):
        cards.append((f"Linux · {arch} (experimental)", f"linux-{arch}.tar.gz",
                      "Experimental · Debian 12 or compatible · agent provisioning is incomplete"))
entries = []
for title, suffix, note in cards:
    if mac_only == "1" and suffix.startswith("windows"):
        # Keep the Windows download the site already has.
        kept = re.search(r'  \{\n    title: "Windows[^}]*\}', src)
        if kept:
            entries.append(kept.group(0))
        continue
    name = f"connecto-{ver}-{suffix}"
    size = os.path.getsize(os.path.join(dist, name)) / 1e6
    entries.append(
        "  {\n"
        f'    title: "{title}",\n'
        f'    version: "{ver}",\n'
        f'    file: "downloads/{name}",\n'
        f'    size: "{size:.1f} MB",\n'
        f'    sha256: "{sums[name]}",\n'
        f'    note: "{note}"\n'
        "  }"
    )
start = src.index("window.CONNECTO_RELEASES")
# Keep what follows the app's list (the Connecto Chat list) as it is.
tail = src[src.index("\n];\n", start) + len("\n];\n"):]
head = src[:start]
head = re.sub(r"Version \S+ is the\n// current build \(0\.1\.0 to \S+ are",
              f"Version {ver} is the\n// current build (0.1.0 to {prev} are", head)
open(out, "w").write(head + "window.CONNECTO_RELEASES = [\n" + ",\n".join(entries) + "\n];\n" + tail)
PY
  local site_files=("$out")
  if [ -f "$DIST/done-chat-windows" ]; then
    # The Windows companion is an entry of CONNECTO_CHAT_RELEASES in releases.js.
    python3 - "$out" "$VERSION" <<'PYTHON'
from pathlib import Path
import re, sys
page, ver = Path(sys.argv[1]), sys.argv[2]
src = page.read_text()
start = src.index("window.CONNECTO_CHAT_RELEASES")
entry = re.compile(r'  \{\n    title: "Windows[^}]*\}').search(src, start)
if not entry:
    sys.exit("Expected a Windows entry in CONNECTO_CHAT_RELEASES")
block, count = re.subn(r"downloads/connecto-[0-9]+\.[0-9]+\.[0-9]+-chat-windows-x86_64\.zip",
                       f"downloads/connecto-{ver}-chat-windows-x86_64.zip", entry.group(0))
if count != 2:
    sys.exit("Expected Windows companion download and checksum links")
block = re.sub(r'version: "[^"]*"', f'version: "{ver}"', block, count=1)
page.write_text(src[: entry.start()] + block + src[entry.end():])
PYTHON
  fi
  COMMIT_MSG="Site: $VERSION downloads" commit_push "${site_files[@]}"
}

step_feed() {
  VERSION="$(current_version)"
  CONNECTO_VERSION="$VERSION" "$here/make-update-feed.sh" "${1:-}"
  cargo run -q --locked -p connecto-update --bin connecto-update-feed -- verify "$DIST/update.json"
}

# Copy site/ as it is at $1 to the server. The downloads folder is left alone.
deploy_site() {
  local ref="$1" tmp
  log "deploying the site from $ref"
  tmp="$(mktemp -d)"
  git archive "$ref" site | tar -x -C "$tmp"
  rsync -rtz --delete --exclude README.md --exclude '.*' --filter 'P /downloads/**' \
    -e "$(ssh_opts)" "$tmp/site/" "$CONNECTO_DEPLOY_HOST:$DEPLOY_ROOT/"
  rm -r "$tmp"
  remote "chown -R root:root $DEPLOY_ROOT/ && find $DEPLOY_ROOT -path $DEPLOY_ROOT/downloads -prune -o -type d -exec chmod 755 {} + -o -type f -exec chmod 644 {} +"
}

# A site-only release: the site as it is at $1, with no build and no feed. It
# must have main's latest download list, so it never puts back old downloads.
step_deploy_site() {
  local ref="${1:-}" commit lists
  [ -n "$ref" ] || die 'usage: release.sh deploy-site <commit or tag>'
  need CONNECTO_DEPLOY_HOST; need CONNECTO_DEPLOY_KEY
  git fetch -q origin main
  commit="$(git rev-parse --verify "$ref^{commit}")" || die "no such commit: $ref"
  lists="$(git log -1 --format=%H origin/main -- site/assets/releases.js)"
  if [ -n "$lists" ] && ! git merge-base --is-ancestor "$lists" "$commit"; then
    die "$ref does not have main's latest download list ($(git log -1 --format='%h %s' "$lists")): rebase it on main"
  fi
  deploy_site "$commit"
  curl -fsS -o /dev/null "$SITE_URL/docs/" || die "the site does not answer after the deploy"
  log "deployed the site from $(git log -1 --format='%h %s' "$commit"): $SITE_URL"
}

step_publish() {
  need CONNECTO_DEPLOY_HOST; need CONNECTO_DEPLOY_KEY
  # Publishing deploys the site from origin/main, so local commits that are not
  # pushed yet do not block it; only a main that is behind does.
  git fetch -q origin main
  git merge-base --is-ancestor origin/main HEAD || die "main is behind origin/main; pull first"
  check_dist
  [ -f "$DIST/update.json" ] || die "no dist/update.json: run the feed step first"
  cargo run -q --locked -p connecto-update --bin connecto-update-feed -- verify "$DIST/update.json" \
    | grep -q "version $VERSION," || die "dist/update.json is not a signed feed for $VERSION"

  log "uploading the files"
  local list=() f
  for f in $(files); do list+=("$DIST/$f"); done
  rsync -tz -e "$(ssh_opts)" "${list[@]}" "$CONNECTO_DEPLOY_HOST:$DEPLOY_ROOT/downloads/"
  local sums; sums="$(cd "$DIST" && for f in $(files); do grep "  $f\$" SHA256SUMS; done)"
  remote "cd $DEPLOY_ROOT/downloads && chown root:root connecto-$VERSION-* && chmod 644 connecto-$VERSION-* && sha256sum -c --quiet" <<<"$sums" \
    || die "the uploaded files do not match their checksums"
  log "uploaded files match their checksums"

  deploy_site origin/main

  log "uploading update.json (last)"
  rsync -tz -e "$(ssh_opts)" "$DIST/update.json" "$CONNECTO_DEPLOY_HOST:$DEPLOY_ROOT/downloads/update.json"
  remote "chown root:root $DEPLOY_ROOT/downloads/update.json && chmod 644 $DEPLOY_ROOT/downloads/update.json"

  curl -fsS "$SITE_URL/downloads/update.json" | cmp - "$DIST/update.json" || die "live feed differs from the signed release"
  for f in $(files); do
    curl -fsSI "$SITE_URL/downloads/$f" >/dev/null || die "live download unavailable: $f"
  done
  log "released $VERSION: $SITE_URL"
}

cmd="${1:-}"; shift || true
case "$cmd" in
  bump) step_bump "$@" ;;
  check) check_dist ;;
  site) step_site ;;
  feed) step_feed "$@" ;;
  publish) step_publish ;;
  deploy-site) step_deploy_site "$@" ;;
  all)
    [ $# -eq 2 ] || die 'usage: release.sh all <version> "<release notes>"'
    state="$HOME/Library/Application Support/ConnectoRelease"
    launchctl print "gui/$(id -u)/studio.connecto.release" >/dev/null || die "install the tag watcher first"
    step_bump "$1"
    if ! git show-ref --verify --quiet "refs/tags/v$1"; then git tag -a "v$1" -m "$2"; fi
    [ "$(git rev-parse "v$1^{commit}")" = "$(git rev-parse HEAD)" ] || die "tag points to another commit"
    git push origin "refs/tags/v$1"
    log "waiting for the tag watcher (log: ~/Library/Logs/ConnectoRelease/watcher.log)"
    while ! python3 -c 'import json,sys; sys.exit(json.load(open(sys.argv[1])).get(sys.argv[2], {}).get("status") != "published")' "$state/tags.json" "v$1"; do sleep 30; done ;;
  *) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2 ;;
esac
