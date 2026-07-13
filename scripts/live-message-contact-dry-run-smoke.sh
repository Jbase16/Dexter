#!/usr/bin/env bash
# scripts/live-message-contact-dry-run-smoke.sh - no-send Contacts resolution smoke.
#
# This is a safe Phase 68 probe: it asks Dexter to send a message to a
# deliberately unique missing Contacts name and verifies Rust refuses before
# approval or Messages delivery. It may touch Contacts.app for lookup, but it
# must never reach an ActionRequest or send path.

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
LOG="/tmp/dexter-message-contact-dry-run-smoke.log"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
CORE_PID=""
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"

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

assert_absent_since() {
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

require_bins() {
    if [[ ! -x "$CORE_BIN" || ! -x "$CLI_BIN" ]]; then
        say "$FAIL" "missing release binaries; run: cd src/rust-core && cargo build --release --bin dexter-core --bin dexter-cli"
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

start_core() {
    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        say "$INFO" "stop it first; this smoke must start its own core"
        exit 2
    fi

    rm -f "$SOCKET" "$SHELL_SOCKET"
    : > "$LOG"
    say "$INFO" "starting release core; log: $LOG"
    RUST_LOG=info "$CORE_BIN" >>"$LOG" 2>&1 &
    CORE_PID="$!"

    local waited=0
    while [[ "$waited" -lt 90 ]]; do
        if socket_accepts; then
            break
        fi
        if [[ -n "$CORE_PID" ]] && ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say "$FAIL" "core exited before opening socket"
            tail -80 "$LOG" || true
            exit 2
        fi
        sleep 1
        waited=$((waited + 1))
    done
    if ! socket_accepts; then
        say "$FAIL" "core did not open $SOCKET within 90s"
        tail -80 "$LOG" || true
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
            tail -100 "$LOG" || true
            exit 2
        fi
        sleep 1
        waited=$((waited + 1))
    done
    say "$FAIL" "core socket opened, but warmup did not complete within ${CORE_WARMUP_TIMEOUT_SECS}s"
    tail -100 "$LOG" || true
    exit 2
}

assert_sockets_clean() {
    if socket_accepts; then
        say "$FAIL" "daemon still accepts connections after cleanup"
        return 1
    fi
    if [[ -e "$SOCKET" || -e "$SHELL_SOCKET" ]]; then
        say "$FAIL" "stale socket files remain"
        ls -l "$SOCKET" "$SHELL_SOCKET" 2>/dev/null || true
        return 1
    fi
    return 0
}

run_missing_contact_dry_run() {
    local name="missing Contacts recipient refuses before approval"
    local token="Dexter Missing Contact $(date -u +%Y%m%dT%H%M%SZ) $$"
    local body="Dexter contact dry run should not send"
    local prompt="send a text to ${token} saying ${body}"
    local offset out actions_out ok

    offset="$(log_bytes)"
    out="$(mktemp -t dexter-message-contact-dry-run.XXXXXX)"
    actions_out="$(mktemp -t dexter-message-contact-dry-run-actions.XXXXXX)"
    ok=0

    say "$INFO" "testing no-send missing Contacts recipient: $token"
    if ! "$CLI_BIN" --quiet --auto-deny --idle-timeout 240 "$prompt" >"$out" 2>&1; then
        say "$FAIL" "$name - dexter-cli failed"
        cat "$out"
        rm -f "$out"
        return 1
    fi

    assert_count_at_least "$name" "$offset" "Structured iMessage send" 1 || ok=1
    if ! log_since "$offset" | grep -Eq "Contacts name resolution returned no match|Contacts lookup failed"; then
        say "$FAIL" "$name - did not hit the Contacts refusal path"
        ok=1
    fi
    assert_absent_since "$name" "$offset" "Action requires operator approval" || ok=1
    assert_absent_since "$name" "$offset" "ActionApproval received" || ok=1
    assert_absent_since "$name" "$offset" "Operator approved DESTRUCTIVE action" || ok=1
    assert_absent_since "$name" "$offset" "Approved action completed" || ok=1

    if grep -Fq "[ACTION REQUEST" "$out"; then
        say "$FAIL" "$name - CLI received an ActionRequest despite missing contact"
        ok=1
    fi
    if grep -Fxq "Sent." "$out" || grep -Fq "Action completed:" "$out"; then
        say "$FAIL" "$name - send completion appeared despite missing contact"
        ok=1
    fi
    if ! grep -Eq "couldn't find|Contacts lookup failed|didn't send" "$out"; then
        say "$FAIL" "$name - operator-visible refusal message missing"
        ok=1
    fi
    if ! "$CLI_BIN" --actions last >"$actions_out" 2>&1; then
        say "$FAIL" "$name - latest action receipt inspection failed"
        ok=1
    elif ! grep -Fq "message_send" "$actions_out"; then
        say "$FAIL" "$name - latest receipt is not message_send"
        ok=1
    elif ! grep -Fq "FAILED" "$actions_out"; then
        say "$FAIL" "$name - latest receipt did not show failed preflight"
        ok=1
    elif ! grep -Fq "Structured iMessage send refused before execution" "$actions_out"; then
        say "$FAIL" "$name - latest receipt did not preserve Contacts preflight cause"
        ok=1
    elif grep -Fq "$body" "$actions_out"; then
        say "$FAIL" "$name - latest receipt leaked message body"
        ok=1
    fi

    if [[ "$ok" -ne 0 ]]; then
        say "$INFO" "dexter-cli output:"
        cat "$out" || true
        say "$INFO" "latest action receipt:"
        cat "$actions_out" || true
        say "$INFO" "recent core log:"
        tail -100 "$LOG" || true
        rm -f "$out" "$actions_out"
        return 1
    fi

    rm -f "$out" "$actions_out"
    say "$PASS" "$name - no approval or send path opened"
}

require_bins
start_core
say "$INFO" "using log: $LOG"
run_missing_contact_dry_run
stop_core_if_owned
assert_sockets_clean
