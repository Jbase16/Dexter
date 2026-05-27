#!/usr/bin/env bash
# scripts/live-recovery-smoke.sh - live worker recovery regression.
#
# Starts a release Dexter core, waits until `dexter-cli --doctor` reports a
# healthy daemon, restarts each shared worker through the operator recovery RPC,
# verifies health after every restart, then shuts the daemon down.
#
# Usage:
#   scripts/live-recovery-smoke.sh --start-core
#   scripts/live-recovery-smoke.sh /tmp/dexter-recovery-smoke.log

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
LOG="/tmp/dexter-recovery-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
CORE_PID=""

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

cleanup() {
    stop_core_if_owned >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

stop_core_if_owned() {
    if [[ -z "$CORE_PID" ]]; then
        return 0
    fi

    local pid="$CORE_PID"
    CORE_PID=""
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" >/dev/null 2>&1 || true
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

    : > "$LOG"
    say "$INFO" "starting release core; log: $LOG"
    RUST_LOG=info "$CORE_BIN" >> "$LOG" 2>&1 &
    CORE_PID="$!"
}

run_doctor_ok() {
    local label="$1"
    local out_file
    out_file="$(mktemp -t dexter-recovery-doctor.XXXXXX)"

    if ! "$CLI_BIN" --doctor > "$out_file" 2>&1; then
        say "$FAIL" "$label - doctor failed"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    if ! grep -Fq "Result: OK - no failed checks." "$out_file"; then
        say "$FAIL" "$label - doctor did not report clean health"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    say "$PASS" "$label - doctor OK"
    rm -f "$out_file"
    return 0
}

wait_for_doctor_ok() {
    local waited=0
    while [[ "$waited" -lt 180 ]]; do
        if run_doctor_ok "startup readiness" >/tmp/dexter-recovery-wait.out 2>&1; then
            cat /tmp/dexter-recovery-wait.out
            rm -f /tmp/dexter-recovery-wait.out
            return 0
        fi
        if [[ -n "$CORE_PID" ]] && ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say "$FAIL" "core exited during startup"
            tail -80 "$LOG" || true
            cat /tmp/dexter-recovery-wait.out 2>/dev/null || true
            rm -f /tmp/dexter-recovery-wait.out
            return 1
        fi
        sleep 2
        waited=$((waited + 2))
    done

    say "$FAIL" "doctor did not become healthy within 180s"
    tail -80 "$LOG" || true
    cat /tmp/dexter-recovery-wait.out 2>/dev/null || true
    rm -f /tmp/dexter-recovery-wait.out
    return 1
}

restart_component_ok() {
    local component="$1"
    local out_file
    out_file="$(mktemp -t dexter-recovery-restart.XXXXXX)"

    if ! "$CLI_BIN" --restart-component "$component" > "$out_file" 2>&1; then
        say "$FAIL" "restart $component - command failed"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    if ! grep -Fq "Result: OK -" "$out_file"; then
        say "$FAIL" "restart $component - post-restart doctor was not clean"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    say "$PASS" "restart $component - recovery RPC and post-restart doctor OK"
    rm -f "$out_file"
    return 0
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
    wait_for_doctor_ok || exit 1

    restart_component_ok browser || exit 1
    run_doctor_ok "after browser restart" || exit 1

    restart_component_ok tts || exit 1
    run_doctor_ok "after tts restart" || exit 1

    restart_component_ok stt || exit 1
    run_doctor_ok "after stt restart" || exit 1

    assert_owned_core_stops_cleanly || exit 1
    say "$PASS" "live recovery smoke passed"
}

main "$@"
