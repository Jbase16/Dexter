#!/usr/bin/env bash
# scripts/live-hud-action-diagnostic-smoke.sh - automated Swift HUD action diagnostic smoke.
#
# Starts the real Rust core, creates a blocked raw message_send receipt through
# dexter-cli, then asks the Swift HUD to explain the latest action from local
# health, action-history, and session evidence.

set -u

SOCKET="/tmp/dexter.sock"
CORE_LOG="/tmp/dexter-hud-action-diagnostic-core-smoke.log"
SWIFT_LOG="/tmp/dexter-hud-action-diagnostic-swift-smoke.log"
CLI_LOG="/tmp/dexter-hud-action-diagnostic-cli-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_PID=""
SWIFT_PID=""

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

run_cli_with_timeout() {
    local timeout_secs="$1"
    shift
    perl -e 'my $timeout = shift @ARGV; alarm $timeout; exec @ARGV;' \
        "$timeout_secs" "$CLI_BIN" "$@"
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

    local warmup_timeout="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"
    waited=0
    while [[ "$waited" -lt "$warmup_timeout" ]]; do
        if grep -Fq "Daemon startup warmup complete" "$CORE_LOG"; then
            say "$INFO" "core warmup complete"
            return
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "core socket opened, but warmup did not complete within ${warmup_timeout}s"
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

create_blocked_action_entry() {
    local token="$1"
    local action_json
    action_json='{"type":"message_send","recipient":'$(json_string "$token")',"body":"hud diagnostic smoke","rationale":"hud action diagnostic smoke"}'
    : > "$CLI_LOG"
    run_cli_with_timeout 45 --quiet --idle-timeout 20 --action-json "$action_json" >> "$CLI_LOG" 2>&1
}

start_swift_smoke() {
    : > "$SWIFT_LOG"
    say "$INFO" "starting Swift HUD action diagnostic smoke; log: $SWIFT_LOG"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_ACTION_DIAGNOSTIC=1 \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS="${DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS:-1}" \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS="${DEXTER_HUD_SMOKE_EXIT_AFTER_SECS:-10}" \
            swift run
    ) >> "$SWIFT_LOG" 2>&1 &
    SWIFT_PID="$!"
}

assert_socket_clean_after_owned_core_exit() {
    if [[ "$START_CORE" -ne 1 ]]; then
        return 0
    fi

    cleanup
    CORE_PID=""
    SWIFT_PID=""
    sleep 1
    if socket_accepts; then
        say "$FAIL" "Swift HUD action diagnostic smoke - socket still accepts connections after cleanup"
        lsof -nU 2>/dev/null | grep -F -- "$SOCKET" || true
        return 1
    fi
    return 0
}

main() {
    require_bins
    start_core_if_requested

    local stamp token ok
    stamp="$(date +%s)-$$"
    token="HUD_ACTION_DIAGNOSTIC_$stamp"
    ok=0

    if ! create_blocked_action_entry "$token"; then
        say "$FAIL" "Swift HUD action diagnostic smoke - failed to create blocked audit entry"
        cat "$CLI_LOG" || true
        exit 1
    fi

    start_swift_smoke

    wait_for_pattern "$SWIFT_LOG" "[DexterClient] ActionDiagnostic report generated" 60 || {
        say "$FAIL" "Swift HUD action diagnostic smoke - diagnostic report did not complete"
        tail -160 "$SWIFT_LOG" || true
        tail -140 "$CORE_LOG" || true
        cat "$CLI_LOG" || true
        exit 1
    }

    assert_contains "Swift HUD action diagnostic smoke" "$SWIFT_LOG" "[HUDSmoke] enabled" || ok=1
    assert_contains "Swift HUD action diagnostic smoke" "$SWIFT_LOG" "[HUDSmoke] actionDiagnosticRequest" || ok=1
    assert_contains "Swift HUD action diagnostic smoke" "$SWIFT_LOG" "raw message_send action was blocked" || ok=1
    assert_contains "Swift HUD action diagnostic smoke" "$SWIFT_LOG" "$token" || ok=1
    assert_contains "Swift HUD action diagnostic smoke" "$SWIFT_LOG" "[HUDSmoke] showActionDiagnostic" || ok=1
    assert_contains "Swift HUD action diagnostic smoke" "$SWIFT_LOG" "[HUDSmoke] showUtilityMarkdown" || ok=1
    assert_contains "Swift HUD action diagnostic smoke" "$CORE_LOG" "Action diagnostic requested" || ok=1
    assert_socket_clean_after_owned_core_exit || ok=1

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "Swift HUD action diagnostic smoke passed"
    else
        tail -160 "$SWIFT_LOG" || true
        tail -140 "$CORE_LOG" || true
        cat "$CLI_LOG" || true
        exit 1
    fi
}

main "$@"
