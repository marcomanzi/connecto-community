#!/usr/bin/env bash
# Cross-build the Windows x64 Flutter companion on the Mac. Inputs come from
# HEAD (the watcher's detached tag); the package carries the Flutter app version.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
source "$here/common.sh"
load_relay_token
if [ -z "${FLUTTER_ROOT:-}" ]; then
  sdk="$(command -v flutter || true)"
  FLUTTER_ROOT="${sdk%/bin/flutter}"
  FLUTTER_ROOT="${FLUTTER_ROOT:-$HOME/development/flutter}"
fi
export FLUTTER_ROOT
flutter="$FLUTTER_ROOT/bin/flutter"
dart="$FLUTTER_ROOT/bin/dart"
test -x "$flutter" || die "Set FLUTTER_ROOT to the release Mac's Flutter SDK"
work="$(mktemp -d)"
trap 'rm -r "$work"' EXIT
mkdir -p "$work/src" "$work/zip/data/flutter_assets" "$DIST"
DIST="$(cd "$DIST" && pwd)"
git -C "$ROOT" archive HEAD | tar -xf - -C "$work/src"
cd "$work/src/connecto-flutter-app"
"$flutter" pub get --enforce-lockfile
version="$(awk '/^version:/ {print $2}' pubspec.yaml)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+\+[0-9]+$ ]] || die "Invalid Flutter app version: $version"
engine="$(cat "$FLUTTER_ROOT/bin/cache/engine.stamp")"
base="https://storage.googleapis.com/flutter_infra_release/flutter/$engine"
for file in windows-x64-release/windows-x64-flutter.zip windows-x64/flutter-cpp-client-wrapper.zip; do
  curl -fsSL --retry 3 "$base/$file" -o "$work/archive.zip"
  unzip -qo "$work/archive.zip" -d "$work/engine"
done
"$flutter" assemble --output="$work/flutter-output" -dTargetPlatform=windows-x64 \
  -dBuildMode=release -dTrackWidgetCreation=false kernel_snapshot_program
cp .dart_tool/flutter_build/*/app.dill "$work/app.dill"
"$dart" --packages="$FLUTTER_ROOT/packages/flutter_tools/.dart_tool/package_config.json" \
  "$here/chat-windows/bundle_assets.dart" "$work/zip/data/flutter_assets"
cp "$work/engine/flutter_windows.dll" "$work/zip/"
cp "$FLUTTER_ROOT/bin/cache/artifacts/engine/darwin-x64/icudtl.dat" "$work/zip/data/"
python3 - "$work" <<'PY'
from pathlib import Path
import json, shutil, sys
from urllib.parse import unquote, urlparse

work = Path(sys.argv[1])
plugins = {"desktop_drop", "file_selector_windows"}
registered = {p["name"] for p in json.loads(Path(".flutter-plugins-dependencies").read_text())["plugins"]["windows"] if p.get("native_build") and p["name"] != "jni"}
if registered != plugins:
    sys.exit(f"Review Windows plugin build inputs: {registered}")
# jni's upstream Windows CMake only builds a library when a host JVM is requested.
for package in json.loads(Path(".dart_tool/package_config.json").read_text())["packages"]:
    if package["name"] in plugins:
        source = Path(unquote(urlparse(package["rootUri"]).path)) / "windows"
        shutil.copytree(source, work / package["name"])
manifest = {"format-version": [1, 0, 0], "native-assets": {"windows_x64": {
    "package:connecto_flutter_app/peer.dart": ["absolute", "connecto_flutter_peer.dll"]}}}
(work / "zip/data/flutter_assets/NativeAssetsManifest.json").write_text(json.dumps(manifest))
PY

docker build --platform linux/arm64 --target windows -t connecto-release-windows-arm64 - < "$here/Dockerfile"
docker run --rm --platform linux/arm64 -e CONNECTO_RELAY_TOKEN -e CARGO_TARGET_DIR=/target \
  -v "$work:/work" -v "$here/chat-windows:/tools:ro" \
  -v connecto-release-windows-arm64-target:/target -v connecto-release-cargo:/usr/local/cargo/registry \
  connecto-release-windows-arm64 bash /tools/compile.sh "$version"

# This compiler ships with the same Windows engine DLL. It runs on the Mac
# through Wine and produces the Windows AOT snapshot from the Windows kernel.
docker build --platform linux/amd64 -t connecto-chat-wine10 - < "$here/chat-windows/Dockerfile"
docker run --rm --platform linux/amd64 -e WINEDEBUG=-all -v "$work:/work" -w /work \
  connecto-chat-wine10 wine engine/gen_snapshot.exe --deterministic \
  --snapshot_kind=app-aot-elf --elf=zip/data/app.so --strip app.dill

for file in connecto_flutter_app.exe flutter_windows.dll connecto_flutter_peer.dll data/app.so data/icudtl.dat data/flutter_assets/AssetManifest.bin; do
  test -s "$work/zip/$file" || die "Windows companion bundle is missing $file"
done
name="connecto-$VERSION-chat-windows-x86_64.zip"
(cd "$work/zip" && zip -qr "$work/$name" .)
mv "$work/$name" "$DIST/$name"
(cd "$DIST" && shasum -a 256 "$name" > "$name.sha256")
chmod 644 "$DIST/$name" "$DIST/$name.sha256"
record_checksum "$DIST/$name"
record_checksum "$DIST/$name.sha256"
