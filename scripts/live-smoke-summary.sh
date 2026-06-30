#!/usr/bin/env bash
# scripts/live-smoke-summary.sh - run live-smoke targets and write a markdown receipt.
#
# Usage:
#   scripts/live-smoke-summary.sh
#   scripts/live-smoke-summary.sh --fail-fast
#   scripts/live-smoke-summary.sh live-smoke-action-diagnostic live-smoke-operator-status
#   DEXTER_SMOKE_SUMMARY_TARGETS="live-smoke-cli live-smoke-hud" scripts/live-smoke-summary.sh
#
# By default the runner intentionally continues after a failing target so the
# operator gets one complete artifact for the whole attempted pass. Use
# --fail-fast or DEXTER_SMOKE_SUMMARY_FAIL_FAST=1 for interactive passes where
# the first failure should stop the suite.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUMMARY_DIR="${DEXTER_SMOKE_SUMMARY_DIR:-$ROOT_DIR/docs/live-smoke-results}"
STAMP="$(date '+%Y%m%d_%H%M%S')"
SUMMARY_FILE="$SUMMARY_DIR/live-smoke-$STAMP.md"
LATEST_FILE="$SUMMARY_DIR/latest.md"
INDEX_FILE="$SUMMARY_DIR/index.md"
LOG_DIR="$SUMMARY_DIR/logs/$STAMP"

DEFAULT_TARGETS=(
    live-smoke-startup-readiness
    live-smoke-process-control
    live-smoke-stop-report
    live-smoke-run-loop-lifecycle
    live-smoke-stale-swift-stop
    live-smoke-operator-ready
    live-smoke-diagnostic-bundle
    live-smoke-dock-launcher
    live-smoke-recovery
    live-smoke-degraded-mode
    live-smoke-external-failures
    live-smoke-operator-status
    live-smoke-action-diagnostic
    live-smoke-shortcut-action
    live-smoke-window-focus
    live-smoke-window-inspect
    live-smoke-ui-snapshot
    live-smoke-ui-click
    live-smoke-ui-type
    live-smoke-ui-select
    live-smoke-ui-toggle
    live-smoke-ui-pick
    live-smoke-ui-failure-diagnostic
    live-smoke-cli
    live-smoke-action-matrix
    live-smoke-action-receipts
    live-smoke-approval-lifecycle
    live-smoke-hud
    live-smoke-hud-new-session
    live-smoke-hud-lifecycle
    live-smoke-hud-placement
    live-smoke-placement-command
    live-smoke-hud-health
    live-smoke-hud-unavailable-health
    live-smoke-hud-action-history
    live-smoke-hud-action-diagnostic
    live-smoke-hud-ui-failure
    live-smoke-hud-approval
    live-smoke-action-cancel
    live-smoke-barge-in
)

FAIL_FAST=0
SUMMARY_STOP_REASON="completed"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

markdown_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//|/\\|}"
    value="${value//$'\n'/ }"
    printf '%s' "$value"
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

targets_from_env() {
    if [[ -n "${DEXTER_SMOKE_SUMMARY_TARGETS:-}" ]]; then
        # shellcheck disable=SC2206
        TARGETS=($DEXTER_SMOKE_SUMMARY_TARGETS)
    else
        TARGETS=("${DEFAULT_TARGETS[@]}")
    fi
}

rebuild_summary_index() {
    python3 - "$SUMMARY_DIR" "$INDEX_FILE" "$ROOT_DIR" <<'PY'
from __future__ import annotations

import datetime as dt
import pathlib
import re
import sys

summary_dir = pathlib.Path(sys.argv[1])
index_file = pathlib.Path(sys.argv[2])
root_dir = pathlib.Path(sys.argv[3])
max_entries = 20

def field(text: str, name: str, default: str = "") -> str:
    match = re.search(rf"^- {re.escape(name)}: `([^`]*)`", text, re.MULTILINE)
    return match.group(1).strip() if match else default

def relative(path: pathlib.Path) -> str:
    try:
        return str(path.relative_to(root_dir))
    except ValueError:
        return str(path)

def table_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace("|", "\\|").replace("\n", " ")

def target_names(text: str) -> list[str]:
    targets: list[str] = []
    for line in text.splitlines():
        match = re.match(r"^\| `([^`]+)` \| (?:PASS|FAIL) \|", line)
        if match:
            targets.append(match.group(1))
    return targets

summaries = sorted(summary_dir.glob("live-smoke-*.md"), reverse=True)
rows: list[tuple[pathlib.Path, str, str, str, str, str, list[str]]] = []
for path in summaries[:max_entries]:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        continue
    rows.append((
        path,
        field(text, "Started", "unknown"),
        field(text, "Result", "unknown"),
        field(text, "Passed", "0"),
        field(text, "Failed", "0"),
        field(text, "Duration", "unknown"),
        target_names(text),
    ))

lines = [
    "# Dexter Live Smoke Index",
    "",
    f"- Generated: `{dt.datetime.now().astimezone().strftime('%Y-%m-%dT%H:%M:%S%z')}`",
    f"- Root: `{root_dir}`",
    f"- Entries: `{len(rows)}`",
    "",
    "| Summary | Started | Result | Passed | Failed | Duration | Targets |",
    "|---|---:|---:|---:|---:|---:|---|",
]

for path, started, result, passed, failed, duration, targets in rows:
    target_text = ", ".join(f"`{target}`" for target in targets) if targets else "none"
    lines.append(
        "| "
        + f"`{table_escape(relative(path))}` | "
        + f"`{table_escape(started)}` | "
        + f"{table_escape(result)} | "
        + f"`{table_escape(passed)}` | "
        + f"`{table_escape(failed)}` | "
        + f"`{table_escape(duration)}` | "
        + f"{target_text} |"
    )

lines.append("")
lines.append("## Latest")
lines.append("")
lines.append("```bash")
lines.append("sed -n '1,120p' docs/live-smoke-results/latest.md")
lines.append("```")
lines.append("")

index_file.write_text("\n".join(lines), encoding="utf-8")
PY
}

ARGS=()
for arg in "$@"; do
    case "$arg" in
        --fail-fast)
            FAIL_FAST=1
            ;;
        --continue-on-failure)
            FAIL_FAST=0
            ;;
        --)
            ;;
        *)
            ARGS+=("$arg")
            ;;
    esac
done

if [[ "${DEXTER_SMOKE_SUMMARY_FAIL_FAST:-}" == "1" || "${DEXTER_SMOKE_SUMMARY_FAIL_FAST:-}" == "true" ]]; then
    FAIL_FAST=1
fi

if [[ "${#ARGS[@]}" -gt 0 ]]; then
    TARGETS=("${ARGS[@]}")
else
    targets_from_env
fi

mkdir -p "$LOG_DIR"

STARTED_AT="$(date '+%Y-%m-%dT%H:%M:%S%z')"
START_EPOCH="$(date '+%s')"

TARGET_NAMES=()
TARGET_STATUSES=()
TARGET_DURATIONS=()
TARGET_LOGS=()

overall_status=0

say INFO "writing live smoke summary to $SUMMARY_FILE"
say INFO "logs will be stored in $LOG_DIR"
if [[ "$FAIL_FAST" -eq 1 ]]; then
    say INFO "fail-fast enabled; stopping after the first failing target"
fi

for target in "${TARGETS[@]}"; do
    target_start="$(date '+%s')"
    log_file="$LOG_DIR/$target.log"
    TARGET_NAMES+=("$target")
    TARGET_LOGS+=("$log_file")

    (
        cd "$ROOT_DIR" || exit 2
        bash scripts/stop-dexter.sh --quiet >/dev/null 2>&1 || true
    )

    say INFO "running make $target"
    (
        cd "$ROOT_DIR" || exit 2
        make "$target"
    ) 2>&1 | tee "$log_file"
    status="${PIPESTATUS[0]}"

    target_end="$(date '+%s')"
    duration="$((target_end - target_start))"
    TARGET_DURATIONS+=("$(duration_label "$duration")")

    if [[ "$status" -eq 0 ]]; then
        TARGET_STATUSES+=("PASS")
        say PASS "$target completed in $(duration_label "$duration")"
    else
        TARGET_STATUSES+=("FAIL")
        overall_status=1
        say FAIL "$target exited $status after $(duration_label "$duration")"
        if [[ "$FAIL_FAST" -eq 1 ]]; then
            SUMMARY_STOP_REASON="fail_fast_after_$target"
            say INFO "fail-fast stopping after $target"
            break
        fi
    fi
done

FINISHED_AT="$(date '+%Y-%m-%dT%H:%M:%S%z')"
END_EPOCH="$(date '+%s')"
TOTAL_DURATION="$(duration_label "$((END_EPOCH - START_EPOCH))")"

pass_count=0
fail_count=0
for status in "${TARGET_STATUSES[@]}"; do
    if [[ "$status" == "PASS" ]]; then
        pass_count=$((pass_count + 1))
    else
        fail_count=$((fail_count + 1))
    fi
done

{
    echo "# Dexter Live Smoke Summary"
    echo
    echo "- Started: \`$STARTED_AT\`"
    echo "- Finished: \`$FINISHED_AT\`"
    echo "- Duration: \`$TOTAL_DURATION\`"
    echo "- Root: \`$ROOT_DIR\`"
    echo "- Result: \`$([[ "$overall_status" -eq 0 ]] && echo PASS || echo FAIL)\`"
    echo "- Mode: \`$([[ "$FAIL_FAST" -eq 1 ]] && echo fail-fast || echo continue-on-failure)\`"
    echo "- Stop Reason: \`$SUMMARY_STOP_REASON\`"
    echo "- Passed: \`$pass_count\`"
    echo "- Failed: \`$fail_count\`"
    echo "- Logs: \`$LOG_DIR\`"
    echo
    echo "## Targets"
    echo
    echo "| Target | Result | Duration | Log |"
    echo "|---|---:|---:|---|"
    for idx in "${!TARGET_NAMES[@]}"; do
        name="${TARGET_NAMES[$idx]}"
        status="${TARGET_STATUSES[$idx]}"
        duration="${TARGET_DURATIONS[$idx]}"
        log_file="${TARGET_LOGS[$idx]}"
        echo "| \`$(markdown_escape "$name")\` | $status | \`$duration\` | \`$(markdown_escape "$log_file")\` |"
    done
    echo

    if [[ "$fail_count" -gt 0 ]]; then
        echo "## Failure Tails"
        echo
        for idx in "${!TARGET_NAMES[@]}"; do
            if [[ "${TARGET_STATUSES[$idx]}" != "FAIL" ]]; then
                continue
            fi
            name="${TARGET_NAMES[$idx]}"
            log_file="${TARGET_LOGS[$idx]}"
            echo "### \`$(markdown_escape "$name")\`"
            echo
            echo '```text'
            tail -80 "$log_file" || true
            echo '```'
            echo
        done
    fi

    echo "## Re-run"
    echo
    echo '```bash'
    printf 'bash scripts/live-smoke-summary.sh'
    if [[ "$FAIL_FAST" -eq 1 ]]; then
        printf ' --fail-fast'
    fi
    printf ' %q' "${TARGETS[@]}"
    echo
    echo '```'
} > "$SUMMARY_FILE"

cp "$SUMMARY_FILE" "$LATEST_FILE"
rebuild_summary_index

say INFO "summary written: $SUMMARY_FILE"
say INFO "latest summary: $LATEST_FILE"
say INFO "summary index: $INDEX_FILE"

exit "$overall_status"
