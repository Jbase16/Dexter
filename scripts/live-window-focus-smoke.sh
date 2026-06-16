#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-window-focus-core.log"
ACTION_OUT="/tmp/dexter-window-focus.out"
RECENT_OUT="/tmp/dexter-window-focus-recent.out"
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
        tail -n 80 "$CORE_LOG" || true
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
    "type": "window_focus",
    "app_name": "Finder",
    "title_contains": None,
    "rationale": "WINDOW_FOCUS_SMOKE target acquisition"
}))
PY
}

cleanup() {
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$ACTION_OUT" "$RECENT_OUT"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

say INFO "driving Finder window_focus action"
"$CLI_BIN" --idle-timeout 180 --action-json "$(json_action)" >"$ACTION_OUT" 2>&1 \
    || fail "window_focus action did not return cleanly to CLI"

grep -Fq "[ACTION RECEIPT" "$ACTION_OUT" \
    || fail "window_focus action did not emit a receipt"
grep -Fq "window_focus" "$ACTION_OUT" \
    || fail "window_focus action type was not surfaced"
grep -Fq "outcome=executed" "$ACTION_OUT" \
    || fail "window_focus action did not execute"
grep -Fq "Succeeded: focused Finder" "$ACTION_OUT" \
    || fail "window_focus action did not succeed"
if grep -Fq "approval required" "$ACTION_OUT"; then
    fail "window_focus unexpectedly required approval"
fi

"$CLI_BIN" --actions recent --limit 20 >"$RECENT_OUT"
grep -Fq "window_focus" "$RECENT_OUT" \
    || fail "recent action receipts did not include window_focus action type"
grep -Fq "App: Finder" "$RECENT_OUT" \
    || fail "recent action receipts did not show readable Finder target"
grep -Fq "EXECUTED" "$RECENT_OUT" \
    || fail "recent action receipts did not record execution"
grep -Fq "Succeeded: focused Finder" "$RECENT_OUT" \
    || fail "recent action receipts did not record success"

say PASS "window_focus action smoke passed"
