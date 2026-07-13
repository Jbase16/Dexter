#!/usr/bin/env bash
# scripts/live-detached-core-smoke.sh - verify detached core start/stop lifecycle.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
PID_FILE="/tmp/dexter-core.pid"
LOG="/tmp/dexter-core.log"

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

fail() {
    say "$FAIL" "$1" >&2
    if [[ -f "$LOG" ]]; then
        tail -120 "$LOG" >&2 || true
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
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections at $SOCKET"
fi

rm -f "$PID_FILE" "$LOG"
say "$INFO" "starting detached core through make start-core"
if ! make -C "$ROOT_DIR" start-core; then
    fail "make start-core failed"
fi

[[ -s "$PID_FILE" ]] || fail "detached core PID file was not written"
PID="$(tr -cd '0-9' < "$PID_FILE")"
[[ -n "$PID" ]] || fail "detached core PID file did not contain a PID"
kill -0 "$PID" >/dev/null 2>&1 || fail "detached core launcher PID is not alive"
socket_accepts || fail "detached core socket is not accepting after start-core"
say "$PASS" "detached core started and accepted connections"

if ! "$ROOT_DIR/src/rust-core/target/release/dexter-cli" --doctor >/tmp/dexter-detached-core-doctor.out 2>&1; then
    cat /tmp/dexter-detached-core-doctor.out >&2 || true
    fail "doctor failed after detached start"
fi
grep -Fq "Result: OK - no failed checks." /tmp/dexter-detached-core-doctor.out \
    || {
        cat /tmp/dexter-detached-core-doctor.out >&2 || true
        fail "doctor did not report clean health after detached start"
    }
say "$PASS" "doctor reports clean health after detached start"

if ! make -C "$ROOT_DIR" stop; then
    fail "make stop failed after detached start"
fi

if socket_accepts; then
    fail "daemon socket still accepts after make stop"
fi
[[ ! -e "$SOCKET" && ! -e "$SHELL_SOCKET" ]] \
    || fail "socket files remain after make stop"
[[ ! -e "$PID_FILE" ]] || fail "detached core PID file remains after make stop"

say "$PASS" "detached core stop removed sockets and PID file"
say "$PASS" "detached core smoke passed"
