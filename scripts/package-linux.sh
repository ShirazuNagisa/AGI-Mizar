#!/usr/bin/env bash
#
# Build the public-beta Linux artifacts: a portable tarball and a Debian package.
#
# The binary is the same headless kernel the desktop app uses; both artifacts
# register the browser UI (systemd user unit + desktop entry) and the launcher
# bootstraps the per-user config on first use. Nothing here installs or restarts
# a service on the build machine.
#
# Usage:
#   scripts/package-linux.sh [--version 0.1.2] [--beta 1]
#                            [--out DIR] [--skip-ui] [--skip-build]
#
# Output:
#   agi-mizar-<version>-beta.<n>-linux-<arch>.tar.gz
#   agi-mizar_<version>~beta.<n>_<debarch>.deb
#   SHA256SUMS
#   RELEASE-NOTES.md
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
BETA=1
OUT_DIR="${AGI_MIZAR_OUT:-$REPO_ROOT/target/linux-package}"
SKIP_UI=0
SKIP_BUILD=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --beta) BETA="${2:?--beta needs a number}"; shift 2 ;;
    --out) OUT_DIR="${2:?--out needs a directory}"; shift 2 ;;
    --skip-ui) SKIP_UI=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "package-linux: unknown argument: $1" >&2; exit 2 ;;
  esac
done

RELEASE_VERSION="${VERSION}-beta.${BETA}"
DEB_VERSION="${VERSION}~beta.${BETA}"
COMMIT="${AGI_MIZAR_COMMIT:-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)}"
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64; DEB_ARCH=amd64 ;;
  aarch64|arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
  *) echo "package-linux: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

[ -n "$OUT_DIR" ] && [ "$OUT_DIR" != "/" ] && [ "$OUT_DIR" != "$HOME" ] \
  || { echo "package-linux: refusing an unsafe --out" >&2; exit 1; }
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BIN="$TARGET_DIR/release/agi-daemon"

if [ "$SKIP_UI" = 0 ]; then
  command -v npm >/dev/null 2>&1 || { echo "package-linux: npm is required (or pass --skip-ui)" >&2; exit 1; }
  [ -d ui/node_modules ] || npm --prefix ui ci
  npm --prefix ui run build
fi
if [ "$SKIP_BUILD" = 0 ]; then
  cargo build --release -p agi-daemon
fi
[ -x "$BIN" ] || { echo "package-linux: no release daemon at $BIN" >&2; exit 1; }

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
STAGE="$OUT_DIR/agi-mizar-$RELEASE_VERSION-linux-$ARCH"
DEB_ROOT="$OUT_DIR/deb-root"
mkdir -p "$STAGE/bin" "$STAGE/scripts/linux" "$STAGE/share/icons" "$STAGE/share/doc"
mkdir -p "$DEB_ROOT/DEBIAN" "$DEB_ROOT/usr/lib/agi-mizar" "$DEB_ROOT/usr/bin" \
  "$DEB_ROOT/usr/lib/systemd/user" "$DEB_ROOT/usr/share/applications" \
  "$DEB_ROOT/usr/share/icons/hicolor/128x128/apps" "$DEB_ROOT/usr/share/doc/agi-mizar"

install -m 0755 "$BIN" "$STAGE/bin/agi-daemon"
if command -v strip >/dev/null 2>&1; then
  strip --strip-unneeded "$STAGE/bin/agi-daemon" 2>/dev/null || true
fi
install -m 0755 scripts/linux/agi-mizar-web.sh "$STAGE/scripts/linux/agi-mizar-web.sh"
install -m 0755 scripts/linux/install-webui.sh "$STAGE/scripts/linux/install-webui.sh"
install -m 0644 crates/agi-app/icons/128x128.png "$STAGE/share/icons/agi-mizar.png"
install -m 0644 docs/linux.md "$STAGE/share/doc/linux.md"
install -m 0644 LICENSE "$STAGE/LICENSE"

cat > "$STAGE/README.md" <<EOF
# AGI Mizar $RELEASE_VERSION (Linux $ARCH)

Portable public beta. The kernel is the same one the macOS/Windows app uses;
the browser UI is the same React application.

## Install (per user)

\`\`\`sh
./scripts/linux/install-webui.sh
agi-mizar-web open
\`\`\`

The installer copies the kernel to \`~/.local/lib/agi-mizar\`, writes
\`~/.config/agi-mizar/web.env\` (0600), registers the
\`agi-mizar-web.service\` systemd user unit and the **AGI Mizar** desktop entry.
No root, relay or nginx changes.

## Prerequisites

- Linux with systemd user session, \`curl\`, and \`xdg-open\` (or \`gio open\`).
- DSH CLI + \`dsh-mizar\` plugin for the DSH engine:
  \`npm i -g @deepseek-ai/dsh@0.2.1-rc.2\` then
  \`dsh plugin --profile web add dsh-mizar@0.9.0\`.
  Without them the UI opens but DSH is disconnected.

## Headless

\`\`\`sh
./scripts/linux/install-webui.sh --no-desktop --linger
ssh -N -L 64222:127.0.0.1:64222 user@server
agi-mizar-web url
\`\`\`

## Uninstall

\`\`\`sh
./scripts/linux/install-webui.sh --uninstall
\`\`\`

Keeps \`~/.agi-mizar\` data and the config file.

This is an unsigned public beta. Verify \`SHA256SUMS\` before running.
EOF

tar --owner=0 --group=0 -czf "$OUT_DIR/agi-mizar-$RELEASE_VERSION-linux-$ARCH.tar.gz" \
  -C "$OUT_DIR" "$(basename "$STAGE")"

# Debian package: same files in system paths; the helper bootstraps the per-user
# config and starts the user unit on first launch.
install -m 0755 "$BIN" "$DEB_ROOT/usr/lib/agi-mizar/agi-daemon"
strip --strip-unneeded "$DEB_ROOT/usr/lib/agi-mizar/agi-daemon" 2>/dev/null || true
install -m 0755 scripts/linux/agi-mizar-web.sh "$DEB_ROOT/usr/lib/agi-mizar/agi-mizar-web.sh"
install -m 0644 crates/agi-app/icons/128x128.png \
  "$DEB_ROOT/usr/share/icons/hicolor/128x128/apps/agi-mizar.png"
install -m 0644 docs/linux.md "$DEB_ROOT/usr/share/doc/agi-mizar/README.md"
install -m 0644 LICENSE "$DEB_ROOT/usr/share/doc/agi-mizar/copyright"

cat > "$DEB_ROOT/usr/bin/agi-mizar-web" <<'EOF'
#!/bin/sh
exec /usr/lib/agi-mizar/agi-mizar-web.sh "$@"
EOF
chmod 0755 "$DEB_ROOT/usr/bin/agi-mizar-web"

cat > "$DEB_ROOT/usr/lib/systemd/user/agi-mizar-web.service" <<'EOF'
[Unit]
Description=AGI Mizar Web UI
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=%h/.config/agi-mizar/web.env
ExecStart=/usr/lib/agi-mizar/agi-daemon
WorkingDirectory=%h
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF

cat > "$DEB_ROOT/usr/share/applications/agi-mizar-web.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Version=1.0
Name=AGI Mizar
GenericName=Agent Manager
Comment=Configure AGI Mizar and manage agents in your browser
Exec=/usr/bin/agi-mizar-web open
Icon=agi-mizar
Terminal=false
Categories=Development;Utility;
Keywords=AGI;Mizar;DSH;agent;Claude;
StartupNotify=true
EOF

cat > "$DEB_ROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
fi
exit 0
EOF
chmod 0755 "$DEB_ROOT/DEBIAN/postinst"

INSTALLED_SIZE="$(du -sk "$DEB_ROOT" | awk '{print $1}')"
cat > "$DEB_ROOT/DEBIAN/control" <<EOF
Package: agi-mizar
Version: $DEB_VERSION
Section: utils
Priority: optional
Architecture: $DEB_ARCH
Installed-Size: $INSTALLED_SIZE
Maintainer: AGI Mizar <noreply@shirazu-nagisa.com>
Homepage: https://dsh-mizar.shirazu-nagisa.com
Depends: libc6 (>= 2.34), libgcc-s1
Description: AGI Mizar browser UI and headless kernel (public beta)
 The same React UI and kernel used by the AGI Mizar desktop app, packaged
 as a per-user Linux Web UI. It registers a systemd user service and a
 desktop entry; the first launch creates the per-user config and opens the
 browser. DSH and the dsh-mizar plugin are installed separately.
EOF

(cd "$DEB_ROOT" && find . -type f ! -path './DEBIAN/*' -printf '%P\0' \
  | sort -z | xargs -0 md5sum) > "$DEB_ROOT/DEBIAN/md5sums"

(cd "$DEB_ROOT/DEBIAN" && tar --owner=0 --group=0 -czf "$OUT_DIR/control.tar.gz" .)
(cd "$DEB_ROOT" && tar --owner=0 --group=0 -czf "$OUT_DIR/data.tar.gz" .)
printf '2.0\n' > "$OUT_DIR/debian-binary"
DEB_FILE="$OUT_DIR/agi-mizar_${DEB_VERSION}_${DEB_ARCH}.deb"
(cd "$OUT_DIR" && ar rc "$DEB_FILE" debian-binary control.tar.gz data.tar.gz)
rm -f "$OUT_DIR/control.tar.gz" "$OUT_DIR/data.tar.gz" "$OUT_DIR/debian-binary"
rm -rf "$DEB_ROOT" "$STAGE"

cat > "$OUT_DIR/RELEASE-NOTES.md" <<EOF
# AGI Mizar $RELEASE_VERSION — Linux public beta

- Artifact: \`agi-mizar-$RELEASE_VERSION-linux-$ARCH.tar.gz\`
- Debian: \`agi-mizar_${DEB_VERSION}_${DEB_ARCH}.deb\`
- Commit: \`$COMMIT\`
- Built: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- License: GPL-3.0-only

This is an unsigned public beta. The portable tarball is the supported path;
the .deb installs the same files under \`/usr/lib/agi-mizar\` and
\`/usr/bin/agi-mizar-web\`. DSH and \`dsh-mizar\` are prerequisites, not bundled.

Verify:

\`\`\`sh
sha256sum -c SHA256SUMS
\`\`\`
EOF

(cd "$OUT_DIR" && sha256sum agi-mizar-*.tar.gz agi-mizar_*.deb > SHA256SUMS)
echo "Built:"
ls -lh "$OUT_DIR"
echo "SHA256SUMS:"
cat "$OUT_DIR/SHA256SUMS"
