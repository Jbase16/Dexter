#!/usr/bin/env bash
# scripts/live-placement-command-smoke.sh - verify external placement notifications reach Swift.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKET="/tmp/dexter.sock"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_LOG="/tmp/dexter-placement-command-core.log"
SWIFT_LOG="/tmp/dexter-placement-command-swift.log"
CORE_PID=""
SWIFT_PID=""

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

fail() {
    say "$FAIL" "$1" >&2
    tail -120 "$SWIFT_LOG" >&2 2>/dev/null || true
    tail -80 "$CORE_LOG" >&2 2>/dev/null || true
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
    if [[ -n "$SWIFT_PID" ]]; then
        kill "$SWIFT_PID" >/dev/null 2>&1 || true
        wait "$SWIFT_PID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
    bash "$ROOT_DIR/scripts/stop-dexter.sh" --quiet >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

wait_for_log() {
    local log="$1"
    local pattern="$2"
    local timeout_secs="$3"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        grep -Fq "$pattern" "$log" && return 0
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

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
    sleep 1
done
socket_accepts || fail "core did not open $SOCKET"

wait_for_log "$CORE_LOG" "Daemon startup warmup complete" "${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}" \
    || fail "core warmup did not complete"

: > "$SWIFT_LOG"
say "$INFO" "starting Swift HUD idle smoke; log: $SWIFT_LOG"
(
    cd "$SWIFT_DIR" || exit 2
    DEXTER_HUD_SMOKE=1 \
    DEXTER_HUD_SMOKE_IDLE_ONLY=1 \
    DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS=2 \
    DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=30 \
        swift run
) >> "$SWIFT_LOG" 2>&1 &
SWIFT_PID="$!"

wait_for_log "$SWIFT_LOG" "[HUDSmoke] idleOnly" 90 \
    || fail "Swift app did not reach idle smoke state"

for command in snap start stop; do
    bash "$ROOT_DIR/scripts/dexter-place.sh" "$command"
    wait_for_log "$SWIFT_LOG" "[HUDSmoke] placement command=$command" 10 \
        || fail "Swift app did not receive placement command: $command"
    wait_for_log "$SWIFT_LOG" "[HUDSmoke] placement after-command-$command" 10 \
        || fail "Swift app did not log placement snapshot for command: $command"
done

grep -Fq "topCenterHit=false" "$SWIFT_LOG" \
    || fail "external placement path did not preserve top-center click-through evidence"
grep -Fq "centerHit=true" "$SWIFT_LOG" \
    || fail "external placement path did not preserve center hit evidence"

say "$PASS" "external placement command smoke passed"
