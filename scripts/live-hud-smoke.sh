#!/usr/bin/env bash
# scripts/live-hud-smoke.sh - automated Swift HUD live regression check.
#
# Starts the real Rust core and real Swift HUD app, lets the Swift app submit one
# typed turn through HUDWindow.onTextSubmit, then asserts the client/HUD log
# breadcrumbs that prove the UI path worked:
#   HUD showOperatorInput -> beginResponseStreaming -> responseComplete
# plus the DexterClient gRPC session and typed-input enqueue path.
#
# Usage:
#   scripts/live-hud-smoke.sh --start-core

set -u

SOCKET="/tmp/dexter.sock"
CORE_LOG="/tmp/dexter-hud-core-smoke.log"
SWIFT_LOG="/tmp/dexter-hud-swift-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_PID=""
SWIFT_PID=""
SMOKE_TEXT="${DEXTER_HUD_SMOKE_TEXT:-what is 2 plus 2}"

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
}
trap cleanup EXIT INT TERM

require_bins() {
    if [[ ! -x "$CORE_BIN" ]]; then
        say "$FAIL" "missing core binary: $CORE_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-core"
        exit 2
    fi
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
    say "$INFO" "starting Swift HUD smoke; log: $SWIFT_LOG"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_TEXT="$SMOKE_TEXT" \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS="${DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS:-3}" \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS="${DEXTER_HUD_SMOKE_EXIT_AFTER_SECS:-18}" \
            swift run
    ) >> "$SWIFT_LOG" 2>&1 &
    SWIFT_PID="$!"
}

wait_for_pattern() {
    local pattern="$1"
    local timeout_secs="$2"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if grep -Fq "$pattern" "$SWIFT_LOG"; then
            return 0
        fi
        if [[ -n "$SWIFT_PID" ]] && ! kill -0 "$SWIFT_PID" >/dev/null 2>&1; then
            # The process may exit normally after the smoke timeout. Keep checking
            # the final log once more before treating the pattern as missing.
            grep -Fq "$pattern" "$SWIFT_LOG" && return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

count_pattern() {
    local pattern="$1"
    grep -F "$pattern" "$SWIFT_LOG" 2>/dev/null | wc -l | tr -d ' '
}

wait_for_count() {
    local pattern="$1"
    local expected="$2"
    local timeout_secs="$3"
    local waited=0
    local count=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        count=$(count_pattern "$pattern")
        if [[ "$count" -ge "$expected" ]]; then
            return 0
        fi
        if [[ -n "$SWIFT_PID" ]] && ! kill -0 "$SWIFT_PID" >/dev/null 2>&1; then
            count=$(count_pattern "$pattern")
            [[ "$count" -ge "$expected" ]] && return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

assert_log_contains() {
    local label="$1"
    local pattern="$2"
    if ! wait_for_pattern "$pattern" 1; then
        say "$FAIL" "$label - missing Swift log pattern: $pattern"
        return 1
    fi
    return 0
}

assert_log_count_at_least() {
    local label="$1"
    local pattern="$2"
    local expected="$3"
    local count
    count=$(count_pattern "$pattern")
    if [[ "$count" -lt "$expected" ]]; then
        say "$FAIL" "$label - expected >= $expected Swift log occurrences of '$pattern', saw $count"
        return 1
    fi
    return 0
}

assert_log_absent() {
    local label="$1"
    local pattern="$2"
    if grep -Fq "$pattern" "$SWIFT_LOG"; then
        say "$FAIL" "$label - unexpected Swift log pattern: $pattern"
        return 1
    fi
    return 0
}

assert_no_mid_turn_idle_or_hide() {
    local label="$1"
    python3 - "$SWIFT_LOG" <<'PY'
import sys

log_path = sys.argv[1]
in_typed_turn = False
with open(log_path, "r", encoding="utf-8", errors="replace") as f:
    for line in f:
        if "[HUDSmoke] showOperatorInput" in line:
            in_typed_turn = True
            continue
        if in_typed_turn and "[HUDSmoke] responseComplete" in line:
            sys.exit(0)
        if in_typed_turn and (
            "[DexterClient] onResponse ← entityState: idle" in line
            or "[HUDSmoke] hide" in line
        ):
            print(line.rstrip())
            sys.exit(1)

sys.exit(2)
PY
    local rc=$?
    if [[ "$rc" -eq 1 ]]; then
        say "$FAIL" "$label - HUD idled or hid before the typed response completed"
        return 1
    fi
    if [[ "$rc" -eq 2 ]]; then
        say "$FAIL" "$label - could not locate typed HUD turn boundaries"
        return 1
    fi
    return 0
}

run_assertions() {
    local ok=0
    local name="Swift HUD smoke"
    local completion_baseline=0
    local expected_completion_count=0

    wait_for_pattern "[HUDSmoke] autoSubmit" 60 || {
        say "$FAIL" "$name - smoke hook did not auto-submit within 60s"
        tail -100 "$SWIFT_LOG" || true
        return 1
    }
    completion_baseline=$(count_pattern "[HUDSmoke] responseComplete")
    expected_completion_count=$((completion_baseline + 1))

    wait_for_pattern "[App] onTextSubmit fired: '$SMOKE_TEXT'" 10 || {
        say "$FAIL" "$name - Swift app did not receive the auto-submitted turn within 10s"
        tail -100 "$SWIFT_LOG" || true
        return 1
    }

    wait_for_count "[HUDSmoke] responseComplete" "$expected_completion_count" 60 || {
        say "$FAIL" "$name - HUD did not complete the typed response within 60s"
        tail -100 "$SWIFT_LOG" || true
        return 1
    }

    assert_log_contains "$name" "[HUDSmoke] enabled" || ok=1
    assert_log_contains "$name" "[DexterClient] Ping OK" || ok=1
    assert_log_contains "$name" "[HUDSmoke] autoSubmit" || ok=1
    assert_log_contains "$name" "[App] onTextSubmit fired: '$SMOKE_TEXT'" || ok=1
    assert_log_contains "$name" "[HUDSmoke] showOperatorInput" || ok=1
    assert_log_contains "$name" "[DexterClient] sendTypedInput enqueued to stream" || ok=1
    assert_log_contains "$name" "[DexterClient] onResponse ← entityState: thinking" || ok=1
    assert_log_contains "$name" "[HUDSmoke] beginResponseStreaming" || ok=1
    assert_log_contains "$name" "[DexterClient] onResponse ← textResponse: isFinal=true" || ok=1
    assert_log_contains "$name" "[HUDSmoke] responseComplete" || ok=1
    assert_log_contains "$name" "[DexterClient] onResponse ← entityState: idle" || ok=1
    assert_log_count_at_least "$name" "[HUDSmoke] responseComplete" "$expected_completion_count" || ok=1
    assert_no_mid_turn_idle_or_hide "$name" || ok=1

    assert_log_absent "$name" "sendTypedInput DROPPED" || ok=1
    assert_log_absent "$name" "Fatal error" || ok=1

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name passed"
        return 0
    fi

    tail -120 "$SWIFT_LOG" || true
    return 1
}

main() {
    require_bins
    start_core_if_requested
    start_swift_smoke
    run_assertions
}

main "$@"
