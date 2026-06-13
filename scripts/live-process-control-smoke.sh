#!/usr/bin/env bash
# scripts/live-process-control-smoke.sh - make-run external stop regression.
#
# Starts the normal `make run` process tree, waits until Swift launch has begun,
# then runs `make stop` from outside that process tree and verifies the parent
# run loop exits and the daemon socket is gone.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="/tmp/dexter-process-control-smoke.log"
SOCKET="/tmp/dexter.sock"
RUN_PID=""

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

cleanup() {
    if [[ -n "$RUN_PID" ]]; then
        kill "$RUN_PID" >/dev/null 2>&1 || true
        wait "$RUN_PID" >/dev/null 2>&1 || true
    fi
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
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

wait_for_swift_launch() {
    local waited=0
    while [[ "$waited" -lt 180 ]]; do
        if grep -Fq "cd src/swift && swift run" "$LOG" ||
           grep -Fq "[DexterClient] Ping OK" "$LOG"; then
            return 0
        fi
        if [[ -n "$RUN_PID" ]] && ! kill -0 "$RUN_PID" >/dev/null 2>&1; then
            say "$FAIL" "make run exited before Swift launch"
            tail -160 "$LOG" || true
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "make run did not reach Swift launch within 180s"
    tail -160 "$LOG" || true
    return 1
}

wait_for_run_loop_exit() {
    local waited=0
    while [[ "$waited" -lt 20 ]]; do
        if [[ -z "$RUN_PID" ]] || ! kill -0 "$RUN_PID" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "make run parent did not exit after make stop"
    tail -160 "$LOG" || true
    return 1
}

main() {
    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        say "$INFO" "stop it first so process-control smoke owns the run loop"
        exit 2
    fi

    : > "$LOG"
    say "$INFO" "starting normal make run; log: $LOG"
    (cd "$ROOT_DIR" && make run) > "$LOG" 2>&1 &
    RUN_PID="$!"

    wait_for_swift_launch || exit 1
    say "$PASS" "normal make run reached Swift launch"

    if ! make -C "$ROOT_DIR" stop; then
        say "$FAIL" "make stop failed while make run was active"
        tail -160 "$LOG" || true
        exit 1
    fi

    wait_for_run_loop_exit || exit 1
    say "$PASS" "external make stop terminated the make run loop"

    if socket_accepts; then
        say "$FAIL" "daemon socket still accepts after make stop"
        exit 1
    fi

    RUN_PID=""
    say "$PASS" "process control smoke passed"
}

main "$@"
