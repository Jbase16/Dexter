#!/usr/bin/env bash
# scripts/live-browser-recovery-model-stability.sh - repeat the focused
# model-driven browser recovery smoke and write a stability receipt.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUMMARY_DIR="${DEXTER_SMOKE_SUMMARY_DIR:-$ROOT_DIR/docs/live-smoke-results}"
STABILITY_DIR="$SUMMARY_DIR/browser-recovery-model-stability"
STAMP="$(date '+%Y%m%d_%H%M%S')"
LOG_DIR="$STABILITY_DIR/logs/$STAMP"
SUMMARY_FILE="$STABILITY_DIR/browser-recovery-model-stability-$STAMP.md"
LATEST_FILE="$STABILITY_DIR/latest.md"
RUNS="${DEXTER_BROWSER_RECOVERY_MODEL_STABILITY_RUNS:-5}"

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

if ! [[ "$RUNS" =~ ^[1-9][0-9]*$ ]]; then
    say FAIL "DEXTER_BROWSER_RECOVERY_MODEL_STABILITY_RUNS must be a positive integer (got '$RUNS')"
    exit 2
fi

mkdir -p "$LOG_DIR"

STARTED_AT="$(date '+%Y-%m-%dT%H:%M:%S%z')"
START_EPOCH="$(date '+%s')"
RUN_STATUSES=()
RUN_DURATIONS=()
RUN_LOGS=()
overall_status=0

say INFO "running model-driven browser recovery smoke $RUNS time(s)"
say INFO "logs will be stored in $LOG_DIR"

for ((run = 1; run <= RUNS; run++)); do
    run_label="$(printf 'run-%02d' "$run")"
    log_file="$LOG_DIR/$run_label.log"
    run_start="$(date '+%s')"

    (
        cd "$ROOT_DIR" || {
            printf '[FAIL] unable to enter root directory for cleanup: %s\n' "$ROOT_DIR" >&2
            exit 2
        }
        bash scripts/stop-dexter.sh --quiet >/dev/null 2>&1 || true
    )

    say INFO "$run_label: make live-smoke-browser-recovery-model"
    (
        cd "$ROOT_DIR" || exit 2
        make live-smoke-browser-recovery-model
    ) 2>&1 | tee "$log_file"
    status="${PIPESTATUS[0]}"

    run_end="$(date '+%s')"
    duration="$((run_end - run_start))"
    RUN_DURATIONS+=("$(duration_label "$duration")")
    RUN_LOGS+=("$log_file")

    if [[ "$status" -eq 0 ]]; then
        RUN_STATUSES+=("PASS")
        say PASS "$run_label passed in $(duration_label "$duration")"
    else
        RUN_STATUSES+=("FAIL")
        overall_status=1
        say FAIL "$run_label failed with exit $status after $(duration_label "$duration")"
    fi
done

(
    cd "$ROOT_DIR" || {
        printf '[FAIL] unable to enter root directory for final cleanup: %s\n' "$ROOT_DIR" >&2
        exit 2
    }
    bash scripts/stop-dexter.sh --quiet >/dev/null 2>&1 || true
)

FINISHED_AT="$(date '+%Y-%m-%dT%H:%M:%S%z')"
END_EPOCH="$(date '+%s')"
TOTAL_DURATION="$(duration_label "$((END_EPOCH - START_EPOCH))")"

pass_count=0
fail_count=0
for status in "${RUN_STATUSES[@]}"; do
    if [[ "$status" == "PASS" ]]; then
        pass_count=$((pass_count + 1))
    else
        fail_count=$((fail_count + 1))
    fi
done

{
    echo "# Dexter Browser Recovery Model Stability"
    echo
    echo "- Started: \`$STARTED_AT\`"
    echo "- Finished: \`$FINISHED_AT\`"
    echo "- Duration: \`$TOTAL_DURATION\`"
    echo "- Root: \`$ROOT_DIR\`"
    echo "- Runs requested: \`$RUNS\`"
    echo "- Result: \`$([[ "$overall_status" -eq 0 ]] && echo PASS || echo FAIL)\`"
    echo "- Passed: \`$pass_count\`"
    echo "- Failed: \`$fail_count\`"
    echo "- Logs: \`$LOG_DIR\`"
    echo
    echo "## Runs"
    echo
    echo "| Run | Result | Duration | Log |"
    echo "|---:|---:|---:|---|"
    for idx in "${!RUN_STATUSES[@]}"; do
        run_number="$((idx + 1))"
        status="${RUN_STATUSES[$idx]}"
        duration="${RUN_DURATIONS[$idx]}"
        log_file="${RUN_LOGS[$idx]}"
        echo "| \`$run_number\` | $status | \`$duration\` | \`$(markdown_escape "$log_file")\` |"
    done
    echo

    if [[ "$fail_count" -gt 0 ]]; then
        echo "## Failure Tails"
        echo
        for idx in "${!RUN_STATUSES[@]}"; do
            if [[ "${RUN_STATUSES[$idx]}" != "FAIL" ]]; then
                continue
            fi
            run_number="$((idx + 1))"
            log_file="${RUN_LOGS[$idx]}"
            echo "### Run \`$run_number\`"
            echo
            echo '```text'
            tail -100 "$log_file" || true
            echo '```'
            echo
        done
    fi

    echo "## Re-run"
    echo
    echo '```bash'
    echo "DEXTER_BROWSER_RECOVERY_MODEL_STABILITY_RUNS=$RUNS make live-smoke-browser-recovery-model-stability"
    echo '```'
} > "$SUMMARY_FILE"

cp "$SUMMARY_FILE" "$LATEST_FILE"

say INFO "summary written: $SUMMARY_FILE"
say INFO "latest summary: $LATEST_FILE"

exit "$overall_status"
