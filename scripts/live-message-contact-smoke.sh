#!/usr/bin/env bash
# scripts/live-message-contact-smoke.sh - opt-in Contacts-backed iMessage smoke.
#
# This verifies the real structured message_send path for an existing Contacts
# entry. By default, dexter-cli auto-denies the approval request, so the expected
# path is:
#   message_send -> Contacts resolution -> Messages AppleScript -> ActionRequest
#   -> auto-deny -> no background dispatch -> no send.
#
# With DEXTER_SMOKE_APPROVAL_MODE=approve and DEXTER_SMOKE_ALLOW_REAL_SEND=1, it
# auto-approves the same path and sends the message. Keep that mode opt-in.
#
# Usage:
#   DEXTER_SMOKE_CONTACT_NAME="Some Test Contact" scripts/live-message-contact-smoke.sh --start-core
#   DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" DEXTER_SMOKE_APPROVAL_MODE=approve DEXTER_SMOKE_ALLOW_REAL_SEND=1 scripts/live-message-contact-smoke.sh --start-core
#   DEXTER_SMOKE_CONTACT_NAME="Some Test Contact" scripts/live-message-contact-smoke.sh /tmp/dexter.log

set -u

SOCKET="/tmp/dexter.sock"
LOG="/tmp/dexter-message-contact-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
CORE_PID=""
APPROVAL_MODE="${DEXTER_SMOKE_APPROVAL_MODE:-deny}"

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

cleanup() {
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

require_contact_name() {
    if [[ -z "${DEXTER_SMOKE_CONTACT_NAME:-}" ]]; then
        say "$FAIL" "DEXTER_SMOKE_CONTACT_NAME is required"
        say "$INFO" "Use an existing Contacts entry with a reachable phone or iMessage email."
        say "$INFO" "Example: DEXTER_SMOKE_CONTACT_NAME=\"Some Test Contact\" make live-smoke-message-contact"
        exit 2
    fi
}

require_approval_mode() {
    case "$APPROVAL_MODE" in
        deny|approve)
            ;;
        *)
            say "$FAIL" "DEXTER_SMOKE_APPROVAL_MODE must be 'deny' or 'approve' (got '$APPROVAL_MODE')"
            exit 2
            ;;
    esac

    if [[ "$APPROVAL_MODE" == "approve" && "${DEXTER_SMOKE_ALLOW_REAL_SEND:-}" != "1" ]]; then
        say "$FAIL" "approve mode sends a real iMessage and requires DEXTER_SMOKE_ALLOW_REAL_SEND=1"
        say "$INFO" "Example: DEXTER_SMOKE_CONTACT_NAME=\"Jason Phillips\" DEXTER_SMOKE_ALLOW_REAL_SEND=1 make live-smoke-message-contact-approve"
        exit 2
    fi
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

show_failure_context() {
    local out_file="$1"
    say "$INFO" "dexter-cli output:"
    cat "$out_file" || true
    say "$INFO" "recent core log:"
    tail -80 "$LOG" || true
}

run_known_contact_smoke() {
    local name
    local contact="$DEXTER_SMOKE_CONTACT_NAME"
    local body="${DEXTER_SMOKE_MESSAGE_BODY:-Dexter known-contact smoke test $(date -u +%Y%m%dT%H%M%SZ). Do not reply.}"
    local prompt="send a text to ${contact} saying ${body}"
    local offset out ok cli_flag

    if [[ "$APPROVAL_MODE" == "approve" ]]; then
        name="known-contact message_send resolves and auto-approves"
        cli_flag="--auto-approve"
    else
        name="known-contact message_send resolves and auto-denies"
        cli_flag="--auto-deny"
    fi

    offset="$(log_bytes)"
    out="$(mktemp -t dexter-message-contact.XXXXXX)"
    ok=0

    say "$INFO" "testing Contacts-backed message_send for: $contact (mode: $APPROVAL_MODE)"
    if ! "$CLI_BIN" "$cli_flag" --idle-timeout 180 "$prompt" > "$out" 2>&1; then
        say "$FAIL" "$name - dexter-cli failed"
        show_failure_context "$out"
        rm -f "$out"
        return 1
    fi

    assert_count_at_least "$name" "$offset" "Structured iMessage send" 1 || ok=1
    assert_count_at_least "$name" "$offset" "recipient resolved through Contacts" 1 || ok=1
    assert_count_at_least "$name" "$offset" "Action requires operator approval" 1 || ok=1
    assert_count_at_least "$name" "$offset" "ActionApproval received" 1 || ok=1
    assert_absent "$name" "$offset" "Action dispatched to background task" || ok=1

    if ! grep -Fq "[ACTION REQUEST" "$out"; then
        say "$FAIL" "$name - CLI did not receive an ActionRequest"
        ok=1
    fi
    if ! grep -Fq "category=Destructive" "$out"; then
        say "$FAIL" "$name - ActionRequest was not destructive/approval-gated"
        ok=1
    fi
    if grep -Fq "[idle timeout" "$out"; then
        say "$FAIL" "$name - CLI waited for idle timeout instead of a clean IDLE transition"
        ok=1
    fi
    if ! grep -Fq "[DONE]" "$out"; then
        say "$FAIL" "$name - CLI did not observe the final IDLE transition"
        ok=1
    fi

    if [[ "$APPROVAL_MODE" == "approve" ]]; then
        assert_count_at_least "$name" "$offset" "Operator approved DESTRUCTIVE action — executing" 1 || ok=1
        assert_count_at_least "$name" "$offset" "Approved action completed" 1 || ok=1
        assert_absent "$name" "$offset" "Action rejected by operator" || ok=1

        if ! grep -Fq "approved=true" "$out"; then
            say "$FAIL" "$name - CLI did not auto-approve the ActionRequest"
            ok=1
        fi
        if ! grep -Fq "Action completed:" "$out" && ! grep -Fxq "Sent." "$out"; then
            say "$FAIL" "$name - operator-visible completion message was missing"
            ok=1
        fi
    else
        assert_count_at_least "$name" "$offset" "Action rejected by operator" 1 || ok=1
        assert_absent "$name" "$offset" "Operator approved DESTRUCTIVE action — executing" || ok=1
        assert_absent "$name" "$offset" "Approved action completed" || ok=1

        if ! grep -Fq "approved=false" "$out"; then
            say "$FAIL" "$name - CLI did not auto-deny the ActionRequest"
            ok=1
        fi
        if ! grep -Fq "Action cancelled: operator rejected the action" "$out"; then
            say "$FAIL" "$name - operator-visible rejection message was missing"
            ok=1
        fi
        if grep -Fxq "Sent." "$out"; then
            say "$FAIL" "$name - terminal send confirmation appeared despite auto-deny"
            ok=1
        fi
        if grep -Fq "Action completed:" "$out"; then
            say "$FAIL" "$name - completion appeared despite auto-deny"
            ok=1
        fi
    fi

    if [[ "$ok" -ne 0 ]]; then
        show_failure_context "$out"
        rm -f "$out"
        return 1
    fi

    rm -f "$out"
    if [[ "$APPROVAL_MODE" == "approve" ]]; then
        say "$PASS" "$name - Contacts resolution reached approval and send completed"
    else
        say "$PASS" "$name - Contacts resolution reached approval and denial prevented send"
    fi
}

main() {
    require_contact_name
    require_approval_mode
    require_bins
    start_core_if_requested
    say "$INFO" "using log: $LOG"
    run_known_contact_smoke
}

main "$@"
