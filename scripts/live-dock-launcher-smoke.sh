#!/usr/bin/env bash
# scripts/live-dock-launcher-smoke.sh - validate the Dock-launchable wrapper without opening Terminal.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/dexter-dock-launcher-smoke.XXXXXX")"
APP_PATH="$TMP_ROOT/Dexter.app"
INFO_PLIST="$APP_PATH/Contents/Info.plist"
LAUNCHER="$APP_PATH/Contents/MacOS/DexterLauncher"

cleanup() {
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

pass() {
    printf '[PASS] %s\n' "$1"
}

fail() {
    printf '[FAIL] %s\n' "$1" >&2
    exit 1
}

assert_file() {
    local path="$1"
    local description="$2"
    [[ -f "$path" ]] || fail "$description missing at $path"
    pass "$description exists"
}

assert_executable() {
    local path="$1"
    local description="$2"
    [[ -x "$path" ]] || fail "$description is not executable at $path"
    pass "$description is executable"
}

assert_plist_value() {
    local key="$1"
    local expected="$2"
    local actual
    actual="$(/usr/libexec/PlistBuddy -c "Print :$key" "$INFO_PLIST" 2>/dev/null || true)"
    [[ "$actual" == "$expected" ]] || fail "Info.plist $key expected '$expected' but saw '$actual'"
    pass "Info.plist $key=$expected"
}

assert_contains() {
    local path="$1"
    local needle="$2"
    local description="$3"
    grep -Fq -- "$needle" "$path" || fail "$description missing '$needle'"
    pass "$description"
}

cd "$ROOT_DIR"
bash scripts/install-dexter-app.sh "$APP_PATH" >/tmp/dexter-dock-launcher-install.out

assert_file "$INFO_PLIST" "launcher Info.plist"
assert_file "$LAUNCHER" "launcher executable"
assert_executable "$LAUNCHER" "launcher executable"

plutil -lint "$INFO_PLIST" >/dev/null
pass "Info.plist is valid"

/bin/zsh -n "$LAUNCHER"
pass "launcher shell syntax is valid"

assert_plist_value "CFBundleExecutable" "DexterLauncher"
assert_plist_value "CFBundleIdentifier" "com.jason.dexter.launcher"
assert_plist_value "CFBundleName" "Dexter"
assert_plist_value "CFBundlePackageType" "APPL"
assert_plist_value "LSUIElement" "false"

assert_contains "$LAUNCHER" "set repoPath to \"$ROOT_DIR\"" "launcher embeds current repo path"
assert_contains "$LAUNCHER" "set appPath to \"$APP_PATH\"" "launcher embeds actual app path"
assert_contains "$LAUNCHER" "export OLLAMA_MODELS=/Users/jason/ollama-models" "launcher exports local model store"
assert_contains "$LAUNCHER" "make configure-ollama-models && make stop && make run" "launcher configures models before terminal-backed run loop"
assert_contains "$LAUNCHER" "Dexter Live Logs" "launcher sets live-log terminal title"
assert_contains "$LAUNCHER" "Use Dexter > Restart Dexter" "launcher prints restart guidance"
assert_contains "$LAUNCHER" "Use Dexter > Quit Dexter" "launcher prints quit guidance"

pass "Dock launcher smoke passed"
