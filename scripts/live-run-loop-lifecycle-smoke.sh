#!/usr/bin/env bash
# scripts/live-run-loop-lifecycle-smoke.sh - verify UI quit/restart exits the normal make run tree.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
RUN_PID=""
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

cleanup() {
    if [[ -n "$RUN_PID" ]]; then
        kill "$RUN_PID" >/dev/null 2>&1 || true
        wait "$RUN_PID" >/dev/null 2>&1 || true
    fi
    bash "$ROOT_DIR/scripts/stop-dexter.sh" --quiet >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

fail_with_log() {
    local message="$1"
    local log="$2"
    say "$FAIL" "$message" >&2
    if [[ -f "$log" ]]; then
        tail -180 "$log" >&2 || true
    fi
    exit 1
}

wait_for_pattern() {
    local log="$1"
    local pattern="$2"
    local timeout_secs="$3"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if grep -Fq "$pattern" "$log"; then
            return 0
        fi
        if [[ -n "$RUN_PID" ]] && ! kill -0 "$RUN_PID" >/dev/null 2>&1; then
            grep -Fq "$pattern" "$log" && return 0
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

wait_for_run_loop_exit() {
    local timeout_secs="$1"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if [[ -z "$RUN_PID" ]] || ! kill -0 "$RUN_PID" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

run_case() {
    local action="$1"
    local log="/tmp/dexter-run-loop-${action}-smoke.log"
    local sentinel="/tmp/dexter-run-loop-${action}-restart-sentinel"
    local action_label
    action_label="$(printf '%s' "$action" | tr '[:upper:]' '[:lower:]')"

    if socket_accepts; then
        fail_with_log "a Dexter daemon is already accepting connections at $SOCKET" "$log"
    fi

    : > "$log"
    rm -f "$sentinel"
    say "$INFO" "starting make run for UI $action smoke; log: $log"
    (
        cd "$ROOT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_LIFECYCLE_ACTION="$action" \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS=3 \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=8 \
        DEXTER_PROCESS_CONTROL_RESTART_SENTINEL="$sentinel" \
            make run
    ) > "$log" 2>&1 &
    RUN_PID="$!"

    wait_for_pattern "$log" "[HUDSmoke] lifecycleActionRequest action=$action_label" 360 \
        || fail_with_log "make run did not reach HUD $action action" "$log"
    wait_for_pattern "$log" "[HUDSmoke] showDexter" 30 \
        || fail_with_log "HUD did not render lifecycle confirmation for $action" "$log"

    if [[ "$action_label" == "restart" ]]; then
        wait_for_pattern "$log" "[DexterProcessControl] restart sentinel wrote" 30 \
            || fail_with_log "restart path did not write sentinel" "$log"
        [[ -s "$sentinel" ]] \
            || fail_with_log "restart sentinel file is missing or empty" "$log"
    else
        if [[ -e "$sentinel" ]]; then
            fail_with_log "quit path unexpectedly wrote restart sentinel" "$log"
        fi
    fi

    wait_for_run_loop_exit 45 \
        || fail_with_log "make run parent did not exit after UI $action" "$log"
    RUN_PID=""

    if socket_accepts; then
        fail_with_log "daemon socket still accepts after UI $action" "$log"
    fi
    [[ ! -e "$SOCKET" && ! -e "$SHELL_SOCKET" ]] \
        || fail_with_log "socket files remain after UI $action" "$log"

    say "$PASS" "UI $action exited the normal make run tree"
}

run_case restart
run_case quit
say "$PASS" "run-loop lifecycle smoke passed"
