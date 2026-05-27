#!/usr/bin/env bash
# scripts/live-action-receipts-smoke.sh - live regression for action receipts.
#
# Starts a release Dexter core, drives synthetic action specs through dexter-cli,
# then verifies `dexter-cli --actions` can inspect the resulting audit receipts.
#
# Usage:
#   scripts/live-action-receipts-smoke.sh --start-core
#   scripts/live-action-receipts-smoke.sh /tmp/dexter-action-receipts-smoke.log

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
LOG="/tmp/dexter-action-receipts-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
CORE_PID=""
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --start-core)
            START_CORE=1
            ;;
        *)
            LOG="$1"
            ;;
    esac
    shift
done

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

log_bytes() {
    stat -f%z "$LOG" 2>/dev/null || echo 0
}

log_since() {
    local offset="$1"
    tail -c "+$((offset + 1))" "$LOG" 2>/dev/null || true
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

stop_core_if_owned() {
    if [[ -z "$CORE_PID" ]]; then
        return 0
    fi

    local pid="$CORE_PID"
    CORE_PID=""
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" >/dev/null 2>&1 || true
}

cleanup() {
    stop_core_if_owned >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

assert_sockets_clean() {
    local label="$1"
    if socket_accepts; then
        say "$FAIL" "$label - daemon still accepts connections after cleanup"
        return 1
    fi
    if [[ -e "$SOCKET" || -e "$SHELL_SOCKET" ]]; then
        say "$FAIL" "$label - stale socket files remain"
        ls -l "$SOCKET" "$SHELL_SOCKET" 2>/dev/null || true
        return 1
    fi
    return 0
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

    rm -f "$SOCKET" "$SHELL_SOCKET"
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
    while [[ "$waited" -lt "$CORE_WARMUP_TIMEOUT_SECS" ]]; do
        if grep -Fq "Daemon startup warmup complete" "$LOG"; then
            say "$INFO" "core warmup complete"
            return
        fi
        if [[ -n "$CORE_PID" ]] && ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say "$FAIL" "core exited during startup"
            tail -80 "$LOG" || true
            exit 2
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "core socket opened, but warmup did not complete within ${CORE_WARMUP_TIMEOUT_SECS}s"
    tail -80 "$LOG" || true
    exit 2
}

run_action_json() {
    local mode="$1"
    local action_json="$2"
    local out_file="$3"
    : > "$out_file"

    case "$mode" in
        deny)
            "$CLI_BIN" --auto-deny --idle-timeout 180 --action-json "$action_json" > "$out_file" 2>&1
            ;;
        approve)
            "$CLI_BIN" --auto-approve --idle-timeout 180 --action-json "$action_json" > "$out_file" 2>&1
            ;;
        quiet)
            "$CLI_BIN" --quiet --idle-timeout 180 --action-json "$action_json" > "$out_file" 2>&1
            ;;
        *)
            say "$FAIL" "internal error: unknown action mode $mode"
            return 2
            ;;
    esac
}

assert_contains() {
    local file="$1"
    local pattern="$2"
    local label="$3"
    if ! grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - missing: $pattern"
        cat "$file"
        return 1
    fi
    return 0
}

assert_log_contains_since() {
    local offset="$1"
    local pattern="$2"
    local label="$3"
    if ! log_since "$offset" | grep -Fq "$pattern"; then
        say "$FAIL" "$label - log missing: $pattern"
        log_since "$offset" | tail -80 || true
        return 1
    fi
    return 0
}

main() {
    require_bins
    start_core_if_requested

    local stamp safe_token denied_token approved_token
    stamp="$(date +%s)-$$"
    safe_token="RECEIPT_SAFE_$stamp"
    denied_token="RECEIPT_DENIED_$stamp"
    approved_token="RECEIPT_APPROVED_$stamp"

    local safe_action denied_action approved_action
    safe_action='{"type":"shell","args":["echo",'$(json_string "$safe_token" )'],"rationale":"receipt smoke safe"}'
    denied_action='{"type":"shell","args":["echo",'$(json_string "$denied_token" )'],"rationale":"receipt smoke denied","category_override":"destructive"}'
    approved_action='{"type":"shell","args":["echo",'$(json_string "$approved_token" )'],"rationale":"receipt smoke approved","category_override":"destructive"}'

    local safe_out denied_out approved_out recent_out last_out ok denied_offset
    safe_out="$(mktemp -t dexter-receipt-safe.XXXXXX)"
    denied_out="$(mktemp -t dexter-receipt-denied.XXXXXX)"
    approved_out="$(mktemp -t dexter-receipt-approved.XXXXXX)"
    recent_out="$(mktemp -t dexter-receipt-recent.XXXXXX)"
    last_out="$(mktemp -t dexter-receipt-last.XXXXXX)"
    ok=0

    if ! run_action_json quiet "$safe_action" "$safe_out"; then
        say "$FAIL" "safe receipt action - dexter-cli failed"
        cat "$safe_out"
        ok=1
    fi
    denied_offset="$(log_bytes)"
    if ! run_action_json deny "$denied_action" "$denied_out"; then
        say "$FAIL" "denied receipt action - dexter-cli failed"
        cat "$denied_out"
        ok=1
    fi
    if ! run_action_json approve "$approved_action" "$approved_out"; then
        say "$FAIL" "approved receipt action - dexter-cli failed"
        cat "$approved_out"
        ok=1
    fi

    if [[ "$ok" -eq 0 ]]; then
        assert_contains "$denied_out" "Action denied before execution: Run: echo $denied_token." "denied action is visible to operator" || ok=1
        assert_log_contains_since "$denied_offset" "Action status injected into conversation context" "denied action remembered in context" || ok=1
        assert_log_contains_since "$denied_offset" "Action denied" "denied context label logged" || ok=1

        "$CLI_BIN" --actions recent --limit 25 > "$recent_out" 2>&1 || ok=1
        "$CLI_BIN" --actions last > "$last_out" 2>&1 || ok=1

        assert_contains "$recent_out" "target: echo $safe_token" "recent receipts include safe target" || ok=1
        assert_contains "$recent_out" "review: no approval required | approval: not required" "recent receipts include safe approval" || ok=1
        assert_contains "$recent_out" "result: Succeeded: $safe_token" "recent receipts include safe output" || ok=1

        assert_contains "$recent_out" "target: echo $denied_token" "recent receipts include denied target" || ok=1
        assert_contains "$recent_out" "DENIED  shell" "recent receipts include denied status" || ok=1
        assert_contains "$recent_out" "review: approval required | approval: denied" "recent receipts include denied approval" || ok=1
        assert_contains "$recent_out" "result: Denied before execution." "recent receipts include denied result" || ok=1

        assert_contains "$recent_out" "target: echo $approved_token" "recent receipts include approved target" || ok=1
        assert_contains "$recent_out" "review: approval required | approval: approved" "recent receipts include approved approval" || ok=1
        assert_contains "$recent_out" "result: Succeeded: $approved_token" "recent receipts include approved output" || ok=1

        assert_contains "$last_out" "target: echo $approved_token" "last receipt is newest approved action" || ok=1
        assert_contains "$last_out" "review: approval required | approval: approved" "last receipt includes approval" || ok=1
    fi

    rm -f "$safe_out" "$denied_out" "$approved_out" "$recent_out" "$last_out"

    if [[ "$ok" -eq 0 ]]; then
        if [[ "$START_CORE" -eq 1 ]]; then
            stop_core_if_owned
            assert_sockets_clean "live action receipts smoke" || exit 1
        fi
        say "$PASS" "live action receipts smoke passed"
        exit 0
    fi

    say "$FAIL" "live action receipts smoke failed"
    exit 1
}

main "$@"
