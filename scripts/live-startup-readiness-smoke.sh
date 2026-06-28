#!/usr/bin/env bash
# scripts/live-startup-readiness-smoke.sh - startup readiness regression.
#
# Starts a release Dexter core without Swift, verifies the Unix socket appears
# first, verifies pending health wording, waits for doctor-clean daemon readiness
# through the Makefile gate, then confirms cleanup removes the daemon sockets.

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
LOG="/tmp/dexter-startup-readiness-smoke.log"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
CORE_PID=""

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

cleanup() {
    if [[ -n "$CORE_PID" ]]; then
        local pid="$CORE_PID"
        CORE_PID=""
        kill "$pid" >/dev/null 2>&1 || true
        wait "$pid" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

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
        exit 2
    fi
    if [[ ! -x "$CLI_BIN" ]]; then
        say "$FAIL" "missing CLI binary: $CLI_BIN"
        exit 2
    fi
}

start_core() {
    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        say "$INFO" "stop it first so startup readiness is tested from a clean boot"
        exit 2
    fi

    : > "$LOG"
    say "$INFO" "starting release core; log: $LOG"
    RUST_LOG=info "$CORE_BIN" >> "$LOG" 2>&1 &
    CORE_PID="$!"
}

assert_core_alive() {
    if [[ -z "$CORE_PID" ]]; then
        say "$FAIL" "core PID was not recorded"
        return 1
    fi
    if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
        say "$FAIL" "core exited during startup readiness smoke"
        tail -120 "$LOG" || true
        return 1
    fi
    return 0
}

assert_doctor_clean() {
    local out_file
    out_file="$(mktemp -t dexter-startup-doctor.XXXXXX)"

    if ! "$CLI_BIN" --doctor > "$out_file" 2>&1; then
        say "$FAIL" "doctor command failed after readiness gate"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    if ! grep -Fq "OK   daemon health      status ready" "$out_file"; then
        say "$FAIL" "doctor did not report ready daemon health"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    if ! grep -Fq "Result: OK - no failed checks." "$out_file"; then
        say "$FAIL" "doctor did not report clean health after readiness gate"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    rm -f "$out_file"
    say "$PASS" "doctor reports ready and clean health"
    return 0
}

assert_pending_snapshot_is_warming() {
    local out_file
    out_file="$(mktemp -t dexter-startup-pending-doctor.XXXXXX)"

    "$CLI_BIN" --doctor > "$out_file" 2>&1 || true

    if ! grep -Fq "status pending" "$out_file"; then
        say "$INFO" "startup pending snapshot already advanced to ready; warming-label assertion skipped"
        rm -f "$out_file"
        return 0
    fi

    if grep -Fq "not warm" "$out_file"; then
        say "$FAIL" "pending startup doctor snapshot used failure wording for warming models"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    if ! grep -Fq "warming" "$out_file"; then
        say "$FAIL" "pending startup doctor snapshot did not label warming models"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    if grep -Fq "Suggested fixes:" "$out_file"; then
        say "$FAIL" "pending startup doctor snapshot suggested recovery before warmup completed"
        cat "$out_file"
        rm -f "$out_file"
        return 1
    fi

    rm -f "$out_file"
    say "$PASS" "pending startup doctor snapshot labels models as warming"
    return 0
}

assert_cleanup_removes_sockets() {
    cleanup

    if socket_accepts; then
        say "$FAIL" "daemon still accepts connections after cleanup"
        return 1
    fi

    if [[ -e "$SOCKET" || -e "$SHELL_SOCKET" ]]; then
        say "$FAIL" "daemon left stale socket files after SIGTERM cleanup"
        ls -l "$SOCKET" "$SHELL_SOCKET" 2>/dev/null || true
        return 1
    fi

    say "$PASS" "owned daemon stopped without stale sockets"
    return 0
}

main() {
    require_bins
    start_core

    if ! make -C "$ROOT_DIR" wait-for-core; then
        say "$FAIL" "wait-for-core failed"
        tail -120 "$LOG" || true
        exit 1
    fi
    assert_core_alive || exit 1
    say "$PASS" "socket gate passed"
    assert_pending_snapshot_is_warming || exit 1

    if ! make -C "$ROOT_DIR" wait-for-ready; then
        say "$FAIL" "wait-for-ready failed"
        tail -120 "$LOG" || true
        exit 1
    fi
    assert_core_alive || exit 1
    say "$PASS" "doctor readiness gate passed"

    assert_doctor_clean || exit 1
    assert_cleanup_removes_sockets || exit 1
    say "$PASS" "startup readiness smoke passed"
}

main "$@"
