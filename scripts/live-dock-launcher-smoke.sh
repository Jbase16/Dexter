#!/usr/bin/env bash
# scripts/live-dock-launcher-smoke.sh - validate the Dock-launchable wrapper without opening Terminal.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/dexter-dock-launcher-smoke.XXXXXX")"
APP_PATH="$TMP_ROOT/Dexter.app"
INFO_PLIST="$APP_PATH/Contents/Info.plist"
LAUNCHER="$APP_PATH/Contents/MacOS/DexterLauncher"
RESTART_SCRIPT="$ROOT_DIR/scripts/restart-dexter-ui.sh"
APP_SWIFT="$ROOT_DIR/src/swift/Sources/Dexter/App.swift"

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
assert_file "$RESTART_SCRIPT" "restart lifecycle script"
assert_executable "$RESTART_SCRIPT" "restart lifecycle script"

plutil -lint "$INFO_PLIST" >/dev/null
pass "Info.plist is valid"

/bin/zsh -n "$LAUNCHER"
pass "launcher shell syntax is valid"
bash -n "$RESTART_SCRIPT"
pass "restart lifecycle script syntax is valid"

APPLESCRIPT="$TMP_ROOT/DexterLauncher.applescript"
awk '
    /^osascript <<OSA$/ { capture = 1; next }
    /^OSA$/ { capture = 0 }
    capture { print }
' "$LAUNCHER" > "$APPLESCRIPT"
[[ -s "$APPLESCRIPT" ]] || fail "launcher AppleScript heredoc was not extracted"
osacompile -o "$TMP_ROOT/DexterLauncher.scpt" "$APPLESCRIPT" >/dev/null
pass "launcher AppleScript syntax is valid"

assert_plist_value "CFBundleExecutable" "DexterLauncher"
assert_plist_value "CFBundleIdentifier" "com.jason.dexter.launcher"
assert_plist_value "CFBundleName" "Dexter"
assert_plist_value "CFBundlePackageType" "APPL"
assert_plist_value "LSUIElement" "false"

assert_contains "$LAUNCHER" "set repoPath to \"$ROOT_DIR\"" "launcher embeds current repo path"
assert_contains "$LAUNCHER" "set appPath to \"$APP_PATH\"" "launcher embeds actual app path"
assert_contains "$LAUNCHER" "export OLLAMA_MODELS=/Users/jason/ollama-models" "launcher exports local model store"
assert_contains "$LAUNCHER" "DEXTER_STARTED_FROM" "launcher passes app path to lifecycle script"
assert_contains "$LAUNCHER" "scripts/restart-dexter-ui.sh" "launcher uses centralized Terminal lifecycle script"
assert_contains "$LAUNCHER" "Dexter Live Logs" "launcher sets live-log terminal title"
assert_contains "$RESTART_SCRIPT" "Use Dexter > Restart Dexter" "lifecycle script prints restart guidance"
assert_contains "$RESTART_SCRIPT" "Use Dexter > Quit Dexter" "lifecycle script prints quit guidance"
assert_contains "$APP_SWIFT" "scripts/restart-dexter-ui.sh" "Swift restart handler launches lifecycle script"
assert_contains "$RESTART_SCRIPT" "make restart-core" "restart lifecycle script uses detached core restart"
assert_contains "$RESTART_SCRIPT" "make run-swift" "restart lifecycle script starts Swift after core readiness"

pass "Dock launcher smoke passed"
