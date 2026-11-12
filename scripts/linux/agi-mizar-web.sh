#!/usr/bin/env bash
#
# Control the per-user AGI Mizar Web UI.
#
# This is the command the desktop entry runs and the one an operator uses on a
# headless box. It reads the same environment file the systemd unit does, so the
# port and the API token never have to be typed twice.
#
# Usage: agi-mizar-web [open|url|start|stop|restart|status|logs]
set -euo pipefail

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/agi-mizar"
ENV_FILE="${AGI_MIZAR_ENV_FILE:-$CONFIG_DIR/web.env}"
UNIT="agi-mizar-web.service"

die() {
  printf 'agi-mizar-web: %s\n' "$1" >&2
  exit 1
}

ensure_env() {
  [ -r "$ENV_FILE" ] && return 0
  [ -n "${HOME:-}" ] || die "HOME is not set"
  mkdir -p "$CONFIG_DIR"
  chmod 700 "$CONFIG_DIR" 2>/dev/null || true
  local token
  token="$(openssl rand -hex 32 2>/dev/null || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  umask 077
  cat > "$ENV_FILE" <<EOF
AGI_API_HOST=127.0.0.1
AGI_API_PORT=${AGI_MIZAR_WEB_PORT:-64222}
AGI_API_TOKEN=$token
MIZAR_BIND=127.0.0.1
RUST_LOG=agi_core=info,agi_engine_dsh=info,agi_mizar=info
EOF
  chmod 600 "$ENV_FILE"
}

read_env() {
  ensure_env
  # shellcheck disable=SC1090
  . "$ENV_FILE"
  PORT="${AGI_API_PORT:-64222}"
  TOKEN="${AGI_API_TOKEN:-}"
  [ -n "$TOKEN" ] || die "$ENV_FILE has no AGI_API_TOKEN"
}

health_url() {
  printf 'http://127.0.0.1:%s/healthz' "$PORT"
}

launch_url() {
  printf 'http://127.0.0.1:%s/launch?token=%s' "$PORT" "$TOKEN"
}

wait_for_kernel() {
  local tries="${1:-40}"
  local i=0
  while [ "$i" -lt "$tries" ]; do
    if curl -fsS -m 2 "$(health_url)" >/dev/null 2>&1; then
      return 0
    fi
    i=$((i + 1))
    sleep 0.25
  done
  return 1
}

open_browser() {
  local url="$1"
  if command -v xdg-open >/dev/null 2>&1; then
    xdg-open "$url" >/dev/null 2>&1 &
  elif command -v gio >/dev/null 2>&1; then
    gio open "$url" >/dev/null 2>&1 &
  elif command -v open >/dev/null 2>&1; then
    open "$url" >/dev/null 2>&1 &
  else
    printf '%s\n' "$url"
    return 0
  fi
  printf 'AGI Mizar Web UI: %s\n' "http://127.0.0.1:$PORT/"
}

case "${1:-open}" in
  open)
    read_env
    if ! curl -fsS -m 2 "$(health_url)" >/dev/null 2>&1; then
      systemctl --user daemon-reload >/dev/null 2>&1 || true
      systemctl --user enable --now "$UNIT" >/dev/null 2>&1 || true
      wait_for_kernel || die "the kernel did not answer on port $PORT; see: journalctl --user -u $UNIT"
    fi
    open_browser "$(launch_url)"
    ;;
  url)
    read_env
    launch_url
    printf '\n'
    ;;
  start)
    ensure_env
    systemctl --user daemon-reload >/dev/null 2>&1 || true
    systemctl --user enable --now "$UNIT"
    ;;
  stop)
    systemctl --user stop "$UNIT"
    ;;
  restart)
    systemctl --user restart "$UNIT"
    ;;
  status)
    read_env
    systemctl --user status "$UNIT" --no-pager || true
    if curl -fsS -m 2 "$(health_url)" >/dev/null 2>&1; then
      printf 'kernel: up on 127.0.0.1:%s\n' "$PORT"
    else
      printf 'kernel: no answer on 127.0.0.1:%s\n' "$PORT"
    fi
    ;;
  logs)
    journalctl --user -u "$UNIT" -n "${2:-80}" --no-pager
    ;;
  *)
    printf 'usage: agi-mizar-web [open|url|start|stop|restart|status|logs]\n' >&2
    exit 2
    ;;
esac
