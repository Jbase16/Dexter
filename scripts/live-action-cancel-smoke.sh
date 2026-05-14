#!/usr/bin/env bash
# scripts/live-action-cancel-smoke.sh - live regression for action subprocess cancel.
#
# Starts the real Rust core, asks dexter-cli to run a long-lived safe shell
# action (`tail -f`), then has dexter-cli fire HotkeyActivated after the action
# reaches FOCUSED in the same gRPC session. Assertions prove the hotkey cancel
# path drains the in-flight action handle and kills the OS child.
#
# Usage:
#   scripts/live-action-cancel-smoke.sh --start-core

set -u

SOCKET="/tmp/dexter.sock"
LOG="/tmp/dexter-action-cancel-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
CORE_PID=""
TOKEN="dexter-action-cancel-$$_$(date +%s)"
TARGET_FILE="/tmp/${TOKEN}.log"
PROMPT="Use a Dexter shell action to run exactly this command: tail -f $TARGET_FILE"

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

if [[ "${1:-}" == "--start-core" ]]; then
    START_CORE=1
    shift
fi

if [[ "${1:-}" != "" ]]; then
    LOG="$1"
fi

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
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
    rm -f "$TARGET_FILE"
}
trap cleanup EXIT INT TERM

require_bins() {
    if [[ ! -x "$CORE_BIN" ]]; then
        say "$FAIL" "missing core binary: $CORE_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-core --bin dexter-cli"
        exit 2
    fi
    if [[ ! -x "$CLI_BIN" ]]; then
        say "$FAIL" "missing CLI binary: $CLI_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-cli"
        exit 2
    fi
}

start_core_if_requested() {
    if [[ "$START_CORE" -ne 1 ]]; then
        if [[ ! -f "$LOG" ]]; then
            say "$FAIL" "log not found: $LOG"
            say "$INFO" "start Dexter with: ./src/rust-core/target/release/dexter-core 2>&1 | tee $LOG"
            exit 2
        fi
        if ! socket_accepts; then
            say "$FAIL" "no Dexter daemon accepting connections at $SOCKET"
            exit 2
        fi
        return
    fi

    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        say "$INFO" "stop it first, or run this script without --start-core against its log"
        exit 2
    fi

    : > "$LOG"
    say "$INFO" "starting release core; log: $LOG"
    RUST_LOG=info "$CORE_BIN" >> "$LOG" 2>&1 &
    CORE_PID="$!"

    local waited=0
    while [[ "$waited" -lt 90 ]]; do
        if socket_accepts; then
            break
        fi
        sleep 1
        waited=$((waited + 1))
    done
    if ! socket_accepts; then
        say "$FAIL" "core did not open $SOCKET within 90s"
        tail -40 "$LOG" || true
        exit 2
    fi

    waited=0
    while [[ "$waited" -lt 120 ]]; do
        if grep -Fq "Daemon startup warmup complete" "$LOG"; then
            say "$INFO" "core warmup complete"
            return
        fi
        sleep 1
        waited=$((waited + 1))
    done
    say "$FAIL" "core socket opened, but warmup did not complete within 120s"
    tail -80 "$LOG" || true
    exit 2
}

log_bytes() {
    stat -f%z "$LOG" 2>/dev/null || echo 0
}

log_since() {
    local offset="$1"
    tail -c "+$((offset + 1))" "$LOG" 2>/dev/null || true
}

count_since() {
    local offset="$1"
    local pattern="$2"
    log_since "$offset" | grep -F -c -- "$pattern" 2>/dev/null || true
}

assert_count_at_least() {
    local label="$1"
    local offset="$2"
    local pattern="$3"
    local expected="$4"
    local actual
    actual="$(count_since "$offset" "$pattern")"
    if [[ "$actual" -lt "$expected" ]]; then
        say "$FAIL" "$label - expected >= $expected occurrences of '$pattern', saw $actual"
        return 1
    fi
    return 0
}

assert_absent() {
    local label="$1"
    local offset="$2"
    local pattern="$3"
    local actual
    actual="$(count_since "$offset" "$pattern")"
    if [[ "$actual" -ne 0 ]]; then
        say "$FAIL" "$label - unexpected '$pattern' occurrences: $actual"
        return 1
    fi
    return 0
}

tail_process_running() {
    python3 - "$TARGET_FILE" <<'PY'
import subprocess
import sys

target = sys.argv[1]
needle = f"tail -f {target}"
out = subprocess.check_output(["ps", "-axo", "pid=,command="], text=True)
for line in out.splitlines():
    if needle in line:
        print(line.strip())
        sys.exit(0)
sys.exit(1)
PY
}

main() {
    require_bins
    printf 'tail target for Dexter action-cancel smoke\n' > "$TARGET_FILE"
    start_core_if_requested

    local name="action subprocess hotkey cancel"
    local offset out ok
    "$CLI_BIN" --quiet --idle-timeout 30 "say ready" >/dev/null 2>&1 || true
    sleep 1

    offset="$(log_bytes)"
    out="$(mktemp -t dexter-action-cancel.XXXXXX)"
    ok=0

    if ! "$CLI_BIN" \
        --idle-timeout 45 \
        --interrupt-on-focused-after-ms 500 \
        "$PROMPT" > "$out" 2>&1; then
        say "$FAIL" "$name - dexter-cli failed"
        cat "$out"
        rm -f "$out"
        exit 1
    fi

    sleep 1

    assert_count_at_least "$name" "$offset" "Action dispatched to background task" 1 || ok=1
    assert_count_at_least "$name" "$offset" "Global hotkey activated" 1 || ok=1
    assert_count_at_least "$name" "$offset" "Hotkey aborting in-flight generation" 1 || ok=1
    assert_absent "$name" "$offset" "Action result surfaced to operator" || ok=1
    assert_absent "$name" "$offset" "Background action result — spawning continuation generation" || ok=1

    if ! grep -Fq "[INTERRUPT armed after focused:" "$out"; then
        say "$FAIL" "$name - CLI did not arm focused interrupt"
        ok=1
    fi
    if ! grep -Fq "[INTERRUPTED]" "$out"; then
        say "$FAIL" "$name - CLI did not observe LISTENING after interrupt"
        ok=1
    fi

    if tail_process_running; then
        say "$FAIL" "$name - tail subprocess is still running after hotkey cancel"
        ok=1
    fi

    rm -f "$out"

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name passed"
        exit 0
    fi

    tail -120 "$LOG" || true
    exit 1
}

main "$@"
