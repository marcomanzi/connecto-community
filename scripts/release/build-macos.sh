#!/usr/bin/env bash
# Build Connecto for macOS: Connecto.app inside a .dmg in dist/.
#
#   scripts/release/build-macos.sh aarch64     Apple Silicon
#   scripts/release/build-macos.sh x86_64      Intel
#   scripts/release/build-macos.sh universal   both in one app (needs both Rust targets)
#
# Output (VERSION is the workspace version):
#   dist/connecto-VERSION-macos-aarch64.dmg
#   dist/connecto-VERSION-macos-x86_64.dmg
#   dist/connecto-VERSION-macos-universal.dmg
#   dist/SHA256SUMS
#
# Signing and notarization are optional, driven by environment variables:
#   CONNECTO_SIGN_IDENTITY   a "Developer ID Application: ..." identity name or hash
#   CONNECTO_NOTARY_PROFILE  a `xcrun notarytool store-credentials` profile name
#                            (needs CONNECTO_SIGN_IDENTITY)
#   CONNECTO_ENTITLEMENTS    optional entitlements plist for the hardened runtime
# Without them the result is NOT signed (only the ad-hoc signature that Apple
# Silicon needs to run at all) and says so.
#
# Other variables:
#   CONNECTO_MIN_MACOS   minimum macOS version of the app (default 11.0 for
#                        aarch64, 10.15 for x86_64 and universal). Each slice is
#                        built for the lower of this and what its CPU can run:
#                        arm64 slices never go below 11.0.
#   CONNECTO_DIST        output folder (default ./dist)
#   CONNECTO_BUNDLE_ID   default studio.connecto.app
#
# The relay access token built into the app is required (see README.md):
#   CONNECTO_RELAY_TOKEN       the token, or
#   scripts/release/relay-token.local   a file holding it (untracked)
#   CONNECTO_NO_RELAY_TOKEN=1  build on purpose without one (no built-in access)
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

[ "$(uname -s)" = Darwin ] || die "macOS builds need a Mac (hdiutil, codesign, lipo)"
ARCH="${1:-}"
case "$ARCH" in
  aarch64) TARGETS=(aarch64-apple-darwin); DEFAULT_MIN=11.0 ;;
  x86_64) TARGETS=(x86_64-apple-darwin); DEFAULT_MIN=10.15 ;;
  universal) TARGETS=(aarch64-apple-darwin x86_64-apple-darwin); DEFAULT_MIN=10.15 ;;
  *) die "usage: build-macos.sh aarch64|x86_64|universal" ;;
esac
MIN_MACOS="${CONNECTO_MIN_MACOS:-$DEFAULT_MIN}"
load_relay_token

for t in "${TARGETS[@]}"; do
  rustup target list --installed | grep -qx "$t" || die "missing Rust target: rustup target add $t"
done

NAME="connecto-${VERSION}-macos-${ARCH}"
WORK="$DIST/work/$NAME"
APP="$WORK/Connecto.app"
rm -rf "$WORK"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$DIST"

cd "$ROOT"
for t in "${TARGETS[@]}"; do
  slice_min="$MIN_MACOS"
  # Apple Silicon has no macOS before 11.0.
  if [ "$t" = aarch64-apple-darwin ] && [ "$(printf '%s\n11.0\n' "$slice_min" | sort -V | head -1)" != 11.0 ]; then slice_min=11.0; fi
  "$ROOT/scripts/release/check-release-features.sh" "$t" || die "the release feature check failed for $t"
  log "cargo build --release --target $t (macOS >= $slice_min)"
  MACOSX_DEPLOYMENT_TARGET="$slice_min" \
    cargo build --release --locked --target "$t" -p connecto-app -p connecto-hook
done

for bin in connecto-app connecto-hook; do
  if [ "${#TARGETS[@]}" -eq 1 ]; then
    cp "target/${TARGETS[0]}/release/$bin" "$APP/Contents/MacOS/$bin"
  else
    lipo -create -output "$APP/Contents/MacOS/$bin" \
      "target/aarch64-apple-darwin/release/$bin" "target/x86_64-apple-darwin/release/$bin"
  fi
done
# The `connecto` command is the helper under another name.
ln -s connecto-hook "$APP/Contents/MacOS/connecto"

# The icon, from the brand image.
ICONSET="$WORK/Connecto.iconset"
mkdir -p "$ICONSET"
SRC="$ROOT/assets/brand/icon-256.png"
for size in 16 32 64 128 256; do
  sips -z "$size" "$size" "$SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
done
cp "$ICONSET/icon_32x32.png" "$ICONSET/icon_16x16@2x.png"
cp "$ICONSET/icon_64x64.png" "$ICONSET/icon_32x32@2x.png"
cp "$ICONSET/icon_256x256.png" "$ICONSET/icon_128x128@2x.png"
rm -f "$ICONSET/icon_64x64.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Connecto.icns"
rm -rf "$ICONSET"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>connecto-app</string>
  <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
  <key>CFBundleName</key><string>Connecto</string>
  <key>CFBundleDisplayName</key><string>Connecto</string>
  <key>CFBundleIconFile</key><string>Connecto</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>${MIN_MACOS}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

# ---- signing ---------------------------------------------------------------
SIGNED=no
if [ -n "${CONNECTO_SIGN_IDENTITY:-}" ]; then
  log "signing with the identity named in CONNECTO_SIGN_IDENTITY (hardened runtime)"
  ARGS=(--force --timestamp --options runtime --sign "$CONNECTO_SIGN_IDENTITY")
  [ -z "${CONNECTO_ENTITLEMENTS:-}" ] || ARGS+=(--entitlements "$CONNECTO_ENTITLEMENTS")
  # Inside out: the helpers first, then the bundle.
  codesign "${ARGS[@]}" "$APP/Contents/MacOS/connecto-hook"
  codesign "${ARGS[@]}" "$APP/Contents/MacOS/connecto-app"
  codesign "${ARGS[@]}" "$APP"
  codesign --verify --deep --strict "$APP"
  SIGNED=yes
else
  log "NOT SIGNED: CONNECTO_SIGN_IDENTITY is not set; applying an ad-hoc signature only"
  codesign --force --sign - "$APP/Contents/MacOS/connecto-hook"
  codesign --force --sign - "$APP/Contents/MacOS/connecto-app"
  codesign --force --sign - "$APP"
fi

# ---- the disk image --------------------------------------------------------
DMG="$DIST/$NAME.dmg"
STAGE="$WORK/dmg"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
ship_readme > "$STAGE/README.txt"
rm -f "$DMG"
hdiutil create -quiet -volname "Connecto ${VERSION}" -srcfolder "$STAGE" -format UDZO -ov "$DMG"
if [ "$SIGNED" = yes ]; then
  # Apple's timestamp server fails now and then ("A timestamp was expected but
  # was not found"): try again before giving up.
  for try in 1 2 3; do
    codesign --force --timestamp --sign "$CONNECTO_SIGN_IDENTITY" "$DMG" && break
    [ "$try" = 3 ] && die "could not sign $DMG"
    log "signing the disk image failed; trying again in 20s"
    sleep 20
  done
fi

# ---- notarization ----------------------------------------------------------
NOTARIZED=no
if [ -n "${CONNECTO_NOTARY_PROFILE:-}" ]; then
  [ "$SIGNED" = yes ] || die "CONNECTO_NOTARY_PROFILE needs CONNECTO_SIGN_IDENTITY: only a signed build can be notarized"
  log "notarizing (this waits for Apple)"
  xcrun notarytool submit "$DMG" --keychain-profile "$CONNECTO_NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG"
  NOTARIZED=yes
fi

record_checksum "$DMG"

# ---- the update archive ------------------------------------------------------
# What the in-app updater installs: the app alone, zipped with ditto so its
# signature and attributes survive. Per architecture only (the feed has an
# entry for each); a notarized app gets its ticket stapled first, so the
# updater's Gatekeeper check passes offline too.
UPDATE_ZIP=""
if [ "$ARCH" != universal ]; then
  if [ "$NOTARIZED" = yes ]; then
    xcrun stapler staple "$APP" || log "the app could not be stapled; Gatekeeper will check it online"
  fi
  UPDATE_ZIP="$DIST/$NAME.app.zip"
  rm -f "$UPDATE_ZIP"
  ditto -c -k --keepParent "$APP" "$UPDATE_ZIP"
  record_checksum "$UPDATE_ZIP"
fi
rm -rf "$WORK"
echo
echo "built:      $DMG"
[ -z "$UPDATE_ZIP" ] || echo "update:     $UPDATE_ZIP"
echo "signed:     $SIGNED"
echo "notarized:  $NOTARIZED"
if [ "$SIGNED" = no ]; then
  echo "This build is UNSIGNED: macOS Gatekeeper will refuse it on other Macs until it is"
  echo "signed and notarized (set CONNECTO_SIGN_IDENTITY and CONNECTO_NOTARY_PROFILE)."
fi
