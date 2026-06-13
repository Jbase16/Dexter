#!/usr/bin/env bash
# scripts/live-stop-report-smoke.sh - verify make stop prints useful process evidence.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CORE_LOG="/tmp/dexter-stop-report-core.log"
STOP_LOG="/tmp/dexter-stop-report-stop.log"
CORE_PID=""

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

fail() {
    say "$FAIL" "$1" >&2
    if [[ -f "$CORE_LOG" ]]; then
        tail -80 "$CORE_LOG" >&2 || true
    fi
    if [[ -f "$STOP_LOG" ]]; then
        tail -80 "$STOP_LOG" >&2 || true
    fi
    exit 1
}

socket_accepts() {
    python3 - "$SOCKET" <<'PY' >/dev/null 2>&1
import socket
import sys

path = sys.argv[1]
s = socket.socket(socket.AF_UNIX)
s.settimeout(1)
sys.exit(0 if s.connect_ex(path) == 0 else 1)
PY
}

cleanup() {
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
    bash "$ROOT_DIR/scripts/stop-dexter.sh" --quiet >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

[[ -x "$CORE_BIN" ]] || fail "missing core binary: $CORE_BIN"

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections at $SOCKET"
fi

: > "$CORE_LOG"
say "$INFO" "starting release core; log: $CORE_LOG"
RUST_LOG=info "$CORE_BIN" >> "$CORE_LOG" 2>&1 &
CORE_PID="$!"

for _ in {1..90}; do
    socket_accepts && break
    if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
        fail "core exited before opening $SOCKET"
    fi
    sleep 1
done

socket_accepts || fail "core did not open $SOCKET"
say "$PASS" "core opened daemon socket"

: > "$STOP_LOG"
if ! bash "$ROOT_DIR/scripts/stop-dexter.sh" > "$STOP_LOG" 2>&1; then
    fail "stop-dexter.sh failed"
fi
CORE_PID=""

grep -Fq "==> Stopping Dexter process(es):" "$STOP_LOG" \
    || fail "stop output did not include process summary"
grep -Fq "dexter-core" "$STOP_LOG" \
    || fail "stop output did not identify dexter-core"
grep -Fq "(cwd:" "$STOP_LOG" \
    || fail "stop output did not include process cwd"
grep -Fq "==> Dexter stopped" "$STOP_LOG" \
    || fail "stop output did not include completion line"

if socket_accepts; then
    fail "daemon socket still accepts after stop"
fi

[[ ! -e "$SOCKET" && ! -e "$SHELL_SOCKET" ]] \
    || fail "socket files remain after stop"

say "$PASS" "stop report smoke passed"
