#!/usr/bin/env bash
# Shared by the release scripts. Source it; do not run it.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIST="${CONNECTO_DIST:-$ROOT/dist}"

# The product version is the workspace version in Cargo.toml ([workspace.package]).
# Bump it there (one line); every crate and every artifact name follows.
version() {
  awk '
    /^\[workspace\.package\]/ { in_pkg = 1; next }
    /^\[/ { in_pkg = 0 }
    in_pkg && /^version[[:space:]]*=/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
  ' "$ROOT/Cargo.toml"
}
VERSION="${CONNECTO_VERSION:-$(version)}"
[ -n "$VERSION" ] || { echo "could not read the workspace version from Cargo.toml" >&2; exit 1; }

BUNDLE_ID="${CONNECTO_BUNDLE_ID:-studio.connecto.app}"
SITE_URL="https://connecto.studio"

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# The relay access token built into the app (the app reads CONNECTO_RELAY_TOKEN
# at compile time, and uses it only for the default relay). It comes from
# CONNECTO_RELAY_TOKEN, or else from scripts/release/relay-token.local (untracked,
# git-ignored). A release build fails without one. CONNECTO_NO_RELAY_TOKEN=1
# builds on purpose without one. The value is never printed: xtrace is off while
# it is read and restored after.
load_relay_token() {
  local tracing=0
  case "$-" in *x*) tracing=1; set +x ;; esac
  if [ "${CONNECTO_NO_RELAY_TOKEN:-}" = 1 ]; then
    unset CONNECTO_RELAY_TOKEN
    log "CONNECTO_NO_RELAY_TOKEN=1: this build has no built-in relay access token"
  else
    local file="$ROOT/scripts/release/relay-token.local"
    if [ -z "${CONNECTO_RELAY_TOKEN:-}" ] && [ -f "$file" ]; then
      CONNECTO_RELAY_TOKEN="$(tr -d ' \t\r\n' < "$file")"
    fi
    if [ -z "${CONNECTO_RELAY_TOKEN:-}" ]; then
      [ "$tracing" = 1 ] && set -x
      die "no relay access token: set CONNECTO_RELAY_TOKEN, or put it in scripts/release/relay-token.local (untracked). To build on purpose without one, set CONNECTO_NO_RELAY_TOKEN=1."
    fi
    export CONNECTO_RELAY_TOKEN
    log "built-in relay access token: set"
  fi
  [ "$tracing" = 1 ] && set -x
  return 0
}

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  else sha256sum "$1" | awk '{print $1}'; fi
}

# Add (or replace) the checksum line of one artifact in dist/SHA256SUMS, in
# the format `shasum -a 256 -c` reads.
record_checksum() {
  local file="$1" name sums="$DIST/SHA256SUMS" tmp
  name="$(basename "$file")"
  tmp="$(mktemp)"
  if [ -f "$sums" ]; then grep -v "  ${name}\$" "$sums" > "$tmp" || true; fi
  printf '%s  %s\n' "$(sha256_of "$file")" "$name" >> "$tmp"
  sort -k2 "$tmp" > "$sums"
  rm -f "$tmp"
  log "sha256 $(sha256_of "$file")  $name"
}

# The README text shipped inside every archive.
ship_readme() {
  cat <<TXT
Connecto ${VERSION}
${SITE_URL}

Files:
  connecto-app    the desktop app
  connecto-hook   the helper the app installs for agent terminals; also the
                  \`connecto\` command (it is the same program under that name)

First run: \`connecto-app provision\` copies your installed claude and codex
into Connecto's private store; then start \`connecto-app\`.
TXT
}
