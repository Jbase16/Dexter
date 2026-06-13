#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CORE_LOG="/tmp/dexter-ambient-events-core.log"
EVENTS_OUT="/tmp/dexter-ambient-events.out"
TRIGGER_ADD_OUT="/tmp/dexter-ambient-trigger-add.out"
SOCKET="/tmp/dexter.sock"
STATE_DIR="$(
    python3 <<'PY'
import os
from pathlib import Path
try:
    import tomllib
except Exception:
    tomllib = None

config = Path.home() / ".dexter" / "config.toml"
if tomllib is not None and config.exists():
    try:
        data = tomllib.loads(config.read_text())
        state_dir = data.get("core", {}).get("state_dir")
        if isinstance(state_dir, str) and state_dir.strip():
            print(os.path.expanduser(state_dir))
            raise SystemExit(0)
    except Exception:
        pass
print(str(Path.home() / ".dexter" / "state"))
PY
)"
TRIGGERS_FILE="$STATE_DIR/ambient_triggers.json"
TRIGGERS_BACKUP="/tmp/dexter-ambient-triggers.$$.bak"
TRIGGERS_HAD_ORIGINAL=0

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
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

cleanup() {
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
    if [[ "$TRIGGERS_HAD_ORIGINAL" == "1" ]]; then
        mv "$TRIGGERS_BACKUP" "$TRIGGERS_FILE" >/dev/null 2>&1 || true
    else
        rm -f "$TRIGGERS_FILE"
        rm -f "$TRIGGERS_BACKUP"
    fi
}
trap cleanup EXIT

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$EVENTS_OUT" "$TRIGGER_ADD_OUT"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

mkdir -p "$STATE_DIR"
if [[ -f "$TRIGGERS_FILE" ]]; then
    cp "$TRIGGERS_FILE" "$TRIGGERS_BACKUP"
    TRIGGERS_HAD_ORIGINAL=1
fi

say INFO "installing temporary ambient trigger"
"$RUST_DIR/target/release/dexter-cli" \
    --add-trigger "Ambient smoke health warnings" \
    --event-kind health_status_changed \
    --min-severity warn \
    --trigger-action notify_only >"$TRIGGER_ADD_OUT"

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

"$RUST_DIR/target/release/dexter-cli" --events --limit 20 >"$EVENTS_OUT"

grep -Fq "daemon_started" "$EVENTS_OUT" \
    || fail "ambient event queue did not include daemon_started"
grep -Fq "health_status_changed" "$EVENTS_OUT" \
    || fail "ambient event queue did not include health_status_changed"
grep -Fq "Dexter health ready" "$EVENTS_OUT" \
    || fail "ambient event queue did not include ready health transition"
grep -Fq "trigger_matched" "$EVENTS_OUT" \
    || fail "ambient event queue did not include trigger_matched"
grep -Fq "Ambient smoke health warnings" "$EVENTS_OUT" \
    || fail "ambient trigger match did not name the temporary trigger"

say PASS "ambient events smoke passed"
