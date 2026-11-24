#!/usr/bin/env bash
# Run inside the release Mac's MinGW container, in a disposable checkout.
set -euo pipefail
cd /work
runner=src/connecto-flutter-app/windows/runner
windows=src/connecto-flutter-app/windows
wrapper=engine/cpp_client_wrapper
IFS=. read -r major minor patch <<< "${1%+*}"
build="${1#*+}"

# windres reads host paths; Visual Studio embeds the manifest separately.
sed 's|resources\\\\app_icon.ico|resources/app_icon.ico|' "$runner/Runner.rc" > Runner.rc
echo '1 24 "runner.exe.manifest"' >> Runner.rc
x86_64-w64-mingw32-windres -I "$runner" "-DFLUTTER_VERSION_MAJOR=$major" \
  "-DFLUTTER_VERSION_MINOR=$minor" "-DFLUTTER_VERSION_PATCH=$patch" \
  "-DFLUTTER_VERSION_BUILD=$build" "-DFLUTTER_VERSION=\\\"$1\\\"" Runner.rc -O coff -o runner.res

# MinGW defines a function for each COM smart pointer declaration. The header
# already declares IFileDialogPtr, so omit the duplicate from the plugin copy.
sed -i '/^_COM_SMARTPTR_TYPEDEF(IFileDialog, IID_IFileDialog);$/d' file_selector_windows/file_selector_plugin.cpp
x86_64-w64-mingw32-g++ -std=c++17 -O2 -DNDEBUG -DNOMINMAX -DUNICODE -D_UNICODE -DFLUTTER_PLUGIN_IMPL \
  -include windows.h -include shellapi.h -Iengine -I"$wrapper/include" -I"$windows" \
  -Idesktop_drop/include -Ifile_selector_windows/include \
  "$runner"/*.cpp "$windows"/flutter/generated_plugin_registrant.cc \
  "$wrapper"/{core_implementations,standard_codec,plugin_registrar,flutter_engine,flutter_view_controller}.cc \
  desktop_drop/*.cpp file_selector_windows/*.cpp runner.res engine/flutter_windows.dll.lib \
  -static -municode -mwindows -ldwmapi -lole32 -luuid -lshell32 -lshlwapi -lcomdlg32 -loleaut32 \
  -o zip/connecto_flutter_app.exe
cd src/connecto-flutter-app/native
export RUSTUP_TOOLCHAIN="$(rustup default | awk '{print $1}')"
cargo build --locked --release --target x86_64-pc-windows-gnu
cp "$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/connecto_flutter_peer.dll" /work/zip/
