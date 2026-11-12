#!/usr/bin/env bash
#
# Smoke-test a Linux release daemon without installing anything.
#
# Starts the binary with a temporary HOME on a free loopback port, checks the
# launch-cookie flow and the UI page, then stops it. No systemd, no relay, no
# writes outside a temp directory.
#
# Usage:
#   scripts/linux/smoke-test.sh [--binary PATH]
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BINARY="${AGI_MIZAR_BINARY:-$REPO_ROOT/target/release/agi-daemon}"
if [ "${1:-}" = "--binary" ]; then
  BINARY="${2:?--binary needs a path}"
  shift 2
fi
if [ ! -x "$BINARY" ]; then
  BINARY="$REPO_ROOT/bin/agi-daemon"
fi
[ -x "$BINARY" ] || { echo "smoke-test: no executable at $BINARY" >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo "smoke-test: curl is required" >&2; exit 1; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/agi-mizar-smoke.XXXXXX")"
PORT="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"
TOKEN="$(openssl rand -hex 32 2>/dev/null || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
LOG="$TMP/daemon.log"
COOKIES="$TMP/cookies"
PID=""
cleanup() {
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  [ -n "$PID" ] && wait "$PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

mkdir -p "$TMP/home"
env HOME="$TMP/home" DSH_HOME="$TMP/dsh" MIZAR_BIND=127.0.0.1 \
  AGI_API_HOST=127.0.0.1 AGI_API_PORT="$PORT" AGI_API_TOKEN="$TOKEN" \
  AGI_RELAY_URL=http://127.0.0.1:1 RUST_LOG=error \
  "$BINARY" "$TMP/endpoint.json" >"$LOG" 2>&1 &
PID=$!

wait_up() {
  local i=0
  while [ "$i" -lt 60 ]; do
    if curl -fsS -m 1 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
      return 0
    fi
    i=$((i + 1))
    sleep 0.25
  done
  return 1
}
wait_up || { echo "smoke-test: daemon did not start" >&2; sed -E 's/(token[ =:]+)[A-Za-z0-9_-]{16,}/\1REDACTED/g' "$LOG" >&2; exit 1; }

fail() { echo "smoke-test: FAIL: $1" >&2; exit 1; }

[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/api/v1/state")" = "401" ] \
  || fail "state without auth was not 401"
curl -fsS "http://127.0.0.1:$PORT/" | grep -q '<title>AGI Mizar</title>' \
  || fail "UI page has no AGI Mizar title"

curl -sS -D "$TMP/launch.headers" -o /dev/null -c "$COOKIES" \
  "http://127.0.0.1:$PORT/launch?token=$TOKEN" >/dev/null
grep -qi '^HTTP/1.1 303' "$TMP/launch.headers" || fail "launch was not 303"
grep -qi 'set-cookie:.*HttpOnly' "$TMP/launch.headers" || fail "cookie is not HttpOnly"
grep -qi 'set-cookie:.*SameSite=Strict' "$TMP/launch.headers" || fail "cookie is not SameSite=Strict"
[ "$(curl -s -o /dev/null -w '%{http_code}' -b "$COOKIES" "http://127.0.0.1:$PORT/api/v1/state")" = "200" ] \
  || fail "cookie did not authorize state"

curl -sS -D "$TMP/evil.headers" -o /dev/null \
  "http://127.0.0.1:$PORT/launch?token=$TOKEN&next=//evil.example" >/dev/null
tr -d '\r' < "$TMP/evil.headers" | grep -qi '^location: /$' || fail "open redirect was not blocked"

[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST -b "$COOKIES" "http://127.0.0.1:$PORT/api/v1/logout")" = "204" ] \
  || fail "logout was not 204"

echo "smoke-test: PASS (port $PORT, $(wc -c < "$LOG") log bytes)"
