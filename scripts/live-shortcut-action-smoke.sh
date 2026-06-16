#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-shortcut-action-core.log"
ACTION_OUT="/tmp/dexter-shortcut-action.out"
RECENT_OUT="/tmp/dexter-shortcut-action-recent.out"
EVENTS_OUT="/tmp/dexter-shortcut-action-events.out"
INBOX_OUT="/tmp/dexter-shortcut-action-inbox.out"
SOCKET="/tmp/dexter.sock"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    for file in "$ACTION_OUT" "$RECENT_OUT" "$EVENTS_OUT" "$INBOX_OUT"; do
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

cleanup_smoke_inbox() {
    python3 - "$HOME/.dexter/state/ambient_events.jsonl" "$HOME/.dexter/state/ambient_acknowledgements.json" <<'PY' || true
import json
import sys
from pathlib import Path

events_path = Path(sys.argv[1])
ack_path = Path(sys.argv[2])
if not events_path.exists():
    raise SystemExit(0)

acknowledged = set()
if ack_path.exists():
    try:
        raw = json.loads(ack_path.read_text())
        if isinstance(raw, list):
            acknowledged.update(str(item) for item in raw)
        elif isinstance(raw, dict):
            acknowledged.update(
                str(item)
                for item in raw.get("acknowledged_event_ids", [])
                if str(item).strip()
            )
    except Exception:
        pass

for line in events_path.read_text(errors="replace").splitlines():
    if not line.strip():
        continue
    try:
        event = json.loads(line)
    except Exception:
        continue
    haystack = json.dumps(event, sort_keys=True).upper()
    if "SHORTCUT_SMOKE_" in haystack and "TRIGGER_MATCHED" in haystack:
        event_id = event.get("event_id")
        if event_id:
            acknowledged.add(str(event_id))

ack_path.parent.mkdir(parents=True, exist_ok=True)
payload = {
    "schema_version": "1.0",
    "acknowledged_event_ids": sorted(acknowledged),
}
tmp_path = ack_path.with_name(f".{ack_path.name}.shortcut-smoke.tmp")
tmp_path.write_text(json.dumps(payload, indent=2) + "\n")
tmp_path.replace(ack_path)
PY
}

json_action() {
    python3 - "$1" <<'PY'
import json
import sys

name = sys.argv[1]
print(json.dumps({
    "type": "shortcut",
    "name": name,
    "input_path": None,
    "output_path": None,
    "rationale": "shortcut action smoke approval gate"
}))
PY
}

cleanup() {
    cleanup_smoke_inbox
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$ACTION_OUT" "$RECENT_OUT" "$EVENTS_OUT" "$INBOX_OUT"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

shortcut_name="SHORTCUT_SMOKE_DO_NOT_RUN_$(date +%s)_$$"
action_json="$(json_action "$shortcut_name")"

say INFO "driving auto-denied Shortcut action"
"$CLI_BIN" --auto-deny --idle-timeout 180 --action-json "$action_json" >"$ACTION_OUT" 2>&1 \
    || fail "Shortcut action did not return cleanly to CLI"

grep -Fq "approval required" "$ACTION_OUT" \
    || fail "Shortcut action did not request approval"
grep -Fq "[ACTION REQUEST" "$ACTION_OUT" \
    || fail "Shortcut action did not emit an action request"
grep -Fq "Denied" "$ACTION_OUT" \
    || fail "Shortcut action was not auto-denied"
if grep -Fq "EXECUTED" "$ACTION_OUT"; then
    fail "Shortcut action executed despite auto-deny"
fi

"$CLI_BIN" --actions recent --limit 20 >"$RECENT_OUT"
grep -Fq "shortcut" "$RECENT_OUT" \
    || fail "recent action receipts did not include shortcut action type"
grep -Fq "Shortcut: $shortcut_name" "$RECENT_OUT" \
    || fail "recent action receipts did not show readable Shortcut target"
grep -Fq "DENIED" "$RECENT_OUT" \
    || fail "recent action receipts did not record denial"

"$CLI_BIN" --events --limit 80 >"$EVENTS_OUT"
grep -Fq "action_approval_requested" "$EVENTS_OUT" \
    || fail "ambient events did not include Shortcut approval request"
grep -Fq "action_denied" "$EVENTS_OUT" \
    || fail "ambient events did not include Shortcut denial"
grep -Fq "$shortcut_name" "$EVENTS_OUT" \
    || fail "ambient events did not mention Shortcut smoke name"

cleanup_smoke_inbox
"$CLI_BIN" --inbox --limit 20 >"$INBOX_OUT"
if grep -Fq "$shortcut_name" "$INBOX_OUT"; then
    fail "Shortcut smoke left an unacknowledged inbox notice"
fi

say PASS "Shortcut action smoke passed"
