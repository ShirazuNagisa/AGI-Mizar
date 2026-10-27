#!/usr/bin/env bash
#
# Assemble a runnable .app around the debug binary.
#
# `cargo build -p agi-app` produces a binary, not a bundle — so the Dock draws a
# generic icon and the app has no name or identifier. Tauri's own bundler (see
# package.sh) needs the CLI and a release build; this is the development
# shortcut: the same binary, wrapped with the icon and Info.plist that
# tauri.conf.json declares, signed ad-hoc so macOS will open it.
#
# Launch it the same way the bundle would be launched:
#
#   AGI_RELAY_URL=... AGI_RELAY_TOKEN=... \
#     "target/debug/AGI Mizar.app/Contents/MacOS/agi-mizar"
#
# The executable path inside Contents/MacOS is what makes macOS read the bundle's
# Info.plist and draw its icon.
#
# Usage:
#   scripts/dev-app.sh          # build the bundle
#   scripts/dev-app.sh --open   # build it and open it
#
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build -p agi-app

APP="target/debug/AGI Mizar.app"
RESOURCES="$APP/Contents/Resources"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$RESOURCES"

cp target/debug/agi-mizar "$APP/Contents/MacOS/agi-mizar"
cp crates/agi-app/icons/icon.icns "$RESOURCES/icon.icns"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>AGI Mizar</string>
  <key>CFBundleExecutable</key>
  <string>agi-mizar</string>
  <key>CFBundleIconFile</key>
  <string>icon</string>
  <key>CFBundleIdentifier</key>
  <string>me.shirazu.agimizar</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>AGI Mizar</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

printf 'APPL????' > "$APP/Contents/PkgInfo"

# Ad-hoc: enough for a bundle launched from this machine, not for distribution.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "$APP"

if [[ "${1:-}" == "--open" ]]; then
  open "$APP"
fi
