#!/usr/bin/env bash
# Verify the Swift HUD can create a local diagnostic bundle without terminal use.

set -u

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SWIFT_DIR="$ROOT_DIR/src/swift"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dexter-hud-diagnostic-bundle.XXXXXX")"
SWIFT_LOG="/tmp/dexter-hud-diagnostic-bundle-smoke.log"
SWIFT_PID=""

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

cleanup() {
    if [[ -n "$SWIFT_PID" ]]; then
        kill "$SWIFT_PID" >/dev/null 2>&1 || true
        wait "$SWIFT_PID" >/dev/null 2>&1 || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

wait_for_pattern() {
    local file="$1"
    local pattern="$2"
    local timeout_secs="$3"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if grep -Fq "$pattern" "$file"; then
            return 0
        fi
        if [[ -n "$SWIFT_PID" ]] && ! kill -0 "$SWIFT_PID" >/dev/null 2>&1; then
            grep -Fq "$pattern" "$file" && return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

assert_contains() {
    local label="$1"
    local file="$2"
    local pattern="$3"
    if ! grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - missing pattern: $pattern"
        return 1
    fi
    return 0
}

assert_absent() {
    local label="$1"
    local file="$2"
    local pattern="$3"
    if grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - unexpected pattern: $pattern"
        return 1
    fi
    return 0
}

start_swift_smoke() {
    : > "$SWIFT_LOG"
    say "$INFO" "starting Swift HUD diagnostic-bundle smoke; log: $SWIFT_LOG"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_DIAGNOSTIC_BUNDLE=1 \
        DEXTER_HUD_SMOKE_SKIP_VOICE_CAPTURE=1 \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS=1 \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=10 \
        DEXTER_DIAGNOSTIC_DIR="$TMP_DIR" \
            swift run
    ) >> "$SWIFT_LOG" 2>&1 &
    SWIFT_PID="$!"
}

main() {
    local ok=0
    local label="Swift HUD diagnostic-bundle smoke"

    start_swift_smoke
    wait_for_pattern "$SWIFT_LOG" "[HUDSmoke] showDiagnosticBundleResult" 30 || {
        say "$FAIL" "$label - HUD did not render diagnostic bundle result"
        tail -140 "$SWIFT_LOG" || true
        exit 1
    }

    wait "$SWIFT_PID" >/dev/null 2>&1 || true
    SWIFT_PID=""

    assert_contains "$label" "$SWIFT_LOG" "[HUDSmoke] diagnosticBundleRequest" || ok=1
    assert_contains "$label" "$SWIFT_LOG" "### Diagnostic Bundle" || ok=1
    assert_contains "$label" "$SWIFT_LOG" "Status: created" || ok=1
    assert_contains "$label" "$SWIFT_LOG" "Report: \`$TMP_DIR/dexter-diagnostic-" || ok=1
    assert_contains "$label" "$SWIFT_LOG" "Latest: \`$TMP_DIR/latest.md\`" || ok=1
    assert_absent "$label" "$SWIFT_LOG" "Fatal error" || ok=1

    if [[ ! -f "$TMP_DIR/latest.md" ]]; then
        say "$FAIL" "$label - latest diagnostic report was not created"
        ok=1
    fi
    if ! find "$TMP_DIR" -maxdepth 1 -type f -name 'dexter-diagnostic-*.md' | grep -q .; then
        say "$FAIL" "$label - timestamped diagnostic report was not created"
        ok=1
    fi

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$label passed"
        return 0
    fi

    tail -140 "$SWIFT_LOG" || true
    return 1
}

main "$@"
