#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_BIN="$RUST_DIR/target/release/dexter-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ambient-inbox-core.log"
SWIFT_LOG="/tmp/dexter-ambient-inbox-swift.log"
ACTION_OUT="/tmp/dexter-ambient-inbox-action.out"
SOCKET="/tmp/dexter.sock"
CORE_PID=""
SWIFT_PID=""
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"

source "$ROOT_DIR/scripts/lib/process-tree.sh"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    if [[ -f "$SWIFT_LOG" ]]; then
        say INFO "Swift log tail:"
        tail -n 140 "$SWIFT_LOG" || true
    fi
    if [[ -f "$CORE_LOG" ]]; then
        say INFO "core log tail:"
        tail -n 120 "$CORE_LOG" || true
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
    "rationale": "ambient inbox smoke failed"
}))
PY
}

cleanup() {
    if [[ -n "$SWIFT_PID" ]]; then
        stop_process_tree "$SWIFT_PID"
        wait "$SWIFT_PID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$CORE_PID" ]]; then
        stop_process_tree "$CORE_PID"
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

wait_for_pattern() {
    local file="$1"
    local pattern="$2"
    local timeout_secs="$3"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if grep -Fq "$pattern" "$file"; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

verify_acknowledged_trigger() {
    python3 - "$1" "$HOME/.dexter/state/ambient_events.jsonl" "$HOME/.dexter/state/ambient_acknowledgements.json" <<'PY'
import json
import sys

token, events_path, acks_path = sys.argv[1:4]
events = []
with open(events_path, "r", encoding="utf-8") as handle:
    for line in handle:
        line = line.strip()
        if line:
            events.append(json.loads(line))

matched_action_id = None
for event in events:
    payload = event.get("payload") or {}
    if event.get("kind") == "action_failed" and token in str(payload.get("description", "")):
        matched_action_id = event.get("event_id")

if not matched_action_id:
    raise SystemExit(f"no action_failed event found for token {token}")

trigger_id = None
for event in events:
    payload = event.get("payload") or {}
    if (
        event.get("kind") == "trigger_matched"
        and payload.get("matched_event_id") == matched_action_id
        and payload.get("trigger_name") == "Dexter action failures"
    ):
        trigger_id = event.get("event_id")

if not trigger_id:
    raise SystemExit("no default action-failure trigger match found for failed action")

with open(acks_path, "r", encoding="utf-8") as handle:
    acknowledged = set((json.load(handle).get("acknowledged_event_ids") or []))

if trigger_id not in acknowledged:
    raise SystemExit(f"trigger event {trigger_id} was not acknowledged")
PY
}

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

if [[ ! -x "$CORE_BIN" || ! -x "$CLI_BIN" ]]; then
    fail "missing release binaries; build with: cd src/rust-core && cargo build --release --bin dexter-core --bin dexter-cli"
fi

rm -f "$CORE_LOG" "$SWIFT_LOG" "$ACTION_OUT"

say INFO "starting release core; log: $CORE_LOG"
RUST_LOG=info "$CORE_BIN" >>"$CORE_LOG" 2>&1 &
CORE_PID="$!"

waited=0
while [[ "$waited" -lt 90 ]]; do
    if socket_accepts; then
        break
    fi
    sleep 1
    waited=$((waited + 1))
done
socket_accepts || fail "core did not open $SOCKET within 90s"

waited=0
while [[ "$waited" -lt "$CORE_WARMUP_TIMEOUT_SECS" ]]; do
    if grep -Fq "Daemon startup warmup complete" "$CORE_LOG"; then
        say INFO "core warmup complete"
        break
    fi
    sleep 1
    waited=$((waited + 1))
done
grep -Fq "Daemon startup warmup complete" "$CORE_LOG" \
    || fail "core warmup did not complete within ${CORE_WARMUP_TIMEOUT_SECS}s"

token="AMBIENT_INBOX_FAILED_$(date +%s)_$$"
say INFO "driving failed action to create ambient trigger match"
"$CLI_BIN" --quiet --idle-timeout 180 --action-json "$(json_action "$token")" >"$ACTION_OUT" 2>&1 \
    || fail "failed synthetic action did not return cleanly to CLI"

say INFO "starting Swift HUD ambient inbox smoke; log: $SWIFT_LOG"
(
    cd "$SWIFT_DIR" || exit 2
    DEXTER_HUD_SMOKE=1 \
    DEXTER_HUD_SMOKE_IDLE_ONLY=1 \
    DEXTER_HUD_SMOKE_KEEP_CORE_ON_EXIT=1 \
    DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS=1 \
    DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=10 \
    DEXTER_PROACTIVE_HEALTH_INITIAL_DELAY_SECS=120 \
    DEXTER_PROACTIVE_AMBIENT_INITIAL_DELAY_SECS=1 \
    DEXTER_PROACTIVE_AMBIENT_POLL_INTERVAL_SECS=60 \
        swift run
) >>"$SWIFT_LOG" 2>&1 &
SWIFT_PID="$!"

wait_for_pattern "$SWIFT_LOG" "[DexterClient] Proactive ambient inbox surfacing" 60 \
    || fail "Swift did not surface ambient inbox notice"
wait_for_pattern "$SWIFT_LOG" "[HUDSmoke] showAmbientNotice" 30 \
    || fail "HUD did not render ambient notice"
wait_for_pattern "$SWIFT_LOG" "[DexterClient] Ambient inbox acknowledged count=" 30 \
    || fail "Swift did not acknowledge ambient inbox notice"

wait "$SWIFT_PID" >/dev/null 2>&1 || true
SWIFT_PID=""

grep -Fq "Dexter Notices" "$SWIFT_LOG" \
    || fail "ambient notice markdown did not include Dexter Notices"
grep -Fq "Dexter action failures" "$SWIFT_LOG" \
    || fail "ambient notice did not include default action failure trigger"
grep -Fq "Ambient inbox requested" "$CORE_LOG" \
    || fail "core did not receive AmbientInbox RPC"
grep -Fq "Ambient events acknowledged" "$CORE_LOG" \
    || fail "core did not receive ambient acknowledgement RPC"

verify_acknowledged_trigger "$token" \
    || fail "durable acknowledgement did not include the surfaced trigger"

say PASS "ambient inbox HUD smoke passed"
