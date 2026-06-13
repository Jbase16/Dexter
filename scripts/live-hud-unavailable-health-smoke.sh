#!/usr/bin/env bash
# scripts/live-hud-unavailable-health-smoke.sh - verify HUD health is actionable when core is down.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
SWIFT_DIR="$ROOT_DIR/src/swift"
SWIFT_LOG="/tmp/dexter-hud-unavailable-health-swift.log"
SWIFT_PID=""

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
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
    bash "$ROOT_DIR/scripts/stop-dexter.sh" --quiet >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

fail() {
    say "$FAIL" "$1" >&2
    tail -160 "$SWIFT_LOG" >&2 2>/dev/null || true
    exit 1
}

wait_for_pattern() {
    local pattern="$1"
    local timeout_secs="$2"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        grep -Fq "$pattern" "$SWIFT_LOG" && return 0
        if [[ -n "$SWIFT_PID" ]] && ! kill -0 "$SWIFT_PID" >/dev/null 2>&1; then
            grep -Fq "$pattern" "$SWIFT_LOG" && return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

assert_contains() {
    local pattern="$1"
    grep -Fq "$pattern" "$SWIFT_LOG" || fail "missing Swift log pattern: $pattern"
}

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections at $SOCKET"
fi
rm -f "$SOCKET" "$SHELL_SOCKET"

: > "$SWIFT_LOG"
say "$INFO" "starting Swift HUD unavailable-health smoke; log: $SWIFT_LOG"
(
    cd "$SWIFT_DIR" || exit 2
    DEXTER_HUD_SMOKE=1 \
    DEXTER_HUD_SMOKE_HEALTH=1 \
    DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS=1 \
    DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=8 \
        swift run
) >> "$SWIFT_LOG" 2>&1 &
SWIFT_PID="$!"

wait_for_pattern "[HUDSmoke] showUtilityMarkdown" 45 \
    || fail "HUD did not render unavailable health markdown"

assert_contains "Status: unavailable"
assert_contains "Could not reach the Rust core at /tmp/dexter.sock."
assert_contains "Recovery: choose Restart Dexter from the HUD or Dexter menu."
assert_contains 'Terminal fallback: run `cd /Users/jason/Developer/Dex && make open-app` or `cd /Users/jason/Developer/Dex && make run`.'
assert_contains "[HUDSmoke] markdownPreview"

wait "$SWIFT_PID" >/dev/null 2>&1 || true
SWIFT_PID=""

if socket_accepts; then
    fail "daemon socket unexpectedly accepts after unavailable-health smoke"
fi
[[ ! -e "$SOCKET" && ! -e "$SHELL_SOCKET" ]] \
    || fail "socket files remain after unavailable-health smoke"

say "$PASS" "HUD unavailable-health smoke passed"
