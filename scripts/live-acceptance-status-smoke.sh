#!/usr/bin/env bash
# Verify acceptance-status parsing against isolated fake live-smoke receipts.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dexter-acceptance-status.XXXXXX")"
EMPTY_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dexter-acceptance-empty.XXXXXX")"

cleanup() {
    rm -rf "$TMP_DIR" "$EMPTY_DIR"
}
trap cleanup EXIT

fail() {
    printf '[FAIL] %s\n' "$1" >&2
    exit 1
}

pass() {
    printf '[PASS] %s\n' "$1"
}

require_contains() {
    local file="$1"
    local pattern="$2"
    local message="$3"

    if ! rg -q -- "$pattern" "$file"; then
        printf '[DEBUG] missing pattern: %s\n' "$pattern" >&2
        cat "$file" >&2 || true
        fail "$message"
    fi
}

write_summary() {
    local file="$1"
    local started="$2"
    shift 2

    {
        printf '# Dexter Live Smoke Summary\n\n'
        printf -- '- Started: `%s`\n' "$started"
        printf -- '- Finished: `%s`\n' "$started"
        printf -- '- Duration: `1s`\n'
        printf -- '- Root: `%s`\n' "$ROOT_DIR"
        printf -- '- Result: `PASS`\n'
        printf -- '- Passed: `%s`\n' "$#"
        printf -- '- Failed: `0`\n\n'
        printf '## Targets\n\n'
        printf '| Target | Result | Duration | Log |\n'
        printf '|---|---:|---:|---|\n'
        for target in "$@"; do
            printf '| `%s` | PASS | `1s` | `/tmp/%s.log` |\n' "$target" "$target"
        done
    } > "$file"
}

write_summary "$TMP_DIR/live-smoke-20260609_010000.md" "2026-06-09T01:00:00-0700" \
    live-smoke-dock-launcher \
    live-smoke-process-control \
    live-smoke-stop-report \
    live-smoke-run-loop-lifecycle \
    live-smoke-stale-swift-stop \
    live-smoke-hud-lifecycle \
    live-smoke-hud-placement \
    live-smoke-placement-command

write_summary "$TMP_DIR/live-smoke-20260609_020000.md" "2026-06-09T02:00:00-0700" \
    live-smoke-residency-proof \
    live-smoke-startup-readiness \
    live-smoke-operator-status \
    live-smoke-hud-health \
    live-smoke-hud-unavailable-health

write_summary "$TMP_DIR/live-smoke-20260609_030000.md" "2026-06-09T03:00:00-0700" \
    live-smoke-external-failures \
    live-smoke-action-diagnostic \
    live-smoke-shortcut-action \
    live-smoke-window-focus \
    live-smoke-window-inspect \
    live-smoke-ui-snapshot \
    live-smoke-ui-click \
    live-smoke-ui-type \
    live-smoke-ui-select \
    live-smoke-ui-toggle \
    live-smoke-ui-pick \
    live-smoke-ui-failure-diagnostic \
    live-smoke-action-matrix \
    live-smoke-browser-recovery \
    live-smoke-action-receipts \
    live-smoke-approval-lifecycle \
    live-smoke-action-cancel

write_summary "$TMP_DIR/live-smoke-20260609_040000.md" "2026-06-09T04:00:00-0700" \
    live-smoke-dock-launcher \
    live-smoke-process-control \
    live-smoke-stop-report \
    live-smoke-run-loop-lifecycle \
    live-smoke-stale-swift-stop \
    live-smoke-hud-lifecycle \
    live-smoke-hud-placement \
    live-smoke-placement-command \
    live-smoke-residency-proof \
    live-smoke-startup-readiness \
    live-smoke-operator-status \
    live-smoke-hud-health \
    live-smoke-hud-unavailable-health \
    live-smoke-external-failures \
    live-smoke-action-diagnostic \
    live-smoke-shortcut-action \
    live-smoke-window-focus \
    live-smoke-window-inspect \
    live-smoke-ui-snapshot \
    live-smoke-ui-click \
    live-smoke-ui-type \
    live-smoke-ui-select \
    live-smoke-ui-toggle \
    live-smoke-ui-pick \
    live-smoke-ui-failure-diagnostic \
    live-smoke-action-matrix \
    live-smoke-browser-recovery \
    live-smoke-action-receipts \
    live-smoke-approval-lifecycle \
    live-smoke-hud-action-history \
    live-smoke-hud-action-diagnostic \
    live-smoke-hud-ui-failure \
    live-smoke-hud-approval \
    live-smoke-action-cancel

OUT="$(mktemp "${TMPDIR:-/tmp}/dexter-acceptance-status.out.XXXXXX")"
EMPTY_OUT="$(mktemp "${TMPDIR:-/tmp}/dexter-acceptance-status-empty.out.XXXXXX")"

DEXTER_SMOKE_SUMMARY_DIR="$TMP_DIR" DEXTER_ACCEPTANCE_STRICT=1 "$ROOT_DIR/scripts/acceptance-status.sh" > "$OUT"

require_contains "$OUT" '# Dexter Acceptance Status' "acceptance status missing title"
require_contains "$OUT" 'Main acceptance battery | PASS' "main acceptance battery did not pass"
require_contains "$OUT" 'Operator controls | PASS' "operator controls slice did not pass"
require_contains "$OUT" 'Runtime health | PASS' "runtime health slice did not pass"
require_contains "$OUT" 'Action safety | PASS' "action safety slice did not pass"
require_contains "$OUT" 'make live-smoke-acceptance' "main acceptance command missing"
require_contains "$OUT" 'make live-smoke-operator-controls' "operator controls command missing"
require_contains "$OUT" 'make live-smoke-runtime-health' "runtime health command missing"
require_contains "$OUT" 'make live-smoke-action-safety' "action safety command missing"
require_contains "$OUT" 'live-smoke-residency-proof' "residency proof target missing"

DEXTER_SMOKE_SUMMARY_DIR="$EMPTY_DIR" "$ROOT_DIR/scripts/acceptance-status.sh" > "$EMPTY_OUT"
require_contains "$EMPTY_OUT" 'Operator controls | MISSING' "empty non-strict run should report missing operator controls"

if DEXTER_SMOKE_SUMMARY_DIR="$EMPTY_DIR" DEXTER_ACCEPTANCE_STRICT=1 "$ROOT_DIR/scripts/acceptance-status.sh" >/tmp/dexter-acceptance-strict-empty.out 2>&1; then
    fail "strict acceptance status should fail when evidence is missing"
fi

rm -f "$OUT" "$EMPTY_OUT" /tmp/dexter-acceptance-strict-empty.out
pass "acceptance status smoke passed"
