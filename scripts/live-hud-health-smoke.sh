#!/usr/bin/env bash
# scripts/live-hud-health-smoke.sh - automated Swift HUD health/recovery smoke.
#
# Starts the real Rust core and real Swift HUD app, asks the HUD status surface
# for daemon Health plus recent ActionHistory snapshots, then restarts the
# browser worker through the HUD recovery closure. This verifies the
# operator-visible path without a manual click and without routing through the
# model/action pipeline.

set -u

SOCKET="/tmp/dexter.sock"
CORE_LOG="/tmp/dexter-hud-health-core-smoke.log"
SWIFT_LOG="/tmp/dexter-hud-health-swift-smoke.log"
SILENT_SWIFT_LOG="/tmp/dexter-hud-health-silent-swift-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT_DIR/scripts/lib/process-tree.sh"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_PID=""
SWIFT_PID=""
RESTART_COMPONENT="${DEXTER_HUD_HEALTH_SMOKE_RESTART_COMPONENT:-browser}"
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"

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

json_string() {
    python3 - "$1" <<'PY'
import json
import sys

print(json.dumps(sys.argv[1]))
PY
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
        stop_process_tree "$SWIFT_PID"
        wait "$SWIFT_PID" >/dev/null 2>&1 || true
    fi
    stop_core_if_owned >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

stop_core_if_owned() {
    if [[ -z "$CORE_PID" ]]; then
        return 0
    fi

    local pid="$CORE_PID"
    CORE_PID=""
    stop_process_tree "$pid"
    wait "$pid" >/dev/null 2>&1 || true
}

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
    while [[ "$waited" -lt "$CORE_WARMUP_TIMEOUT_SECS" ]]; do
        if grep -Fq "Daemon startup warmup complete" "$CORE_LOG"; then
            say "$INFO" "core warmup complete"
            return
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "core socket opened, but warmup did not complete within ${CORE_WARMUP_TIMEOUT_SECS}s"
    tail -80 "$CORE_LOG" || true
    exit 2
}

create_audit_entry() {
    local token="$1"
    local action_json
    action_json='{"type":"shell","args":["echo",'$(json_string "$token" )'],"rationale":"hud status smoke safe"}'
    "$CLI_BIN" --quiet --idle-timeout 180 --action-json "$action_json" >/tmp/dexter-hud-health-cli-smoke.log 2>&1
}

start_swift_smoke() {
    : > "$SWIFT_LOG"
    say "$INFO" "starting Swift HUD health smoke; log: $SWIFT_LOG"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_HEALTH=1 \
        DEXTER_HUD_SMOKE_RESTART_COMPONENT="$RESTART_COMPONENT" \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS="${DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS:-3}" \
        DEXTER_HUD_SMOKE_RESTART_DELAY_SECS="${DEXTER_HUD_SMOKE_RESTART_DELAY_SECS:-3}" \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS="${DEXTER_HUD_SMOKE_EXIT_AFTER_SECS:-14}" \
            swift run
    ) >> "$SWIFT_LOG" 2>&1 &
    SWIFT_PID="$!"
}

run_swift_silent_ready_smoke() {
    : > "$SILENT_SWIFT_LOG"
    say "$INFO" "starting Swift HUD silent-ready health smoke; log: $SILENT_SWIFT_LOG"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_IDLE_ONLY=1 \
        DEXTER_HUD_SMOKE_KEEP_CORE_ON_EXIT=1 \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS=1 \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=8 \
        DEXTER_PROACTIVE_HEALTH_INITIAL_DELAY_SECS=2 \
        DEXTER_PROACTIVE_HEALTH_RETRY_DELAY_SECS=2 \
            swift run
    ) >> "$SILENT_SWIFT_LOG" 2>&1 &
    SWIFT_PID="$!"

    wait_for_pattern "$SILENT_SWIFT_LOG" "[DexterClient] Proactive health probe silent status=ready" 30 || {
        say "$FAIL" "Swift HUD silent-ready health smoke - proactive ready probe did not stay silent"
        tail -140 "$SILENT_SWIFT_LOG" || true
        tail -120 "$CORE_LOG" || true
        return 1
    }

    wait "$SWIFT_PID" >/dev/null 2>&1 || true
    SWIFT_PID=""

    assert_contains "Swift HUD silent-ready health smoke" "$SILENT_SWIFT_LOG" "[HUDSmoke] idleOnly" || return 1
    assert_absent "Swift HUD silent-ready health smoke" "$SILENT_SWIFT_LOG" "[HUDSmoke] showUtilityMarkdown" || return 1
    say "$PASS" "Swift HUD silent-ready health smoke passed"
    return 0
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
        if [[ -n "$SWIFT_PID" ]] && ! kill -0 "$SWIFT_PID" >/dev/null 2>&1; then
            grep -Fq "$pattern" "$file" && return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

assert_contains() {
    local label="$1"
    local file="$2"
    local pattern="$3"
    if ! grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - missing pattern: $pattern"
        return 1
    fi
    return 0
}

assert_contains_any() {
    local label="$1"
    local file="$2"
    shift 2
    local pattern
    for pattern in "$@"; do
        if grep -Fq "$pattern" "$file"; then
            return 0
        fi
    done
    say "$FAIL" "$label - missing all expected patterns: $*"
    return 1
}

assert_absent() {
    local label="$1"
    local file="$2"
    local pattern="$3"
    if grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - unexpected pattern: $pattern"
        return 1
    fi
    return 0
}

run_assertions() {
    local ok=0
    local name="Swift HUD health smoke"

    wait_for_pattern "$SWIFT_LOG" "[DexterClient] Restart RPC OK target=$RESTART_COMPONENT success=true" 90 || {
        say "$FAIL" "$name - restart RPC did not complete within 90s"
        tail -140 "$SWIFT_LOG" || true
        tail -120 "$CORE_LOG" || true
        return 1
    }

    assert_contains "$name" "$SWIFT_LOG" "[HUDSmoke] enabled" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "[HUDSmoke] healthRequest" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "[DexterClient] Health RPC OK" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "[DexterClient] ActionHistory RPC OK" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "[DexterClient] AmbientHistory RPC OK" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "Residency:" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "Current Context" || ok=1
    assert_contains_any "$name" "$SWIFT_LOG" "Dexter can:" "No focused app context" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "Latest Action Summary" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "Recent Ambient Events" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "action_succeeded" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "The latest audited action executed successfully." || ok=1
    assert_contains "$name" "$SWIFT_LOG" "Evidence: Succeeded:" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "$HUD_STATUS_TOKEN" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "[HUDSmoke] showUtilityMarkdown" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "[HUDSmoke] restartRequest target=$RESTART_COMPONENT" || ok=1
    assert_contains "$name" "$SWIFT_LOG" "[DexterClient] Restart RPC OK target=$RESTART_COMPONENT success=true" || ok=1
    assert_contains "$name" "$CORE_LOG" "Health snapshot requested" || ok=1
    assert_contains "$name" "$CORE_LOG" "Action history requested" || ok=1
    assert_contains "$name" "$CORE_LOG" "Ambient history requested" || ok=1
    assert_contains "$name" "$CORE_LOG" "Component restart requested" || ok=1
    assert_contains "$name" "$CORE_LOG" "Component restart complete" || ok=1

    assert_absent "$name" "$SWIFT_LOG" "Fatal error" || ok=1
    assert_absent "$name" "$SWIFT_LOG" "sendTypedInput DROPPED" || ok=1

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name passed"
        return 0
    fi

    tail -140 "$SWIFT_LOG" || true
    tail -120 "$CORE_LOG" || true
    return 1
}

assert_owned_core_stops_cleanly() {
    if [[ "$START_CORE" -ne 1 ]]; then
        return 0
    fi

    stop_core_if_owned

    if socket_accepts; then
        say "$FAIL" "daemon still accepts connections after smoke cleanup"
        return 1
    fi

    if [[ -e "$SOCKET" || -e "/tmp/dexter-shell.sock" ]]; then
        say "$FAIL" "daemon left stale socket files after SIGTERM cleanup"
        ls -l "$SOCKET" /tmp/dexter-shell.sock 2>/dev/null || true
        return 1
    fi

    return 0
}

main() {
    require_bins
    start_core_if_requested
    run_swift_silent_ready_smoke || exit 1

    local stamp
    stamp="$(date +%s)-$$"
    HUD_STATUS_TOKEN="HUD_STATUS_$stamp"
    if ! create_audit_entry "$HUD_STATUS_TOKEN"; then
        say "$FAIL" "Swift HUD health smoke - failed to create status audit entry"
        cat /tmp/dexter-hud-health-cli-smoke.log || true
        exit 1
    fi

    start_swift_smoke
    run_assertions || exit 1
    assert_owned_core_stops_cleanly || exit 1
}

main "$@"
