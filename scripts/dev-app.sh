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
#     "${CARGO_TARGET_DIR:-target}/debug/AGI Mizar.app/Contents/MacOS/agi-mizar"
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

# Cargo decides where the binary lands, and `CARGO_TARGET_DIR` is how a machine
# moves that directory — this one does, to a volume outside the repository.
# Asking Cargo the same question is the point: a hardcoded `target/debug` does
# not fail when the two disagree, it quietly packages whatever that directory
# last held, so the app runs code that is no longer in the tree.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

# `release` embeds the window's assets in the binary; `debug` reads them from the
# repository at runtime, which on macOS means asking for Documents access on
# every launch. Debugging wants the fast build; running the app wants the quiet
# one.
PROFILE="${PROFILE:-debug}"
CARGO_PROFILE=()
[[ "$PROFILE" == "release" ]] && CARGO_PROFILE=(--release)

cargo build -p agi-app "${CARGO_PROFILE[@]}"
# The Claude hooks point at this binary by absolute path, so a dev bundle that
# lacks it cannot configure Claude at all — the guide fails at step three with
# "agi-mizar-hook was not found". Packaged builds get it from package.sh.
cargo build -p agi-hook-shim "${CARGO_PROFILE[@]}"

# The bundle lives beside the binary: one directory to clean, one place to look.
# A hidden parent keeps LaunchServices from indexing this copy: the visible app
# is the one in /Applications, and two bundles with one identifier means two
# Dock icons and two entries in Launchpad.
APP="$TARGET_DIR/$PROFILE/.dev-bundle/AGI Mizar.app"
RESOURCES="$APP/Contents/Resources"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$RESOURCES"

cp "$TARGET_DIR/$PROFILE/agi-mizar" "$APP/Contents/MacOS/agi-mizar"
cp crates/agi-app/icons/icon-dark.icns "$RESOURCES/icon.icns"

# The model runner is a bundle resource, exactly as packaged builds have it, so
# `Resources/bin/llama-server` is the path a dev build exercises too.
cp "$TARGET_DIR/$PROFILE/agi-mizar-hook" "$RESOURCES/agi-mizar-hook"
chmod +x "$RESOURCES/agi-mizar-hook"

if [[ -d crates/agi-app/resources/bin ]]; then
  mkdir -p "$RESOURCES/bin"
  cp -R crates/agi-app/resources/bin/. "$RESOURCES/bin/"
fi

# The version in the bundle is the workspace's, not a string kept by hand: a
# bundle that claims 0.1.0 while the kernel reports 0.1.1 is a bug report waiting
# to happen.
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
VERSION="${VERSION:-0.0.0}"

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
  <string>__VERSION__</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

sed -i '' "s/__VERSION__/$VERSION/" "$APP/Contents/Info.plist"

# Keep Spotlight and Launchpad out of the build tree.
printf '' > "$(dirname "$APP")/.metadata_never_index"

printf 'APPL????' > "$APP/Contents/PkgInfo"

# Ad-hoc: enough for a bundle launched from this machine, not for distribution.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "$APP"

if [[ "${1:-}" == "--open" ]]; then
  open "$APP"
fi
