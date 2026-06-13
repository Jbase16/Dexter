#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ambient-trigger-actions-core.log"
ACTION_OUT="/tmp/dexter-ambient-trigger-actions-action.out"
EVENTS_OUT="/tmp/dexter-ambient-trigger-actions-events.out"
INBOX_OUT="/tmp/dexter-ambient-trigger-actions-inbox.out"
SOCKET="/tmp/dexter.sock"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    for file in "$EVENTS_OUT" "$INBOX_OUT" "$ACTION_OUT"; do
        if [[ -f "$file" ]]; then
            say INFO "$(basename "$file"):"
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
    python3 - "$SOCKET" <<'PY' >/dev/null 2>&1
import socket
import sys

path = sys.argv[1]
s = socket.socket(socket.AF_UNIX)
s.settimeout(1)
sys.exit(0 if s.connect_ex(path) == 0 else 1)
PY
}

json_action() {
    python3 - "$1" <<'PY'
import json
import sys

token = sys.argv[1]
print(json.dumps({
    "type": "shell",
    "args": ["false", token],
    "rationale": "ambient trigger actions smoke failed"
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

rm -f "$CORE_LOG" "$ACTION_OUT" "$EVENTS_OUT" "$INBOX_OUT"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

stamp="$(date +%s)_$$"
token="AMBIENT_TRIGGER_ACTIONS_FAILED_$stamp"
approval_trigger="Smoke ask approval $stamp"
task_trigger="Smoke start task $stamp"

say INFO "adding ask-approval and start-task triggers"
"$CLI_BIN" --add-trigger "$approval_trigger" \
    --event-kind action_failed \
    --min-severity warn \
    --trigger-action ask_approval >/dev/null
"$CLI_BIN" --add-trigger "$task_trigger" \
    --event-kind action_failed \
    --min-severity warn \
    --trigger-action start_task >/dev/null

say INFO "driving failed synthetic action"
"$CLI_BIN" --quiet --idle-timeout 180 --action-json "$(json_action "$token")" >"$ACTION_OUT" 2>&1 \
    || fail "failed synthetic action did not return cleanly to CLI"

"$CLI_BIN" --events --limit 120 >"$EVENTS_OUT"
"$CLI_BIN" --inbox --limit 120 >"$INBOX_OUT"

grep -Fq "trigger_action_approval_requested" "$EVENTS_OUT" \
    || fail "ask-approval trigger did not emit approval-request event"
grep -Fq "$approval_trigger" "$EVENTS_OUT" \
    || fail "approval-request event did not mention unique trigger"
grep -Fq "waiting for operator approval" "$EVENTS_OUT" \
    || fail "approval-request event did not include operator approval copy"
grep -Fq "trigger_task_completed" "$EVENTS_OUT" \
    || fail "start-task trigger did not emit deterministic task event"
grep -Fq "$task_trigger" "$EVENTS_OUT" \
    || fail "task event did not mention unique trigger"
grep -Fq "$token" "$EVENTS_OUT" \
    || fail "task/trigger events did not preserve failed action token"
grep -Fq "make why" "$EVENTS_OUT" \
    || fail "deterministic task event did not include diagnostic next step"

grep -Fq "trigger_action_approval_requested" "$INBOX_OUT" \
    || fail "approval-request event did not appear in ambient inbox"
grep -Fq "trigger_task_completed" "$INBOX_OUT" \
    || fail "task event did not appear in ambient inbox"

say PASS "ambient trigger action smoke passed"
