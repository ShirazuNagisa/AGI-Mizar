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

# The hook shim ships beside the app: the installer writes an absolute path to it
# into ~/.claude/settings.json, so it has to exist where the bundle puts it.
cargo build --release -p agi-mizar-hook

TARGET_ARGS=()
if [[ "${1:-}" == "--universal" ]]; then
  # One bundle that runs natively on both Apple Silicon and Intel. The CLI builds
  # both and lipos them; it needs both targets installed.
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  TARGET_ARGS=(--target universal-apple-darwin)
fi

cargo tauri build "${TARGET_ARGS[@]}"

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
