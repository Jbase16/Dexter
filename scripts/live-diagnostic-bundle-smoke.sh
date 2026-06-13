#!/usr/bin/env bash
# Verify the local diagnostic bundle can be generated from any cwd.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dexter-diagnostic-smoke.XXXXXX")"

cleanup() {
    rm -rf "$TMP_DIR"
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
        sed -n '1,220p' "$file" >&2 || true
        fail "$message"
    fi
}

cd /tmp
DEXTER_DIAGNOSTIC_DIR="$TMP_DIR" "$ROOT_DIR/scripts/diagnostic-bundle.sh" >/tmp/dexter-diagnostic-smoke.out

REPORT="$(find "$TMP_DIR" -maxdepth 1 -type f -name 'dexter-diagnostic-*.md' | head -1)"
LATEST="$TMP_DIR/latest.md"

[[ -n "$REPORT" ]] || fail "diagnostic report was not created"
[[ -f "$LATEST" ]] || fail "latest diagnostic report was not created"
cmp -s "$REPORT" "$LATEST" || fail "latest diagnostic report does not match timestamped report"

require_contains "$LATEST" '# Dexter Diagnostic Bundle' "diagnostic report missing title"
require_contains "$LATEST" 'Root: `/Users/jason/Developer/Dex`' "diagnostic report did not normalize to repo root"
require_contains "$LATEST" '## System' "diagnostic report missing system section"
require_contains "$LATEST" '## Dexter Processes' "diagnostic report missing process section"
require_contains "$LATEST" '## Dexter Sockets' "diagnostic report missing socket section"
require_contains "$LATEST" '## Ollama Environment' "diagnostic report missing Ollama environment section"
require_contains "$LATEST" '## Model Stores' "diagnostic report missing model store section"
require_contains "$LATEST" '## Disk' "diagnostic report missing disk section"
require_contains "$LATEST" '## Dock Launcher' "diagnostic report missing Dock launcher section"
require_contains "$LATEST" '## Dock Launcher Command' "diagnostic report missing Dock launcher command section"
require_contains "$LATEST" 'OLLAMA_MODELS=/Users/jason/ollama-models' "diagnostic report missing launcher model-store export"
require_contains "$LATEST" 'make configure-ollama-models && make stop && make run' "diagnostic report missing launcher startup command"
require_contains "$LATEST" '## Doctor' "diagnostic report missing doctor section"
require_contains "$LATEST" '## Operator Status' "diagnostic report missing operator status section"
require_contains "$LATEST" 'Skipped by default' "operator status should be skipped by default"
require_contains "$LATEST" '## Latest Live Smoke Summary' "diagnostic report missing live-smoke summary pointer"
require_contains "$LATEST" '## Recent Live Smoke Index' "diagnostic report missing live-smoke index"
require_contains "$LATEST" '## Acceptance Status' "diagnostic report missing acceptance status"
require_contains "$LATEST" '# Dexter Acceptance Status' "diagnostic report missing acceptance status output"
require_contains "$LATEST" 'Main acceptance battery' "diagnostic report missing main acceptance status row"

if rg -q -- 'ps axww.*rg .*dexter-core' "$LATEST"; then
    fail "process probe appears to include its own rg command"
fi

if rg -q -- "sed -n '1,120p' docs/live-smoke-results/latest.md" "$LATEST"; then
    fail "diagnostic report embedded the live-smoke index helper block"
fi

pass "diagnostic bundle smoke passed"
