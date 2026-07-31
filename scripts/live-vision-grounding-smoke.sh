#!/usr/bin/env bash
# Prove a current-screen question is backed by a fresh screenshot and that the
# exact serving release binary persists that evidence with the completed turn.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
LOG="/tmp/dexter-vision-grounding-smoke.log"
OUT="/tmp/dexter-vision-grounding-smoke.out"
DOCTOR_OUT="/tmp/dexter-vision-grounding-doctor.out"
PROMPT="Describe what you see in Safari right now. Only state details visible in the fresh screenshot."
CORE_PID=""

fail() {
    printf '[FAIL] %s\n' "$*"
    [[ -f "$OUT" ]] && cat "$OUT"
    [[ -f "$LOG" ]] && tail -100 "$LOG"
    exit 1
}

socket_accepts() {
    python3 - "$SOCKET" <<'PY' >/dev/null 2>&1
import socket
import sys

sock = socket.socket(socket.AF_UNIX)
sock.settimeout(1)
sys.exit(0 if sock.connect_ex(sys.argv[1]) == 0 else 1)
PY
}

cleanup() {
    if [[ -n "$CORE_PID" ]]; then
        local pid="$CORE_PID"
        CORE_PID=""
        kill "$pid" >/dev/null 2>&1 || true
        wait "$pid" >/dev/null 2>&1 || true
    fi
    rm -f "$SOCKET" "$SHELL_SOCKET"
}
trap cleanup EXIT INT TERM

socket_accepts && fail "a Dexter daemon already owns $SOCKET"
rm -f "$SOCKET" "$SHELL_SOCKET" "$LOG" "$OUT" "$DOCTOR_OUT"

bash "$ROOT_DIR/scripts/sign-dexter-core.sh" >/dev/null \
    || fail "release core could not be assigned Dexter's stable macOS identity"

printf '[INFO] starting fresh release core\n'
RUST_LOG=info "$CORE_BIN" >"$LOG" 2>&1 &
CORE_PID="$!"

bash "$ROOT_DIR/scripts/wait-for-ready.sh" \
    --cli-bin "$CLI_BIN" \
    --timeout "${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}" \
    --out "$DOCTOR_OUT" \
    --label "Vision grounding core" \
    --core-pid "$CORE_PID" \
    --core-log "$LOG" \
    >/dev/null || fail "core did not reach ready health"

printf '[INFO] asking exact current-Safari visual question\n'
"$CLI_BIN" --quiet --idle-timeout 240 "$PROMPT" >"$OUT" 2>&1 \
    || fail "Vision query did not return cleanly"

[[ "$(wc -c < "$OUT" | tr -d ' ')" -ge 80 ]] \
    || fail "Vision answer was empty or too short to be useful"
grep -Fq "I didn't produce a usable answer" "$OUT" \
    && fail "Vision returned the empty-generation fallback"
grep -Fq "I don't have access" "$OUT" \
    && fail "Vision returned a model capability denial"
grep -Fq "Vision query — capturing screen" "$LOG" \
    || fail "Vision route did not request a fresh capture"
grep -Fq 'vision_attachment_marker' "$LOG" \
    || fail "prompt manifest did not record the image-bearing user message"

python3 - "$ROOT_DIR" "$CORE_PID" "$PROMPT" <<'PY' \
    || fail "persisted Vision evidence did not match the serving release core"
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve()
expected_pid = int(sys.argv[2])
prompt = sys.argv[3]
records = []
state_root = pathlib.Path.home() / ".dexter" / "state" / "context_turns"
for path in state_root.glob("*/*.json"):
    try:
        record = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        continue
    if record.get("user_text_preview") == prompt:
        records.append((record.get("updated_at", ""), path, record))

if not records:
    raise SystemExit("no persisted Vision turn found")
_, path, record = max(records, key=lambda row: row[0])
runtime = record.get("runtime") or {}
expected_path = (root / "src/rust-core/target/release/dexter-core").resolve()
actual_path = pathlib.Path(runtime.get("executable_path", "")).resolve()
if runtime.get("process_id") != expected_pid:
    raise SystemExit(f"PID mismatch in {path}")
if actual_path != expected_path:
    raise SystemExit(f"executable mismatch in {path}: {actual_path}")
if not runtime.get("identity"):
    raise SystemExit(f"runtime identity missing in {path}")
if len(runtime.get("executable_blake3") or "") != 64:
    raise SystemExit(f"executable content hash missing in {path}")
capture = next(
    (item for item in record.get("evidence", []) if item.get("source") == "screen_capture"),
    None,
)
if not capture or not capture.get("payload_hash"):
    raise SystemExit(f"screenshot evidence missing in {path}")
generation = record.get("generation") or {}
if generation.get("response_len", 0) < 80:
    raise SystemExit(f"persisted Vision response was not useful in {path}")
PY

printf '[PASS] fresh Vision grounding and runtime provenance passed\n'
