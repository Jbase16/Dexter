#!/usr/bin/env bash
# scripts/live-external-failures-smoke.sh - deterministic external-integration failure smoke.
#
# Starts a fresh release core with short/injected failure knobs, then verifies
# privileged external surfaces fail visibly instead of hanging, guessing, or
# slipping around the Rust-side action boundary.

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
LOG="/tmp/dexter-external-failures-smoke.log"
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

assert_file_contains() {
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

start_core() {
    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        say "$INFO" "stop it first; this smoke must start its own core so failure knobs are guaranteed"
        exit 2
    fi

    rm -f "$SOCKET" "$SHELL_SOCKET"
    : > "$LOG"
    say "$INFO" "starting release core with external-failure knobs; log: $LOG"
    DEXTER_ACTION_APPLESCRIPT_TIMEOUT_SECS=2 \
    DEXTER_SCREENCAPTURE_BIN=/tmp/dexter-missing-screencapture-smoke \
    RUST_LOG=info \
    "$CORE_BIN" >> "$LOG" 2>&1 &
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

run_action_quiet() {
    local out_file="$1"
    local action_json="$2"
    : > "$out_file"
    "$CLI_BIN" --quiet --idle-timeout 180 --action-json "$action_json" > "$out_file" 2>&1
}

run_text_quiet() {
    local out_file="$1"
    local text="$2"
    : > "$out_file"
    "$CLI_BIN" --quiet --idle-timeout 180 "$text" > "$out_file" 2>&1
}

test_message_send_fails_closed() {
    local name="generic message_send action fails closed"
    local action_json offset out ok
    action_json='{"type":"message_send","recipient":"Dexter External Smoke","body":"external failure smoke","rationale":"external failure smoke"}'
    offset="$(log_bytes)"
    out="$(mktemp -t dexter-external-message-send.XXXXXX)"
    ok=0

    if ! run_action_quiet "$out" "$action_json"; then
        say "$FAIL" "$name - dexter-cli failed"
        cat "$out"
        rm -f "$out"
        return 1
    fi

    assert_file_contains "$out" "message_send must be resolved by the orchestrator before execution" "$name" || ok=1
    assert_count_at_least "$name" "$offset" "Synthetic ActionSpec received from dexter-cli" 1 || ok=1
    assert_count_at_least "$name" "$offset" "Action status injected into conversation context" 1 || ok=1
    assert_absent_since "$name" "$offset" "Action requires operator approval" || ok=1
    rm -f "$out"

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name - structured send cannot bypass orchestrator Contacts resolution"
        return 0
    fi
    return 1
}

test_applescript_error_reports_failure() {
    local name="AppleScript runtime error is operator-visible"
    local script action_json offset out ok
    script='error "Dexter external smoke failure" number -128'
    action_json='{"type":"apple_script","script":'$(json_string "$script" )',"rationale":"external failure smoke"}'
    offset="$(log_bytes)"
    out="$(mktemp -t dexter-external-applescript-error.XXXXXX)"
    ok=0

    if ! run_action_quiet "$out" "$action_json"; then
        say "$FAIL" "$name - dexter-cli failed"
        cat "$out"
        rm -f "$out"
        return 1
    fi

    assert_file_contains "$out" "Action failed: AppleScript: external failure smoke" "$name" || ok=1
    assert_file_contains "$out" "Dexter external smoke failure" "$name" || ok=1
    assert_count_at_least "$name" "$offset" "Synthetic ActionSpec received from dexter-cli" 1 || ok=1
    assert_count_at_least "$name" "$offset" "Action status injected into conversation context" 1 || ok=1
    rm -f "$out"

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name - osascript stderr is surfaced as an action failure"
        return 0
    fi
    return 1
}

test_applescript_timeout_reports_failure() {
    local name="AppleScript timeout is bounded and visible"
    local script action_json offset out recent ok
    script='delay 5'
    action_json='{"type":"apple_script","script":'$(json_string "$script" )',"rationale":"external timeout smoke"}'
    offset="$(log_bytes)"
    out="$(mktemp -t dexter-external-applescript-timeout.XXXXXX)"
    recent="$(mktemp -t dexter-external-applescript-timeout-recent.XXXXXX)"
    ok=0

    if ! run_action_quiet "$out" "$action_json"; then
        say "$FAIL" "$name - dexter-cli failed"
        cat "$out"
        rm -f "$out" "$recent"
        return 1
    fi

    assert_file_contains "$out" "Action failed: AppleScript: external timeout smoke" "$name" || ok=1
    assert_file_contains "$out" "timed out after 2s" "$name" || ok=1
    assert_count_at_least "$name" "$offset" "Synthetic ActionSpec received from dexter-cli" 1 || ok=1
    if ! "$CLI_BIN" --actions last > "$recent" 2>&1; then
        say "$FAIL" "$name - dexter-cli --actions last failed"
        cat "$recent"
        ok=1
    else
        assert_file_contains "$recent" "Timed out: timed out after 2s" "$name receipt" || ok=1
    fi
    rm -f "$out" "$recent"

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name - long osascript execution terminates under the smoke timeout"
        return 0
    fi
    return 1
}

test_vision_capture_failure_demotes() {
    local name="vision capture failure demotes to primary"
    local offset out ok
    offset="$(log_bytes)"
    out="$(mktemp -t dexter-external-vision-capture.XXXXXX)"
    ok=0

    if ! run_text_quiet "$out" "look at this screenshot and tell me what app is open"; then
        say "$FAIL" "$name - dexter-cli failed"
        cat "$out"
        rm -f "$out"
        return 1
    fi

    assert_count_at_least "$name" "$offset" "Vision query — capturing screen" 1 || ok=1
    assert_count_at_least "$name" "$offset" "Vision: screencapture process spawn failed" 1 || ok=1
    assert_count_at_least "$name" "$offset" "Vision demotion: no image attached, re-routing to PRIMARY" 1 || ok=1
    rm -f "$out"

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name - missing screencapture degrades without a fake visual answer"
        return 0
    fi
    return 1
}

main() {
    require_bins
    start_core

    test_message_send_fails_closed || exit 1
    test_applescript_error_reports_failure || exit 1
    test_applescript_timeout_reports_failure || exit 1
    test_vision_capture_failure_demotes || exit 1

    stop_core_if_owned
    assert_sockets_clean "external failures smoke" || exit 1
    say "$PASS" "live external-failures smoke passed"
}

main "$@"
