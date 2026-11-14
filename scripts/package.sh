#!/usr/bin/env bash
#
# Build a signed, installable bundle.
#
# Tauri's bundler lives in its CLI, which is a separate install:
#
#   cargo install tauri-cli --version '^2' --locked
#
# The Rust build alone (cargo build -p agi-app) produces a runnable binary, but
# not a .app, a .dmg, an .msi or an .exe installer — those need the CLI, and on
# macOS a signature and a notarization ticket before Gatekeeper will open them
# without a warning.
#
# Usage:
#   scripts/package.sh              # the current platform
#   scripts/package.sh --universal  # macOS: arm64 + x86_64 in one bundle
#
set -euo pipefail

cd "$(dirname "$0")/.."

if ! cargo tauri --version >/dev/null 2>&1; then
  cat >&2 <<'MISSING'
cargo-tauri is not installed.

  cargo install tauri-cli --version '^2' --locked

Without it, `cargo build -p agi-app --release` still produces a binary, but no
installer, no icon, no bundle metadata and no signature.
MISSING
  exit 1
fi

# The hook shim ships inside the app: the installer writes an absolute path to
# it into ~/.claude/settings.json, so it has to exist where the bundle puts it.
# It goes in as a Tauri resource — resources are signed and notarized with the
# bundle, which a file copied in afterwards would not be.
cargo build --release -p agi-hook-shim
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
SHIM="$TARGET_DIR/release/agi-mizar-hook"
if [[ "${1:-}" == "--universal" ]]; then
  SHIM="$TARGET_DIR/universal-apple-darwin/release/agi-mizar-hook"
fi
mkdir -p crates/agi-app/resources
cp "$SHIM" crates/agi-app/resources/agi-mizar-hook
chmod +x crates/agi-app/resources/agi-mizar-hook

# The remote-agent sidecar. It is what the settings-page install button runs,
# so it has to be inside the signed bundle, not a path the user happened to
# build in a target directory.
cargo build --release -p agi-remote
REMOTE="$TARGET_DIR/release/agi-remote"
if [[ "${1:-}" == "--universal" ]]; then
  REMOTE="$TARGET_DIR/universal-apple-darwin/release/agi-remote"
fi
mkdir -p crates/agi-app/resources/bin
cp "$REMOTE" crates/agi-app/resources/bin/agi-remote
chmod +x crates/agi-app/resources/bin/agi-remote

TARGET_ARGS=()
if [[ "${1:-}" == "--universal" ]]; then
  # One bundle that runs natively on both Apple Silicon and Intel. The CLI builds
  # both and lipos them; it needs both targets installed.
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  TARGET_ARGS=(--target universal-apple-darwin)
fi

# Signing and notarization, when the machine can do them.
#
# Tauri's bundler signs and notarizes on its own when these are set. Checking
# here turns "the DMG looked fine" into a sentence about what it actually is.
if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  if ! security find-identity -v -p codesigning | grep -qF "$APPLE_SIGNING_IDENTITY"; then
    echo "warning: APPLE_SIGNING_IDENTITY is not in this keychain; the bundle will be ad-hoc signed" >&2
  fi
  if [[ -z "${APPLE_ID:-}" || -z "${APPLE_PASSWORD:-}" || -z "${APPLE_TEAM_ID:-}" ]]; then
    echo "warning: Apple ID credentials are incomplete; the bundle will be signed but not notarized" >&2
  fi
else
  echo "note: no APPLE_SIGNING_IDENTITY; the bundle is ad-hoc signed and runs only on this Mac" >&2
fi

# The updater's signature is what makes a downloaded bundle trusted; without the
# key the CLI refuses to produce the updater artifacts, which is the correct
# failure for a release but not for a local build.
UPDATER_ARTIFACTS=false
if [[ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" || -n "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" ]]; then
  UPDATER_ARTIFACTS=true
else
  echo "note: no TAURI_SIGNING_PRIVATE_KEY; updater artifacts will not be produced" >&2
fi

if [[ ! -x crates/agi-app/resources/bin/llama-server ]]; then
  echo "note: no llama-server in resources/bin; run scripts/fetch-llama.sh" >&2
fi

set +e
cargo tauri build ${TARGET_ARGS[@]+"${TARGET_ARGS[@]}"} \
  --config "{\"bundle\":{\"createUpdaterArtifacts\":${UPDATER_ARTIFACTS},\"resources\":{\"resources/agi-mizar-hook\":\"agi-mizar-hook\",\"resources/bin\":\"bin\"}}}"
BUNDLE_STATUS=$?
set -e

# Nested binaries are not signed by the bundler: it signs MacOS/, Frameworks/
# and friends, not Resources/. Every dylib and the runner itself have to be
# signed with the same identity first, then the app re-signed around them. Do
# not use --deep here: it signs with the wrong options and hides the real error.
APP_DIR="$TARGET_DIR/release/bundle/macos/AGI Mizar.app"
if [[ "${#TARGET_ARGS[@]}" -gt 0 ]]; then
  APP_DIR="$TARGET_DIR/${TARGET_ARGS[1]}/release/bundle/macos/AGI Mizar.app"
fi
if [[ ! -d "$APP_DIR" ]]; then
  echo "error: tauri build failed before producing $APP_DIR (exit $BUNDLE_STATUS)" >&2
  exit "$BUNDLE_STATUS"
fi
if [[ "$BUNDLE_STATUS" -ne 0 ]]; then
  echo "warning: tauri exited $BUNDLE_STATUS after producing the app bundle; continuing without updater artifacts" >&2
fi
if [[ -d "$APP_DIR/Contents/Resources/bin" && -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  find "$APP_DIR/Contents/Resources/bin" -type f -name '*.dylib' -print0 \
    | xargs -0 -I{} codesign --force --options runtime --timestamp \
        --sign "$APPLE_SIGNING_IDENTITY" {}
  codesign --force --options runtime --timestamp \
    --sign "$APPLE_SIGNING_IDENTITY" "$APP_DIR/Contents/Resources/bin/llama-server"
  codesign --force --options runtime --timestamp \
    --sign "$APPLE_SIGNING_IDENTITY" "$APP_DIR/Contents/Resources/bin/agi-remote"
  codesign --force --options runtime --timestamp \
    --sign "$APPLE_SIGNING_IDENTITY" "$APP_DIR"
fi

# The bundler notarizes the .app, but the disk image around it is ours to
# submit. A DMG that is signed and not notarized is still rejected by
# Gatekeeper — "Unnotarized Developer ID" — so this step is not optional for a
# build somebody else will install.
BUNDLE_DIR="$TARGET_DIR/release/bundle"
if [[ "${#TARGET_ARGS[@]}" -gt 0 ]]; then
  BUNDLE_DIR="$TARGET_DIR/${TARGET_ARGS[1]}/release/bundle"
fi
DMG="$(find "$BUNDLE_DIR/dmg" -maxdepth 1 -name '*.dmg' -print -quit 2>/dev/null || true)"
if [[ -n "$DMG" ]]; then
  if ! xcrun stapler validate "$DMG" >/dev/null 2>&1; then
    if [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
      echo "notarizing $DMG"
      xcrun notarytool submit "$DMG" \
        --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID" \
        --wait
      xcrun stapler staple "$DMG"
    else
      echo "warning: $DMG is not notarized (set APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID)" >&2
    fi
  fi
  xcrun stapler validate "$DMG" >/dev/null 2>&1 && echo "dmg: notarized and stapled"
fi

# The Tauri CLI writes the updater artifacts (the .app.tar.gz and its .sig) but
# not `latest.json`: that manifest is a release-level merge of every platform and
# its URLs point at the GitHub release. Write the local one here with the same
# tool CI runs, so a hand-built release can be published with the manifest the
# updater expects. GitHub renames uploaded assets (spaces become dots), and the
# tool applies that rule when it builds the URL.
LATEST_JSON=""
if [[ "$UPDATER_ARTIFACTS" == true ]]; then
  SIG_FILE="$(find "$BUNDLE_DIR/macos" -maxdepth 1 -name '*.app.tar.gz.sig' -print -quit 2>/dev/null || true)"
  VERSION="$(sed -n 's/^[[:space:]]*"version": "\([0-9][^"]*\)".*/\1/p' crates/agi-app/tauri.conf.json | head -1)"
  if [[ -z "$SIG_FILE" ]]; then
    echo "warning: updater artifacts were enabled but no .app.tar.gz.sig exists under $BUNDLE_DIR/macos; latest.json not written" >&2
  elif [[ -z "$VERSION" ]]; then
    echo "warning: cannot read the bundle version from crates/agi-app/tauri.conf.json; latest.json not written" >&2
  elif ! command -v node >/dev/null 2>&1; then
    echo "error: node is not on PATH; scripts/updater-json.mjs writes latest.json for updater builds" >&2
    exit 1
  else
    LATEST_JSON="$BUNDLE_DIR/latest.json"
    node scripts/updater-json.mjs \
      --version "$VERSION" \
      --tag "v$VERSION" \
      --repo "${RELEASE_REPO:-ShirazuNagisa/AGI-Mizar}" \
      --out "$LATEST_JSON" \
      --platform "darwin-aarch64=$SIG_FILE" \
      --platform "darwin-x86_64=$SIG_FILE"
    echo "latest.json: $LATEST_JSON (darwin-aarch64, darwin-x86_64)"
  fi
fi

if [[ -n "${APPLE_SIGNING_IDENTITY:-}" && -n "${APPLE_ID:-}" ]]; then
  cat <<DONE

Built, signed and notarized. Gatekeeper on another Mac will open it.

DONE
else
  cat <<'NEXT'

Built. What the bundle still needs before anybody can install it:

  macOS
    APPLE_SIGNING_IDENTITY="Developer ID Application: …"
    APPLE_ID, APPLE_PASSWORD (an app-specific password), APPLE_TEAM_ID
    The CLI signs and notarizes when those are set; without them the .app runs
    only on this machine (right-click → Open), and the .dmg is unsigned.

  Windows
    A code-signing certificate in the user's store. Without one, SmartScreen
    warns on first run; the installer still works.

  Both
    The first launch should offer to install the Claude Code hooks rather than
    doing it silently — ~/.claude/settings.json belongs to the operator.
NEXT
fi

# The release upload needs both the signed bundle and the manifest that points
# at it; saying which of the two is missing here saves a failed release.
if [[ -n "$LATEST_JSON" ]]; then
  echo "updater: latest.json is at $LATEST_JSON — upload it next to the .app.tar.gz and its .sig"
elif [[ "$UPDATER_ARTIFACTS" == true ]]; then
  echo "updater: no latest.json was written (see the warning above)" >&2
fi
