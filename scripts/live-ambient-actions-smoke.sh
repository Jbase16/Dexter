#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ambient-actions-core.log"
SAFE_OUT="/tmp/dexter-ambient-actions-safe.out"
FAILED_OUT="/tmp/dexter-ambient-actions-failed.out"
DENIED_OUT="/tmp/dexter-ambient-actions-denied.out"
EVENTS_OUT="/tmp/dexter-ambient-actions-events.out"
SOCKET="/tmp/dexter.sock"
DELETE_SENTINEL="/tmp/dexter-ambient-action-deny-$$"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    if [[ -f "$EVENTS_OUT" ]]; then
        say INFO "ambient events output:"
        cat "$EVENTS_OUT" || true
    fi
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
    python3 - "$@" <<'PY'
import json
import sys

mode = sys.argv[1]
if mode == "safe":
    token = sys.argv[2]
    print(json.dumps({
        "type": "shell",
        "args": ["echo", token],
        "rationale": "ambient action smoke safe"
    }))
elif mode == "failed":
    token = sys.argv[2]
    print(json.dumps({
        "type": "shell",
        "args": ["false", token],
        "rationale": "ambient action smoke failed"
    }))
elif mode == "denied":
    path = sys.argv[2]
    print(json.dumps({
        "type": "shell",
        "args": ["rm", "-rf", path],
        "rationale": "ambient action smoke denied",
        "category_override": "destructive"
    }))
else:
    raise SystemExit(f"unknown mode: {mode}")
PY
}

cleanup() {
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
    rm -rf "$DELETE_SENTINEL"
}
trap cleanup EXIT

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$SAFE_OUT" "$FAILED_OUT" "$DENIED_OUT" "$EVENTS_OUT"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

safe_token="AMBIENT_ACTION_SAFE_$(date +%s)_$$"
failed_token="AMBIENT_ACTION_FAILED_$(date +%s)_$$"
mkdir -p "$DELETE_SENTINEL"

safe_action="$(json_action safe "$safe_token")"
failed_action="$(json_action failed "$failed_token")"
denied_action="$(json_action denied "$DELETE_SENTINEL")"

say INFO "driving safe synthetic action"
"$CLI_BIN" --quiet --idle-timeout 180 --action-json "$safe_action" >"$SAFE_OUT" 2>&1 \
    || fail "safe synthetic action failed"

say INFO "driving failing synthetic action"
"$CLI_BIN" --quiet --idle-timeout 180 --action-json "$failed_action" >"$FAILED_OUT" 2>&1 \
    || fail "failing synthetic action did not return cleanly to CLI"

say INFO "driving auto-denied destructive synthetic action"
"$CLI_BIN" --auto-deny --idle-timeout 180 --action-json "$denied_action" >"$DENIED_OUT" 2>&1 \
    || fail "denied synthetic action failed"

if [[ ! -d "$DELETE_SENTINEL" ]]; then
    fail "auto-denied destructive action unexpectedly removed $DELETE_SENTINEL"
fi

"$CLI_BIN" --events --limit 80 >"$EVENTS_OUT"

grep -Fq "action_succeeded" "$EVENTS_OUT" \
    || fail "ambient event queue did not include action_succeeded"
grep -Fq "$safe_token" "$EVENTS_OUT" \
    || fail "ambient action success event did not mention safe token"
grep -Fq "action_failed" "$EVENTS_OUT" \
    || fail "ambient event queue did not include action_failed"
grep -Fq "$failed_token" "$EVENTS_OUT" \
    || fail "ambient action failure event did not mention failed token"
grep -Fq "trigger_matched" "$EVENTS_OUT" \
    || fail "ambient event queue did not include trigger_matched for default action failure trigger"
grep -Fq "Dexter action failures" "$EVENTS_OUT" \
    || fail "default action failure trigger did not match failed action"
grep -Fq "action_approval_requested" "$EVENTS_OUT" \
    || fail "ambient event queue did not include action_approval_requested"
grep -Fq "$DELETE_SENTINEL" "$EVENTS_OUT" \
    || fail "ambient action approval event did not mention denied sentinel path"
grep -Fq "action_denied" "$EVENTS_OUT" \
    || fail "ambient event queue did not include action_denied"

say PASS "ambient action smoke passed"
