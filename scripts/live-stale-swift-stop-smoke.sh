#!/usr/bin/env bash
# scripts/live-stale-swift-stop-smoke.sh - prove make stop kills repo-owned SwiftPM app strays.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT_DIR/scripts/lib/process-tree.sh"

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_LOG="/tmp/dexter-stale-swift-stop-core.log"
SWIFT_LOG="/tmp/dexter-stale-swift-stop-swift.log"
CORE_PID=""
SWIFT_PID=""

pass() {
    printf '[PASS] %s\n' "$1"
}

fail() {
    printf '[FAIL] %s\n' "$1" >&2
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

process_cwd() {
    local pid="$1"
    lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1
}

repo_swift_app_pids() {
    local pid cwd
    while IFS= read -r pid; do
        [[ -n "$pid" ]] || continue
        cwd="$(process_cwd "$pid")"
        if [[ "$cwd" == "$SWIFT_DIR" ]]; then
            printf '%s\n' "$pid"
        fi
    done < <(pgrep -x Dexter 2>/dev/null || true)
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
    rm -f "$SOCKET" "$SHELL_SOCKET"
}
trap cleanup EXIT INT TERM

[[ -x "$CORE_BIN" ]] || fail "missing core binary: $CORE_BIN"

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections at $SOCKET"
fi

: > "$CORE_LOG"
RUST_LOG=info "$CORE_BIN" >> "$CORE_LOG" 2>&1 &
CORE_PID="$!"

for _ in {1..90}; do
    socket_accepts && break
    sleep 1
done
socket_accepts || {
    tail -80 "$CORE_LOG" || true
    fail "core did not open $SOCKET"
}

: > "$SWIFT_LOG"
(
    cd "$SWIFT_DIR"
    DEXTER_HUD_SMOKE=1 \
    DEXTER_HUD_SMOKE_TEXT="stale swift stop smoke" \
    DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS=90 \
    DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=120 \
        swift run
) >> "$SWIFT_LOG" 2>&1 &
SWIFT_PID="$!"

swift_app_pid=""
for _ in {1..60}; do
    swift_app_pid="$(repo_swift_app_pids | head -1)"
    [[ -n "$swift_app_pid" ]] && break
    sleep 1
done

[[ -n "$swift_app_pid" ]] || {
    tail -120 "$SWIFT_LOG" || true
    fail "repo-owned Swift Dexter app process did not start"
}
pass "repo-owned Swift Dexter app started pid=$swift_app_pid"

stop_process_tree "$CORE_PID"
wait "$CORE_PID" >/dev/null 2>&1 || true
CORE_PID=""
rm -f "$SOCKET" "$SHELL_SOCKET"

if socket_accepts; then
    fail "core still accepts connections after simulated daemon stop"
fi
pass "simulated daemon stop left Swift app as the remaining process"

bash "$ROOT_DIR/scripts/stop-dexter.sh" --quiet

if repo_swift_app_pids | grep -q .; then
    repo_swift_app_pids >&2 || true
    fail "make stop did not kill repo-owned Swift Dexter app"
fi

socket_accepts && fail "socket still accepts after make stop"
[[ ! -e "$SOCKET" && ! -e "$SHELL_SOCKET" ]] || fail "socket files remain after make stop"

stop_process_tree "$SWIFT_PID"
wait "$SWIFT_PID" >/dev/null 2>&1 || true
SWIFT_PID=""

pass "stale Swift stop smoke passed"
