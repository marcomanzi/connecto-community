#!/usr/bin/env bash
# Build the x64 Windows ZIP with MinGW in the local release container.
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
cd "$ROOT"
load_relay_token
target=x86_64-pc-windows-gnu
"$ROOT/scripts/release/check-release-features.sh" "$target"
cargo build --release --locked --target "$target" -p connecto-app -p connecto-hook
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/Connecto" "$DIST"
built="${CARGO_TARGET_DIR:-target}/$target/release"
cp "$built/connecto-app.exe" "$built/connecto-hook.exe" "$work/Connecto/"
cp "$built/connecto-hook.exe" "$work/Connecto/connecto.exe"
# Include MinGW runtime libraries if the linker imported them.
for dll in libgcc_s_seh-1.dll libstdc++-6.dll libwinpthread-1.dll; do
  if x86_64-w64-mingw32-objdump -p "$work"/Connecto/*.exe | grep -i "DLL Name: $dll" >/dev/null; then
    cp "$(x86_64-w64-mingw32-gcc -print-file-name="$dll")" "$work/Connecto/"
  fi
done
cat > "$work/Connecto/README.txt" <<TXT
Connecto $VERSION for Windows x64
Extract the Connecto folder into %LOCALAPPDATA%\Programs and start connecto-app.exe.
Keep the helper programs beside the app. This build is unsigned.
https://connecto.studio
TXT
archive="connecto-$VERSION-windows-x86_64.zip"
(cd "$work" && zip -qr "$archive" Connecto)
mv "$work/$archive" "$DIST/$archive"
record_checksum "$DIST/$archive"
