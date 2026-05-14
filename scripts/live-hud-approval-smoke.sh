#!/usr/bin/env bash
# scripts/live-hud-approval-smoke.sh - automated Swift HUD approval regression.
#
# Starts the real Rust core and real Swift HUD app, submits a destructive typed
# turn through HUDWindow.onTextSubmit, then uses the Swift smoke-only approval
# policy to deny the ActionRequest. Assertions prove:
#   - Swift received the ActionRequest through the HUD session.
#   - Swift sent an ActionApproval denial.
#   - Rust rejected the action.
#   - The destructive command did not run.
#
# Usage:
#   scripts/live-hud-approval-smoke.sh --start-core

set -u

SOCKET="/tmp/dexter.sock"
CORE_LOG="/tmp/dexter-hud-approval-core-smoke.log"
SWIFT_LOG="/tmp/dexter-hud-approval-swift-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_PID=""
SWIFT_PID=""
TARGET_DIR="/tmp/dexter-hud-smoke-delete-me"
MARKER="$TARGET_DIR/proof.txt"
SMOKE_TEXT="${DEXTER_HUD_APPROVAL_SMOKE_TEXT:-Use a Dexter shell action to run exactly this command: rm -rf /tmp/dexter-hud-smoke-delete-me}"

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

if [[ "${1:-}" == "--start-core" ]]; then
    START_CORE=1
    shift
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
    if [[ -n "$SWIFT_PID" ]]; then
        kill "$SWIFT_PID" >/dev/null 2>&1 || true
        wait "$SWIFT_PID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
    rm -rf "$TARGET_DIR"
}
trap cleanup EXIT INT TERM

require_bins() {
    if [[ ! -x "$CORE_BIN" ]]; then
        say "$FAIL" "missing core binary: $CORE_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-core"
        exit 2
    fi
}

prepare_marker() {
    rm -rf "$TARGET_DIR"
    mkdir -p "$TARGET_DIR"
    printf 'do not delete\n' > "$MARKER"
}

start_core_if_requested() {
    if [[ "$START_CORE" -ne 1 ]]; then
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

    : > "$CORE_LOG"
    say "$INFO" "starting release core; log: $CORE_LOG"
    RUST_LOG=info "$CORE_BIN" >> "$CORE_LOG" 2>&1 &
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
        tail -40 "$CORE_LOG" || true
        exit 2
    fi

    waited=0
    while [[ "$waited" -lt 120 ]]; do
        if grep -Fq "Daemon startup warmup complete" "$CORE_LOG"; then
            say "$INFO" "core warmup complete"
            return
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "core socket opened, but warmup did not complete within 120s"
    tail -80 "$CORE_LOG" || true
    exit 2
}

start_swift_smoke() {
    : > "$SWIFT_LOG"
    say "$INFO" "starting Swift HUD approval smoke; log: $SWIFT_LOG"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_TEXT="$SMOKE_TEXT" \
        DEXTER_HUD_SMOKE_ACTION_APPROVAL=deny \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS="${DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS:-3}" \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS="${DEXTER_HUD_SMOKE_EXIT_AFTER_SECS:-24}" \
            swift run
    ) >> "$SWIFT_LOG" 2>&1 &
    SWIFT_PID="$!"
}

wait_for_pattern() {
    local file="$1"
    local pattern="$2"
    local timeout_secs="$3"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if grep -Fq "$pattern" "$file"; then
            return 0
        fi
        if [[ "$file" == "$SWIFT_LOG" && -n "$SWIFT_PID" ]] && ! kill -0 "$SWIFT_PID" >/dev/null 2>&1; then
            grep -Fq "$pattern" "$file" && return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

assert_log_contains() {
    local label="$1"
    local file="$2"
    local pattern="$3"
    if ! wait_for_pattern "$file" "$pattern" 1; then
        say "$FAIL" "$label - missing log pattern: $pattern"
        return 1
    fi
    return 0
}

assert_log_absent() {
    local label="$1"
    local file="$2"
    local pattern="$3"
    if grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - unexpected log pattern: $pattern"
        return 1
    fi
    return 0
}

run_assertions() {
    local ok=0
    local name="Swift HUD approval smoke"

    wait_for_pattern "$SWIFT_LOG" "[HUDSmoke] autoSubmit" 60 || {
        say "$FAIL" "$name - smoke hook did not auto-submit within 60s"
        tail -100 "$SWIFT_LOG" || true
        return 1
    }

    wait_for_pattern "$SWIFT_LOG" "[HUDSmoke] actionApproval" 90 || {
        say "$FAIL" "$name - Swift did not auto-deny an ActionRequest within 90s"
        tail -140 "$SWIFT_LOG" || true
        tail -100 "$CORE_LOG" || true
        return 1
    }

    wait_for_pattern "$CORE_LOG" "Action rejected by operator" 30 || {
        say "$FAIL" "$name - core did not reject the denied action"
        tail -120 "$CORE_LOG" || true
        return 1
    }

    assert_log_contains "$name" "$SWIFT_LOG" "[HUDSmoke] enabled" || ok=1
    assert_log_contains "$name" "$SWIFT_LOG" "[App] onTextSubmit fired: '$SMOKE_TEXT'" || ok=1
    assert_log_contains "$name" "$SWIFT_LOG" "[DexterClient] onResponse ← actionRequest:" || ok=1
    assert_log_contains "$name" "$SWIFT_LOG" "[HUDSmoke] actionRequest" || ok=1
    assert_log_contains "$name" "$SWIFT_LOG" "category=destructive" || ok=1
    assert_log_contains "$name" "$SWIFT_LOG" "$TARGET_DIR" || ok=1
    assert_log_contains "$name" "$SWIFT_LOG" "[HUDSmoke] actionApproval" || ok=1
    assert_log_contains "$name" "$SWIFT_LOG" "approved=false" || ok=1
    assert_log_contains "$name" "$SWIFT_LOG" "Action cancelled:" || ok=1

    assert_log_contains "$name" "$CORE_LOG" "Action requires operator approval" || ok=1
    assert_log_contains "$name" "$CORE_LOG" "DESTRUCTIVE action awaiting operator approval" || ok=1
    assert_log_contains "$name" "$CORE_LOG" "ActionApproval received" || ok=1
    assert_log_contains "$name" "$CORE_LOG" "Action rejected by operator" || ok=1
    assert_log_absent "$name" "$CORE_LOG" "Action dispatched to background task" || ok=1

    if [[ ! -f "$MARKER" ]]; then
        say "$FAIL" "$name - marker file was deleted despite auto-deny"
        ok=1
    fi

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name passed"
        return 0
    fi

    tail -140 "$SWIFT_LOG" || true
    tail -120 "$CORE_LOG" || true
    return 1
}

main() {
    require_bins
    prepare_marker
    start_core_if_requested
    start_swift_smoke
    run_assertions
}

main "$@"
