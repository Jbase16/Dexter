#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ui-click-core.log"
ACTION_OUT="/tmp/dexter-ui-click.out"
RECENT_OUT="/tmp/dexter-ui-click-recent.out"
SMOKE_APP="/tmp/DexterUIClickSmoke.app"
SOCKET="/tmp/dexter.sock"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    for file in "$ACTION_OUT" "$RECENT_OUT"; do
        if [[ -f "$file" ]]; then
            say INFO "$file:"
            cat "$file" || true
        fi
    done
    if [[ -f "$CORE_LOG" ]]; then
        say INFO "core log tail:"
        tail -n 100 "$CORE_LOG" || true
    fi
    exit 1
}

socket_accepts() {
    python3 - "$SOCKET" <<'PY'
import socket
import sys

path = sys.argv[1]
s = socket.socket(socket.AF_UNIX)
s.settimeout(1)
sys.exit(0 if s.connect_ex(path) == 0 else 1)
PY
}

json_action() {
    python3 <<'PY'
import json

print(json.dumps({
    "type": "ui_click",
    "app_name": "DexterUIClickSmoke",
    "role": "AXButton",
    "label": "OK",
    "max_depth": 3,
    "rationale": "UI_CLICK_SMOKE press a temporary dialog OK button"
}))
PY
}

cleanup() {
    pkill -f DexterUIClickSmoke >/dev/null 2>&1 || true
    rm -rf "$SMOKE_APP"
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$ACTION_OUT" "$RECENT_OUT"
rm -rf "$SMOKE_APP"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

say INFO "creating temporary click fixture app"
osacompile -o "$SMOKE_APP" \
    -e 'display dialog "Dexter UI Click Smoke" buttons {"OK"} default button "OK" giving up after 30' \
    >/dev/null
open -n "$SMOKE_APP"

say INFO "waiting for temporary app to expose Accessibility elements"
for _ in {1..40}; do
    if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUIClickSmoke"
    if (count of matchingProcesses) is 0 then error "not running" number 1728
    set targetProcess to item 1 of matchingProcesses
    if (count of UI elements of targetProcess) is 0 then error "not exposed" number 1728
end tell
APPLESCRIPT
    then
        break
    fi
    sleep 0.25
done

say INFO "driving ui_click action against temporary OK button"
"$CLI_BIN" --idle-timeout 180 --action-json "$(json_action)" >"$ACTION_OUT" 2>&1 \
    || fail "ui_click action did not return cleanly to CLI"

grep -Fq "[ACTION RECEIPT" "$ACTION_OUT" \
    || fail "ui_click action did not emit a receipt"
grep -Fq "ui_click" "$ACTION_OUT" \
    || fail "ui_click action type was not surfaced"
grep -Fq "outcome=executed" "$ACTION_OUT" \
    || fail "ui_click action did not execute"
grep -Fq "Succeeded: pressed UI control:" "$ACTION_OUT" \
    || fail "ui_click action did not report the pressed control"
grep -Fq "app: DexterUIClickSmoke" "$ACTION_OUT" \
    || fail "ui_click action did not target the temporary app"
if grep -Fq "approval required" "$ACTION_OUT"; then
    fail "ordinary ui_click unexpectedly required approval"
fi

"$CLI_BIN" --actions recent --limit 20 >"$RECENT_OUT"
grep -Fq "ui_click" "$RECENT_OUT" \
    || fail "recent action receipts did not include ui_click action type"
grep -Fq "UI click: DexterUIClickSmoke AXButton \"OK\"" "$RECENT_OUT" \
    || fail "recent action receipts did not show readable UI click target"
grep -Fq "EXECUTED" "$RECENT_OUT" \
    || fail "recent action receipts did not record execution"
grep -Fq "Succeeded: pressed UI control:" "$RECENT_OUT" \
    || fail "recent action receipts did not record pressed control"

if pgrep -f DexterUIClickSmoke >/dev/null 2>&1; then
    fail "temporary dialog app was still running after ui_click"
fi

say PASS "ui_click action smoke passed"
