#!/usr/bin/env bash
# Write and sign dist/update.json, the feed the in-app updater reads, for the
# update archives of this version in dist/ (connecto-VERSION-macos-<arch>.app.zip
# from build-macos.sh, connecto-VERSION-windows-x86_64.zip from build-windows.ps1).
#
#   scripts/release/make-update-feed.sh ["release notes"]
#
# The private signing key (never committed) comes from, in this order:
#   CONNECTO_UPDATE_SIGNING_KEY        the key text (CI: a repository secret)
#   CONNECTO_UPDATE_SIGNING_KEY_FILE   a file holding it
#   scripts/release/update-signing-key.local   (git-ignored)
# Make the key pair once with:
#   cargo run -q -p connecto-update --bin connecto-update-feed -- keygen scripts/release/update-signing-key.local
# and put the printed public key in crates/connecto-update/update-public-key.txt.
#
# Other variables: CONNECTO_DIST (default ./dist), CONNECTO_VERSION (default the
# workspace version), CONNECTO_UPDATE_BASE_URL (default
# https://connecto.studio/downloads/, where the archives are uploaded).
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
NOTES="${1:-}"
BASE_URL="${CONNECTO_UPDATE_BASE_URL:-https://connecto.studio/downloads/}"
cd "$ROOT"
cargo run -q --locked -p connecto-update --bin connecto-update-feed -- sign \
  --version "$VERSION" --dist "$DIST" --base-url "$BASE_URL" \
  --published "$(date -u +%Y-%m-%d)" --notes "$NOTES"
log "upload the archives and update.json to $BASE_URL (update.json last)"
