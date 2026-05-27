#!/usr/bin/env bash
# scripts/live-approval-lifecycle-smoke.sh - approval lifecycle regression.
#
# Starts a release core with a short approval timeout, then uses dexter-cli to
# exercise typed yes/no/cancel approval paths plus a delayed stale approval.

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
LOG="/tmp/dexter-approval-lifecycle-smoke.log"
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
    say "$INFO" "starting release core with 2s approval timeout; log: $LOG"
    DEXTER_ACTION_APPROVAL_TIMEOUT_SECS=2 RUST_LOG=info "$CORE_BIN" >> "$LOG" 2>&1 &
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

make_action() {
    local token="$1"
    printf '{"type":"shell","args":["echo",%s],"rationale":"approval lifecycle smoke","category_override":"destructive"}' "$(json_string "$token")"
}

run_cli() {
    local out_file="$1"
    shift
    : > "$out_file"
    "$CLI_BIN" "$@" > "$out_file" 2>&1
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

assert_log_contains() {
    local pattern="$1"
    local label="$2"
    if ! grep -Fq "$pattern" "$LOG"; then
        say "$FAIL" "$label - core log missing: $pattern"
        tail -120 "$LOG" || true
        return 1
    fi
    return 0
}

main() {
    require_bins
    start_core_if_requested

    local stamp yes_token no_token cancel_token expired_token
    stamp="$(date +%s)-$$"
    yes_token="APPROVAL_TYPED_YES_$stamp"
    no_token="APPROVAL_TYPED_NO_$stamp"
    cancel_token="APPROVAL_TYPED_CANCEL_$stamp"
    expired_token="APPROVAL_EXPIRED_$stamp"

    local yes_out no_out cancel_out expired_out recent_out ok
    yes_out="$(mktemp -t dexter-approval-yes.XXXXXX)"
    no_out="$(mktemp -t dexter-approval-no.XXXXXX)"
    cancel_out="$(mktemp -t dexter-approval-cancel.XXXXXX)"
    expired_out="$(mktemp -t dexter-approval-expired.XXXXXX)"
    recent_out="$(mktemp -t dexter-approval-recent.XXXXXX)"
    ok=0

    run_cli "$yes_out" --approval-text yes --idle-timeout 180 --action-json "$(make_action "$yes_token")" || {
        say "$FAIL" "typed approval yes - dexter-cli failed"
        cat "$yes_out"
        exit 1
    }
    assert_contains "$yes_out" "outcome=executed" "typed approval yes" || ok=1
    assert_contains "$yes_out" "$yes_token" "typed approval yes" || ok=1

    run_cli "$no_out" --approval-text no --idle-timeout 180 --action-json "$(make_action "$no_token")" || {
        say "$FAIL" "typed approval no - dexter-cli failed"
        cat "$no_out"
        exit 1
    }
    assert_contains "$no_out" "outcome=denied" "typed approval no" || ok=1

    run_cli "$cancel_out" --approval-text cancel --idle-timeout 180 --action-json "$(make_action "$cancel_token")" || {
        say "$FAIL" "typed approval cancel - dexter-cli failed"
        cat "$cancel_out"
        exit 1
    }
    assert_contains "$cancel_out" "outcome=denied" "typed approval cancel" || ok=1

    run_cli "$expired_out" --auto-approve --approval-delay-ms 3500 --idle-timeout 180 --action-json "$(make_action "$expired_token")" || {
        say "$FAIL" "expired delayed approval - dexter-cli failed"
        cat "$expired_out"
        exit 1
    }
    assert_contains "$expired_out" "outcome=expired" "expired delayed approval" || ok=1
    assert_contains "$expired_out" "Approval expired before execution" "expired delayed approval" || ok=1

    run_cli "$recent_out" --actions recent --limit 12 || {
        say "$FAIL" "recent action receipts - dexter-cli failed"
        cat "$recent_out"
        exit 1
    }
    assert_contains "$recent_out" "$yes_token" "recent action receipts" || ok=1
    assert_contains "$recent_out" "$no_token" "recent action receipts" || ok=1
    assert_contains "$recent_out" "$cancel_token" "recent action receipts" || ok=1
    assert_contains "$recent_out" "$expired_token" "recent action receipts" || ok=1
    assert_contains "$recent_out" "EXPIRED" "recent action receipts" || ok=1

    assert_log_contains "Typed approval response received during ALERT" "typed approval paths" || ok=1
    assert_log_contains "Cancellation word arrived during ALERT" "typed cancel path" || ok=1
    assert_log_contains "ActionApproval arrived after approval deadline" "expired approval path" || ok=1

    if [[ "$ok" -eq 0 ]]; then
        if [[ "$START_CORE" -eq 1 ]]; then
            stop_core_if_owned
            assert_sockets_clean "live approval lifecycle smoke" || exit 1
        fi
        say "$PASS" "live approval lifecycle smoke passed"
    else
        tail -120 "$LOG" || true
        exit 1
    fi
}

main "$@"
