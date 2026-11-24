#!/usr/bin/env bash
# Run in the watcher's private checkout. DIST is unique to the tagged commit.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
source "$here/common.sh"
: "${CONNECTO_SIGN_IDENTITY:?}" "${CONNECTO_NOTARY_PROFILE:?}"
load_relay_token
mkdir -p "$DIST"
# Each completed step is reusable after a network, notarization or deploy failure.
for spec in 'arm64 windows windows' 'arm64 linux aarch64' 'amd64 linux x86_64'; do
  read -r platform stage arch <<<"$spec"
  marker="$DIST/done-$arch"
  [ ! -f "$marker" ] || continue
  image="connecto-release-$stage-$platform"
  docker build --platform "linux/$platform" --target "$stage" -t "$image" - < "$here/Dockerfile"
  if [ "$stage" = windows ]; then
    cmd=(scripts/release/build-windows-cross.sh)
  else
    cmd=(scripts/release/build-linux.sh "$arch")
  fi
  mkdir -p "$ROOT/target"
  docker run --rm --platform "linux/$platform" -e CONNECTO_RELAY_TOKEN -e CONNECTO_DIST=/out \
    -e CARGO_BUILD_JOBS=5 -v "$ROOT:/src:ro" -v "$DIST:/out" \
    -v "connecto-release-$stage-$platform-target:/src/target" \
    -v connecto-release-cargo:/usr/local/cargo/registry "$image" "${cmd[@]}"
  touch "$marker"
done
for arch in aarch64 x86_64 universal; do
  dmg="$DIST/connecto-$VERSION-macos-$arch.dmg"
  if [ ! -f "$DIST/done-mac-$arch" ]; then
    "$here/build-macos.sh" "$arch"
    xcrun stapler validate -q "$dmg"
    touch "$DIST/done-mac-$arch"
  fi
done
if [ ! -f "$DIST/done-chat-windows" ]; then
  "$here/build-chat-windows.sh"
  touch "$DIST/done-chat-windows"
fi
"$here/release.sh" check
if [ ! -f "$DIST/update.json" ]; then "$here/release.sh" feed "${1:-Connecto $VERSION}"; fi
