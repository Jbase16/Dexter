#!/usr/bin/env bash
# scripts/live-action-safety-shared-smoke.sh - faster action safety pass.
#
# Starts one release Dexter core, then runs the compatible CLI/action smoke
# checks against that shared daemon. This is a day-to-day regression lane; the
# isolated `make live-smoke-action-safety` target remains the stronger release
# check because it proves every smoke can own a fresh daemon independently.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CORE_BIN="$RUST_DIR/target/release/dexter-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
SUMMARY_DIR="${DEXTER_SMOKE_SUMMARY_DIR:-$ROOT_DIR/docs/live-smoke-results}/action-safety-shared"
STAMP="$(date '+%Y%m%d_%H%M%S')"
LOG_DIR="$SUMMARY_DIR/logs/$STAMP"
SUMMARY_FILE="$SUMMARY_DIR/live-smoke-action-safety-shared-$STAMP.md"
LATEST_FILE="$SUMMARY_DIR/latest.md"
CORE_LOG="$LOG_DIR/shared-core.log"
CORE_PID=""
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"

TARGET_NAMES=()
TARGET_STATUSES=()
TARGET_DURATIONS=()
TARGET_LOGS=()

say() {
    printf '[%s] %s\n' "$1" "$2"
}

duration_label() {
    local seconds="$1"
    local minutes=$((seconds / 60))
    local rest=$((seconds % 60))
    if [[ "$minutes" -gt 0 ]]; then
        printf '%dm %02ds' "$minutes" "$rest"
    else
        printf '%ds' "$rest"
    fi
}

markdown_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//|/\\|}"
    value="${value//$'\n'/ }"
    printf '%s' "$value"
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

require_clean_socket() {
    if socket_accepts; then
        say FAIL "a Dexter daemon is already accepting connections at $SOCKET"
        say INFO "stop it first, then rerun this shared-core smoke"
        exit 2
    fi
}

build_binaries() {
    say INFO "building release core and CLI once"
    (
        cd "$RUST_DIR" || exit 2
        cargo build --release --bin dexter-core --bin dexter-cli
    ) || exit 2
    if [[ ! -x "$CORE_BIN" || ! -x "$CLI_BIN" ]]; then
        say FAIL "release binaries were not produced"
        exit 2
    fi
}

start_shared_core() {
    rm -f "$SOCKET" "$SHELL_SOCKET"
    : > "$CORE_LOG"
    say INFO "starting one shared release core; log: $CORE_LOG"
    DEXTER_ACTION_APPROVAL_TIMEOUT_SECS=2 RUST_LOG=info "$CORE_BIN" >> "$CORE_LOG" 2>&1 &
    CORE_PID="$!"

    local waited=0
    while [[ "$waited" -lt 90 ]]; do
        if socket_accepts; then
            break
        fi
        if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say FAIL "shared core exited before opening socket"
            tail -80 "$CORE_LOG" || true
            exit 2
        fi
        sleep 1
        waited=$((waited + 1))
    done

    if ! socket_accepts; then
        say FAIL "shared core did not open $SOCKET within 90s"
        tail -80 "$CORE_LOG" || true
        exit 2
    fi

    waited=0
    while [[ "$waited" -lt "$CORE_WARMUP_TIMEOUT_SECS" ]]; do
        "$CLI_BIN" --doctor >/tmp/dexter-action-safety-shared-doctor.out 2>&1 || true
        if grep -Fq "OK   daemon health      status ready" /tmp/dexter-action-safety-shared-doctor.out \
            && grep -Fq "Result: OK - no failed checks." /tmp/dexter-action-safety-shared-doctor.out; then
            say INFO "shared core doctor-ready after ${waited}s"
            return
        fi
        if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say FAIL "shared core exited during warmup"
            tail -120 "$CORE_LOG" || true
            exit 2
        fi
        sleep 2
        waited=$((waited + 2))
    done

    say FAIL "shared core did not become doctor-ready within ${CORE_WARMUP_TIMEOUT_SECS}s"
    say INFO "last doctor report:"
    cat /tmp/dexter-action-safety-shared-doctor.out 2>/dev/null || true
    tail -120 "$CORE_LOG" || true
    exit 2
}

run_target() {
    local name="$1"
    shift
    local target_start target_end duration log_file status
    target_start="$(date '+%s')"
    log_file="$LOG_DIR/$name.log"
    TARGET_NAMES+=("$name")
    TARGET_LOGS+=("$log_file")

    say INFO "running $name against shared core"
    "$@" > "$log_file" 2>&1
    status="$?"

    target_end="$(date '+%s')"
    duration="$((target_end - target_start))"
    TARGET_DURATIONS+=("$(duration_label "$duration")")

    if [[ "$status" -eq 0 ]]; then
        TARGET_STATUSES+=("PASS")
        say PASS "$name completed in $(duration_label "$duration")"
        return 0
    fi

    TARGET_STATUSES+=("FAIL")
    say FAIL "$name exited $status after $(duration_label "$duration")"
    tail -80 "$log_file" || true
    return "$status"
}

write_summary() {
    local started_at="$1"
    local finished_at="$2"
    local total_duration="$3"
    local overall_status="$4"
    local pass_count=0
    local fail_count=0
    local status
    for status in "${TARGET_STATUSES[@]}"; do
        if [[ "$status" == "PASS" ]]; then
            pass_count=$((pass_count + 1))
        else
            fail_count=$((fail_count + 1))
        fi
    done

    {
        echo "# Dexter Shared-Core Action Safety Smoke"
        echo
        echo "- Started: \`$started_at\`"
        echo "- Finished: \`$finished_at\`"
        echo "- Duration: \`$total_duration\`"
        echo "- Root: \`$ROOT_DIR\`"
        echo "- Result: \`$([[ "$overall_status" -eq 0 ]] && echo PASS || echo FAIL)\`"
        echo "- Mode: \`shared-core\`"
        echo "- Passed: \`$pass_count\`"
        echo "- Failed: \`$fail_count\`"
        echo "- Logs: \`$LOG_DIR\`"
        echo "- Core Log: \`$CORE_LOG\`"
        echo
        echo "## Targets"
        echo
        echo "| Target | Result | Duration | Log |"
        echo "|---|---:|---:|---|"
        for idx in "${!TARGET_NAMES[@]}"; do
            echo "| \`$(markdown_escape "${TARGET_NAMES[$idx]}")\` | ${TARGET_STATUSES[$idx]} | \`${TARGET_DURATIONS[$idx]}\` | \`$(markdown_escape "${TARGET_LOGS[$idx]}")\` |"
        done
        echo
        if [[ "$fail_count" -gt 0 ]]; then
            echo "## Failure Tails"
            echo
            for idx in "${!TARGET_NAMES[@]}"; do
                if [[ "${TARGET_STATUSES[$idx]}" != "FAIL" ]]; then
                    continue
                fi
                echo "### \`$(markdown_escape "${TARGET_NAMES[$idx]}")\`"
                echo
                echo '```text'
                tail -80 "${TARGET_LOGS[$idx]}" || true
                echo '```'
                echo
            done
        fi
        echo "## Re-run"
        echo
        echo '```bash'
        echo "make live-smoke-action-safety-shared"
        echo '```'
    } > "$SUMMARY_FILE"

    cp "$SUMMARY_FILE" "$LATEST_FILE"
}

main() {
    mkdir -p "$LOG_DIR"
    local started_at start_epoch overall_status finished_at end_epoch total_duration
    started_at="$(date '+%Y-%m-%dT%H:%M:%S%z')"
    start_epoch="$(date '+%s')"
    overall_status=0

    require_clean_socket
    build_binaries
    start_shared_core

    run_target live-action-diagnostic-smoke bash scripts/live-action-diagnostic-smoke.sh "$CORE_LOG" || overall_status=1
    if [[ "$overall_status" -eq 0 ]]; then
        run_target live-action-matrix-smoke bash scripts/live-cli-smoke.sh --action-matrix "$CORE_LOG" || overall_status=1
    fi
    if [[ "$overall_status" -eq 0 ]]; then
        run_target live-browser-recovery-smoke bash scripts/live-cli-smoke.sh --browser-recovery "$CORE_LOG" || overall_status=1
    fi
    if [[ "$overall_status" -eq 0 ]]; then
        run_target live-action-receipts-smoke bash scripts/live-action-receipts-smoke.sh "$CORE_LOG" || overall_status=1
    fi
    if [[ "$overall_status" -eq 0 ]]; then
        run_target live-approval-lifecycle-smoke bash scripts/live-approval-lifecycle-smoke.sh "$CORE_LOG" || overall_status=1
    fi
    if [[ "$overall_status" -eq 0 ]]; then
        run_target live-action-cancel-smoke bash scripts/live-action-cancel-smoke.sh "$CORE_LOG" || overall_status=1
    fi

    stop_core_if_owned

    finished_at="$(date '+%Y-%m-%dT%H:%M:%S%z')"
    end_epoch="$(date '+%s')"
    total_duration="$(duration_label "$((end_epoch - start_epoch))")"
    write_summary "$started_at" "$finished_at" "$total_duration" "$overall_status"
    say INFO "summary written: $SUMMARY_FILE"
    say INFO "latest summary: $LATEST_FILE"
    exit "$overall_status"
}

main "$@"
