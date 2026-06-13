#!/usr/bin/env bash
# scripts/live-hud-action-history-smoke.sh - automated Swift HUD action history smoke.
#
# Starts the real Rust core, creates a real audit entry through dexter-cli, then
# asks the Swift HUD to fetch Recent Actions through the ActionHistory unary RPC.

set -u

SOCKET="/tmp/dexter.sock"
CORE_LOG="/tmp/dexter-hud-action-history-core-smoke.log"
SWIFT_LOG="/tmp/dexter-hud-action-history-swift-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT_DIR/scripts/lib/process-tree.sh"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_PID=""
SWIFT_PID=""
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
    if [[ -n "$CORE_PID" ]]; then
        stop_process_tree "$CORE_PID"
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
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
        if ! socket_accepts; then
            say "$FAIL" "no Dexter daemon accepting connections at $SOCKET"
            exit 2
        fi
        return
    fi

    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        say "$INFO" "stop it first, or run this script without --start-core against that daemon"
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

create_audit_entry() {
    local token="$1"
    local action_json
    action_json='{"type":"shell","args":["echo",'$(json_string "$token" )'],"rationale":"hud action history smoke"}'
    "$CLI_BIN" --quiet --idle-timeout 180 --action-json "$action_json" >/tmp/dexter-hud-action-history-cli-smoke.log 2>&1
}

start_swift_smoke() {
    : > "$SWIFT_LOG"
    say "$INFO" "starting Swift HUD action history smoke; log: $SWIFT_LOG"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_ACTION_HISTORY=1 \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS="${DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS:-1}" \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS="${DEXTER_HUD_SMOKE_EXIT_AFTER_SECS:-10}" \
            swift run
    ) >> "$SWIFT_LOG" 2>&1 &
    SWIFT_PID="$!"
}

main() {
    require_bins
    start_core_if_requested

    local stamp token ok
    stamp="$(date +%s)-$$"
    token="HUD_ACTION_HISTORY_$stamp"
    ok=0

    if ! create_audit_entry "$token"; then
        say "$FAIL" "Swift HUD action history smoke - failed to create audit entry"
        cat /tmp/dexter-hud-action-history-cli-smoke.log || true
        exit 1
    fi

    start_swift_smoke

    wait_for_pattern "$SWIFT_LOG" "[DexterClient] ActionHistory RPC OK" 60 || {
        say "$FAIL" "Swift HUD action history smoke - ActionHistory RPC did not complete"
        tail -140 "$SWIFT_LOG" || true
        tail -120 "$CORE_LOG" || true
        exit 1
    }

    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "[HUDSmoke] enabled" || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "[HUDSmoke] actionHistoryRequest" || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "[DexterClient] ActionHistory RPC OK" || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "$token" || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "Latest Action Summary" || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "The latest audited action executed successfully." || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "Evidence: Succeeded:" || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "Recent Receipts" || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "[HUDSmoke] showActionHistory" || ok=1
    assert_contains "Swift HUD action history smoke" "$SWIFT_LOG" "[HUDSmoke] showUtilityMarkdown" || ok=1
    assert_contains "Swift HUD action history smoke" "$CORE_LOG" "Action history requested" || ok=1

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "Swift HUD action history smoke passed"
    else
        tail -140 "$SWIFT_LOG" || true
        tail -120 "$CORE_LOG" || true
        exit 1
    fi
}

main "$@"
