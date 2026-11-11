#!/usr/bin/env bash
#
# Install AGI Mizar's browser UI as a per-user Linux service.
#
# The kernel already serves the same React app the macOS and Windows windows
# load. This script registers that app where a Linux desktop expects to find it:
#
#   - a systemd --user unit that starts the kernel at login and restarts it;
#   - a desktop entry that opens the Web UI in the default browser;
#   - an `agi-mizar-web` command for headless boxes and for `url`/`status`/`logs`.
#
# It does not touch a system-wide AGI Mizar install, a relay, or nginx. The API
# listens on 127.0.0.1 by default; the launcher exchanges its token for an
# HttpOnly cookie, so the address bar and bookmarks stay credential-free.
#
# Usage:
#   scripts/linux/install-webui.sh [--build] [--port 64222] [--host 127.0.0.1]
#                                  [--binary PATH] [--install-dir DIR]
#                                  [--no-start] [--no-desktop] [--linger]
#                                  [--uninstall]
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

INSTALL_DIR="${AGI_MIZAR_INSTALL_DIR:-$HOME/.local/lib/agi-mizar}"
HOST="${AGI_MIZAR_WEB_HOST:-127.0.0.1}"
PORT="${AGI_MIZAR_WEB_PORT:-64222}"
BINARY="${AGI_MIZAR_BINARY:-}"
DO_BUILD=0
DO_START=1
DO_DESKTOP=1
DO_LINGER=0
UNINSTALL=0

usage() {
  awk 'NR > 2 && /^#/ { sub(/^# ?/, ""); print; next } NR > 2 { exit }' "$0"
}

die() {
  printf 'install-webui: %s\n' "$1" >&2
  exit 1
}

say() {
  printf '  %s\n' "$1"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --build) DO_BUILD=1; shift ;;
    --port) PORT="${2:?--port needs a number}"; shift 2 ;;
    --host) HOST="${2:?--host needs an address}"; shift 2 ;;
    --binary) BINARY="${2:?--binary needs a path}"; shift 2 ;;
    --install-dir) INSTALL_DIR="${2:?--install-dir needs a path}"; shift 2 ;;
    --no-start) DO_START=0; shift ;;
    --no-desktop) DO_DESKTOP=0; shift ;;
    --linger) DO_LINGER=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/agi-mizar"
ENV_FILE="$CONFIG_DIR/web.env"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT_FILE="$UNIT_DIR/agi-mizar-web.service"
DESKTOP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
DESKTOP_FILE="$DESKTOP_DIR/agi-mizar-web.desktop"
BIN_DIR="$HOME/.local/bin"
HELPER="$INSTALL_DIR/bin/agi-mizar-web"

[ -n "${HOME:-}" ] || die "HOME is not set"
case "$PORT" in
  ''|*[!0-9]*) die "--port must be a number" ;;
esac
[ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || die "--port must be between 1 and 65535"

if [ "$UNINSTALL" = 1 ]; then
  systemctl --user disable --now agi-mizar-web.service >/dev/null 2>&1 || true
  rm -f "$UNIT_FILE" "$DESKTOP_FILE" "$BIN_DIR/agi-mizar-web"
  rm -f "$INSTALL_DIR/bin/agi-daemon" "$INSTALL_DIR/bin/agi-mizar-web" \
        "$INSTALL_DIR/share/icons/agi-mizar.png"
  systemctl --user daemon-reload >/dev/null 2>&1 || true
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
  fi
  rmdir "$INSTALL_DIR/bin" "$INSTALL_DIR/share/icons" "$INSTALL_DIR/share" \
        "$INSTALL_DIR" 2>/dev/null || true
  printf 'AGI Mizar Web UI removed.\n'
  printf 'Kept your data (~/.agi-mizar) and config (%s).\n' "$CONFIG_DIR"
  exit 0
fi

if [ "$(id -u)" -eq 0 ]; then
  printf 'warning: run this as the desktop user, not root; a root user unit belongs to root.\n' >&2
fi
case "$HOST" in
  127.*|localhost|::1) ;;
  *)
    printf 'warning: AGI_API_HOST=%s exposes the token-protected API beyond loopback.\n' "$HOST" >&2
    printf '         Prefer an SSH tunnel; only continue if that exposure is intended.\n' >&2
    ;;
esac

if [ "$DO_BUILD" = 1 ]; then
  command -v cargo >/dev/null 2>&1 || die "--build needs cargo"
  command -v npm >/dev/null 2>&1 || die "--build needs npm"
  say "building the React UI and the headless kernel"
  ( cd "$REPO_ROOT" && { [ -d ui/node_modules ] || npm --prefix ui ci; } \
      && npm --prefix ui run build && cargo build --release -p agi-daemon )
  BINARY="$REPO_ROOT/target/release/agi-daemon"
fi

if [ -z "$BINARY" ]; then
  for candidate in \
    "$INSTALL_DIR/bin/agi-daemon" \
    "$REPO_ROOT/target/release/agi-daemon" \
    "$REPO_ROOT/target/debug/agi-daemon"
  do
    if [ -x "$candidate" ]; then
      BINARY="$candidate"
      break
    fi
  done
fi
[ -n "$BINARY" ] || die "no agi-daemon binary; run with --build or pass --binary PATH"
[ -x "$BINARY" ] || die "$BINARY is not executable"

# A second install on the same port would fight the first one. If our own unit
# is already running it is a reinstall; otherwise the port is somebody else's.
if command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | awk '{print $4}' | grep -Eq "[:.]${PORT}$"; then
  if ! systemctl --user is-active --quiet agi-mizar-web.service 2>/dev/null; then
    die "port $PORT is already in use; choose another with --port"
  fi
fi

say "installing to $INSTALL_DIR"
install -d -m 0755 "$INSTALL_DIR/bin" "$INSTALL_DIR/share/icons" \
  "$CONFIG_DIR" "$UNIT_DIR" "$BIN_DIR"
install -m 0755 "$BINARY" "$INSTALL_DIR/bin/agi-daemon"
install -m 0755 "$REPO_ROOT/scripts/linux/agi-mizar-web.sh" "$HELPER"
ln -sfn "$HELPER" "$BIN_DIR/agi-mizar-web"
install -m 0644 "$REPO_ROOT/crates/agi-app/icons/128x128.png" \
  "$INSTALL_DIR/share/icons/agi-mizar.png"

# Keep a token across reinstalls: the browser cookie and the service's env must
# agree, and rotating it would silently log every open tab out.
TOKEN=""
if [ -r "$ENV_FILE" ]; then
  TOKEN="$(sed -n 's/^AGI_API_TOKEN=//p' "$ENV_FILE" | head -n 1)"
fi
if [ -z "$TOKEN" ]; then
  if command -v openssl >/dev/null 2>&1; then
    TOKEN="$(openssl rand -hex 32)"
  else
    TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  fi
fi
[ "${#TOKEN}" -ge 32 ] || die "could not generate a token"

umask 077
TMP_ENV="$(mktemp "${TMPDIR:-/tmp}/agi-mizar-web.XXXXXX")"
if [ -r "$ENV_FILE" ]; then
  grep -vE '^(AGI_API_HOST|AGI_API_PORT|AGI_API_TOKEN|MIZAR_BIND|RUST_LOG)=' "$ENV_FILE" > "$TMP_ENV" || true
fi
{
  printf 'AGI_API_HOST=%s\n' "$HOST"
  printf 'AGI_API_PORT=%s\n' "$PORT"
  printf 'AGI_API_TOKEN=%s\n' "$TOKEN"
  printf 'MIZAR_BIND=127.0.0.1\n'
  printf 'RUST_LOG=agi_core=info,agi_engine_dsh=info,agi_mizar=info\n'
} >> "$TMP_ENV"
install -m 0600 "$TMP_ENV" "$ENV_FILE"
rm -f "$TMP_ENV"

cat > "$UNIT_FILE" <<EOF
[Unit]
Description=AGI Mizar Web UI
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$ENV_FILE
ExecStart=$INSTALL_DIR/bin/agi-daemon
WorkingDirectory=$HOME
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF
chmod 0644 "$UNIT_FILE"

if [ "$DO_DESKTOP" = 1 ]; then
  install -d -m 0755 "$DESKTOP_DIR"
  cat > "$DESKTOP_FILE" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=AGI Mizar
GenericName=Agent Manager
Comment=Configure AGI Mizar and manage agents in your browser
Exec=$HELPER open
Icon=$INSTALL_DIR/share/icons/agi-mizar.png
Terminal=false
Categories=Development;Utility;
Keywords=AGI;Mizar;DSH;agent;Claude;
StartupNotify=true
EOF
  chmod 0644 "$DESKTOP_FILE"
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
  fi
fi

systemctl --user daemon-reload >/dev/null 2>&1 || true
started=0
if [ "$DO_START" = 1 ]; then
  if systemctl --user enable --now agi-mizar-web.service >/dev/null 2>&1; then
    started=1
  else
    printf 'warning: could not start the user service from this shell.\n' >&2
    printf '         run: systemctl --user enable --now agi-mizar-web.service\n' >&2
  fi
fi

if [ "$DO_LINGER" = 1 ]; then
  if loginctl enable-linger "$(id -un)" >/dev/null 2>&1; then
    say "lingering enabled: the kernel starts at boot, before login"
  else
    printf 'warning: could not enable linger; ask an administrator to run:\n' >&2
    printf '         loginctl enable-linger %s\n' "$(id -un)" >&2
  fi
fi

printf '\nAGI Mizar Web UI installed.\n'
say "kernel:  $INSTALL_DIR/bin/agi-daemon"
say "config:  $ENV_FILE (0600)"
say "service: agi-mizar-web.service"
if [ "$DO_DESKTOP" = 1 ]; then
  say "launcher: $DESKTOP_FILE"
fi
if [ "$started" = 1 ]; then
  say "status:  agi-mizar-web status"
  say "open:    $HELPER open"
else
  say "start:   systemctl --user enable --now agi-mizar-web.service"
fi
if [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
  printf '\nHeadless: bind is %s:%s. Open the UI through an SSH tunnel:\n' "$HOST" "$PORT"
  printf '  ssh -N -L %s:127.0.0.1:%s <this-host>\n' "$PORT" "$PORT"
  printf '  then run `agi-mizar-web url` locally and paste the result.\n'
fi
