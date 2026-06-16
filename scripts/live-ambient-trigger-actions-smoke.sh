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
TRIGGERS_PATH="$HOME/.dexter/state/ambient_triggers.json"
TRIGGERS_BACKUP="/tmp/dexter-ambient-trigger-actions-triggers.$$.json"
TRIGGERS_EXISTED=0

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
    if [[ "$TRIGGERS_EXISTED" == "1" && -f "$TRIGGERS_BACKUP" ]]; then
        cp "$TRIGGERS_BACKUP" "$TRIGGERS_PATH" || true
    elif [[ "$TRIGGERS_EXISTED" == "0" ]]; then
        rm -f "$TRIGGERS_PATH" || true
    fi
    python3 - <<'PY' >/dev/null 2>&1 || true
import json
import pathlib

state = pathlib.Path.home() / ".dexter" / "state"
events_path = state / "ambient_events.jsonl"
ack_path = state / "ambient_acknowledgements.json"
if not events_path.exists():
    raise SystemExit(0)

events = []
matched_events_by_id = {}
for line in events_path.read_text().splitlines():
    if not line.strip():
        continue
    event = json.loads(line)
    events.append(event)
    event_id = event.get("event_id")
    if event_id:
        matched_events_by_id[event_id] = event

acknowledged = set()
if ack_path.exists() and ack_path.read_text().strip():
    acknowledgement = json.loads(ack_path.read_text())
    acknowledged.update(str(event_id) for event_id in acknowledgement.get("acknowledged_event_ids", []) if event_id)

smoke_prefixes = ("Smoke ask approval ", "Smoke start task ")
smoke_action_token = "AMBIENT_TRIGGER_ACTIONS_FAILED_"
for event in events:
    event_id = event.get("event_id")
    payload = event.get("payload") or {}
    trigger_name = str(payload.get("trigger_name", ""))
    matched_event_id = payload.get("matched_event_id")
    matched_event = matched_events_by_id.get(matched_event_id, {})
    matched_payload = matched_event.get("payload") or {}
    matched_description = str(matched_payload.get("description", ""))
    is_smoke_trigger_notice = trigger_name.startswith(smoke_prefixes)
    is_smoke_default_notice = (
        trigger_name == "Dexter action failures"
        and smoke_action_token in matched_description
    )
    if event_id and (is_smoke_trigger_notice or is_smoke_default_notice):
        acknowledged.add(str(event_id))

ack = {
    "schema_version": "1.0",
    "acknowledged_event_ids": sorted(acknowledged),
}
tmp = ack_path.with_name(f".{ack_path.name}.tmp")
tmp.write_text(json.dumps(ack, indent=2) + "\n")
tmp.replace(ack_path)
PY
    rm -f "$TRIGGERS_BACKUP" || true
}
trap cleanup EXIT

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$ACTION_OUT" "$EVENTS_OUT" "$INBOX_OUT"
if [[ -f "$TRIGGERS_PATH" ]]; then
    mkdir -p "$(dirname "$TRIGGERS_BACKUP")"
    cp "$TRIGGERS_PATH" "$TRIGGERS_BACKUP"
    TRIGGERS_EXISTED=1
fi

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
