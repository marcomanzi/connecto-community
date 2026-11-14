#!/usr/bin/env bash
# Build Connecto for Linux into dist/connecto-VERSION-linux-ARCH.tar.gz.
#
#   scripts/release/build-linux.sh x86_64
#   scripts/release/build-linux.sh aarch64
#
# Run it on a Linux machine of that architecture (CI uses native runners). It
# only tars the programs; see README.md for what does and does not work on Linux.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

[ "$(uname -s)" = Linux ] || die "run this on Linux (CI: .github/workflows/release-build.yml)"
ARCH="${1:-}"
case "$ARCH" in
  x86_64) TARGET=x86_64-unknown-linux-gnu ;;
  aarch64) TARGET=aarch64-unknown-linux-gnu ;;
  *) die "usage: build-linux.sh x86_64|aarch64" ;;
esac
load_relay_token

NAME="connecto-${VERSION}-linux-${ARCH}"
WORK="$DIST/work/$NAME"
rm -rf "$WORK"
mkdir -p "$WORK/$NAME" "$DIST"

cd "$ROOT"
"$ROOT/scripts/release/check-release-features.sh" "$TARGET" || die "the release feature check failed"
log "cargo build --release --target $TARGET"
cargo build --release --locked --target "$TARGET" -p connecto-app -p connecto-hook
for bin in connecto-app connecto-hook; do cp "target/$TARGET/release/$bin" "$WORK/$NAME/$bin"; done
ln -s connecto-hook "$WORK/$NAME/connecto"
ship_readme > "$WORK/$NAME/README.txt"

TAR="$DIST/$NAME.tar.gz"
tar -C "$WORK" -czf "$TAR" "$NAME"
record_checksum "$TAR"
rm -rf "$WORK"
echo "built: $TAR"
