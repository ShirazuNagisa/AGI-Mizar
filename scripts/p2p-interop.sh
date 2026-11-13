#!/usr/bin/env bash
# Cross-language direct-path test: Rust acceptor + Swift probe, over loopback.
#
# The two engines claim to implement the same protocol; the only way to know is
# to make them talk. The probe is the Swift half of MizarKit's public API, the
# example is the Rust engine's, and this script starts both, hands over the
# endpoint id and port, and checks the message crossed both ways.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="$(cd "$ROOT/.." && pwd)"
SECRET="${1:-1111111111111111111111111111111111111111111111111111111111111111}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"; [[ -n "${ACCEPT_PID:-}" ]] && kill "$ACCEPT_PID" 2>/dev/null || true' EXIT

echo "building the Rust acceptor…"
cargo build -q -p agi-mizar --example p2p-accept --manifest-path "$ROOT/Cargo.toml"

echo "building the Swift probe…"
swift build --package-path "$REPO/iOS/MizarKit" --product mizar-net-probe >/dev/null
PROBE="$(swift build --package-path "$REPO/iOS/MizarKit" --show-bin-path)/mizar-net-probe"
[[ -x "$PROBE" ]] || { echo "the probe did not build at $PROBE" >&2; exit 1; }
ACCEPT_BIN="$(cargo metadata --format-version 1 --manifest-path "$ROOT/Cargo.toml" --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/examples/p2p-accept"

"$ACCEPT_BIN" "$SECRET" >"$WORK/accept.log" 2>&1 &
ACCEPT_PID=$!

for _ in $(seq 1 100); do
  grep -q '^PORT ' "$WORK/accept.log" 2>/dev/null && break
  kill -0 "$ACCEPT_PID" 2>/dev/null || { cat "$WORK/accept.log" >&2; exit 1; }
  sleep 0.1
done
ID="$(awk '/^ID /{print $2; exit}' "$WORK/accept.log")"
PORT="$(awk '/^PORT /{print $2; exit}' "$WORK/accept.log")"
[[ -n "$ID" && -n "$PORT" ]] || { cat "$WORK/accept.log" >&2; echo "the acceptor did not report id/port" >&2; exit 1; }

"$PROBE" dial --secret "$SECRET" --peer "$ID" --address "127.0.0.1:$PORT" >"$WORK/dial.log" 2>&1
wait "$ACCEPT_PID"
unset ACCEPT_PID

grep -q '^RECEIVED ping$' "$WORK/accept.log" || { cat "$WORK/accept.log" >&2; exit 1; }
grep -q '^REPLY echo:ping$' "$WORK/dial.log" || { cat "$WORK/dial.log" >&2; exit 1; }
echo "interop ok: Rust acceptor ${ID:0:10}… on 127.0.0.1:$PORT ↔ Swift probe"
