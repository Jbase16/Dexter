#!/usr/bin/env bash
# Verify the consolidated operator readiness command and its concrete side effects.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_PATH="$HOME/Applications/Dexter.app"
LAUNCHER="$APP_PATH/Contents/MacOS/DexterLauncher"
READY_OUT="/tmp/dexter-operator-ready-smoke.out"

pass() {
    printf '[PASS] %s\n' "$1"
}

fail() {
    printf '[FAIL] %s\n' "$1" >&2
    exit 1
}

assert_file() {
    local path="$1"
    local label="$2"
    [[ -f "$path" ]] || fail "$label missing at $path"
    pass "$label exists"
}

assert_executable() {
    local path="$1"
    local label="$2"
    [[ -x "$path" ]] || fail "$label is not executable at $path"
    pass "$label is executable"
}

assert_contains() {
    local path="$1"
    local needle="$2"
    local label="$3"
    grep -Fq -- "$needle" "$path" || fail "$label missing '$needle'"
    pass "$label"
}

assert_socket_absent() {
    local path="$1"
    [[ ! -e "$path" ]] || fail "stale socket still exists: $path"
    pass "socket absent: $path"
}

cd "$ROOT_DIR"
make operator-ready | tee "$READY_OUT"

assert_socket_absent /tmp/dexter.sock
assert_socket_absent /tmp/dexter-shell.sock
assert_file "$APP_PATH/Contents/Info.plist" "installed launcher Info.plist"
assert_file "$LAUNCHER" "installed launcher executable"
assert_executable "$LAUNCHER" "installed launcher executable"

/bin/zsh -n "$LAUNCHER"
pass "installed launcher shell syntax is valid"

assert_contains "$LAUNCHER" "export OLLAMA_MODELS=/Users/jason/ollama-models" "installed launcher exports local model store"
assert_contains "$LAUNCHER" "make configure-ollama-models && make stop && make run" "installed launcher reasserts models before run"
assert_contains "$READY_OUT" "make acceptance-status" "operator-ready prints acceptance status command"
assert_contains "$READY_OUT" "make acceptance-status-strict" "operator-ready prints strict acceptance status command"
assert_contains "$READY_OUT" "make live-smoke-acceptance" "operator-ready prints combined acceptance command"
assert_contains "$READY_OUT" "make live-smoke-runtime-health" "operator-ready prints runtime health acceptance command"
assert_contains "$READY_OUT" "make live-smoke-action-safety" "operator-ready prints action safety acceptance command"

launchctl_value="$(launchctl getenv OLLAMA_MODELS 2>/dev/null || true)"
[[ "$launchctl_value" == "/Users/jason/ollama-models" ]] \
    || fail "launchctl OLLAMA_MODELS expected /Users/jason/ollama-models but saw '${launchctl_value:-unset}'"
pass "launchctl OLLAMA_MODELS points at local runtime store"

pass "operator-ready smoke passed"
