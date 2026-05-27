#!/usr/bin/env bash
# scripts/live-action-diagnostic-smoke.sh - live regression for action diagnostics.
#
# Starts a release Dexter core, drives a synthetic action that must fail closed,
# then verifies `dexter-cli --why` explains the failure from local evidence.
#
# Usage:
#   scripts/live-action-diagnostic-smoke.sh --start-core
#   scripts/live-action-diagnostic-smoke.sh /tmp/dexter-action-diagnostic-smoke.log

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
LOG="/tmp/dexter-action-diagnostic-smoke.log"
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

main() {
    require_bins
    start_core_if_requested

    local stamp recipient action action_out why_out ok
    stamp="$(date +%s)-$$"
    recipient="DiagnosticBypass_$stamp"
    action='{"type":"message_send","recipient":'$(json_string "$recipient" )',"body":"diagnostic smoke","rationale":"diagnostic smoke"}'
    action_out="$(mktemp -t dexter-action-diagnostic-action.XXXXXX)"
    why_out="$(mktemp -t dexter-action-diagnostic-why.XXXXXX)"
    ok=0

    if ! "$CLI_BIN" --quiet --idle-timeout 180 --action-json "$action" > "$action_out" 2>&1; then
        say "$FAIL" "action diagnostic seed action - dexter-cli failed"
        cat "$action_out"
        ok=1
    fi

    if [[ "$ok" -eq 0 ]]; then
        "$CLI_BIN" --why > "$why_out" 2>&1 || ok=1

        assert_contains "$why_out" "### Action Diagnostic" "why prints diagnostic header" || ok=1
        assert_contains "$why_out" "Most Likely Cause" "why prints cause section" || ok=1
        assert_contains "$why_out" "raw message_send action was blocked" "why explains Contacts resolution boundary" || ok=1
        assert_contains "$why_out" "message_send must be resolved by the orchestrator" "why includes concrete evidence" || ok=1
        assert_contains "$why_out" "Recent Action Evidence" "why prints action evidence section" || ok=1
        assert_contains "$why_out" "Send iMessage to: $recipient" "why identifies blocked recipient" || ok=1
    fi

    rm -f "$action_out" "$why_out"

    if [[ "$ok" -eq 0 ]]; then
        if [[ "$START_CORE" -eq 1 ]]; then
            stop_core_if_owned
            assert_sockets_clean "live action diagnostic smoke" || exit 1
        fi
        say "$PASS" "live action diagnostic smoke passed"
        exit 0
    fi

    say "$FAIL" "live action diagnostic smoke failed"
    exit 1
}

main "$@"
