#!/usr/bin/env bash
# scripts/start-dexter-core.sh - start the Rust core as a detached background process.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKET="/tmp/dexter.sock"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
DEFAULT_LOG="/tmp/dexter-core.log"
DEFAULT_PID_FILE="/tmp/dexter-core.pid"
DEFAULT_READY_OUT="/tmp/dexter-start-core-ready.out"
LOG_PATH="$DEFAULT_LOG"
PID_FILE="$DEFAULT_PID_FILE"
READY_OUT="$DEFAULT_READY_OUT"
WAIT_READY=0
RESTART=0
QUIET=0
READY_TIMEOUT="${DEXTER_READY_TIMEOUT_SECS:-300}"
SOCKET_TIMEOUT="${DEXTER_SOCKET_TIMEOUT_SECS:-90}"

usage() {
    cat <<'USAGE'
Usage: scripts/start-dexter-core.sh [options]

Options:
  --restart             Stop any existing Dexter core/UI before starting core.
  --wait-ready          Wait for doctor-clean daemon health, not just socket.
  --log PATH            Write core output to PATH. Default: /tmp/dexter-core.log
  --pid-file PATH       Write launcher PID to PATH. Default: /tmp/dexter-core.pid
  --ready-out PATH      Write readiness output to PATH. Default: /tmp/dexter-start-core-ready.out
  --quiet               Suppress informational output.
  --help, -h            Show this help.
USAGE
}

say() {
    if [[ "$QUIET" -eq 0 ]]; then
        printf '%s\n' "$1"
    fi
}

fail() {
    printf '[FAIL] %s\n' "$*" >&2
    if [[ -f "$LOG_PATH" ]]; then
        printf '[INFO] core log tail (%s):\n' "$LOG_PATH" >&2
        tail -120 "$LOG_PATH" >&2 || true
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

wait_for_socket() {
    local pid="$1"
    local waited=0
    while [[ "$waited" -lt "$SOCKET_TIMEOUT" ]]; do
        if socket_accepts; then
            say "[INFO] Dexter core socket accepting after ${waited}s"
            return 0
        fi
        if ! kill -0 "$pid" >/dev/null 2>&1; then
            fail "Dexter core launcher exited before opening $SOCKET"
        fi
        sleep 1
        waited=$((waited + 1))
    done
    fail "Dexter core did not open $SOCKET within ${SOCKET_TIMEOUT}s"
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --restart)
            RESTART=1
            shift
            ;;
        --wait-ready)
            WAIT_READY=1
            shift
            ;;
        --log)
            LOG_PATH="${2:-}"
            shift 2
            ;;
        --pid-file)
            PID_FILE="${2:-}"
            shift 2
            ;;
        --ready-out)
            READY_OUT="${2:-}"
            shift 2
            ;;
        --quiet)
            QUIET=1
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            printf '[FAIL] unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "$LOG_PATH" || -z "$PID_FILE" || -z "$READY_OUT" ]]; then
    fail "--log, --pid-file, and --ready-out must not be empty"
fi

if [[ ! -x "$CORE_BIN" ]]; then
    fail "Dexter core release binary is not executable at $CORE_BIN; build it with: cd src/rust-core && cargo build --release --bin dexter-core --bin dexter-cli"
fi

# Cargo's linker-generated ad-hoc signature changes on every build. A stable
# development signature keeps Screen Recording and Accessibility TCC grants
# attached to the core across rebuilds.
bash "$ROOT_DIR/scripts/sign-dexter-core.sh" >/dev/null || \
    fail "Dexter core signing failed; refusing to launch with an unstable TCC identity"

if [[ "$RESTART" -eq 1 ]]; then
    bash "$ROOT_DIR/scripts/stop-dexter.sh" --quiet || true
elif socket_accepts; then
    say "[INFO] Dexter core is already accepting connections at $SOCKET"
    exit 0
fi

mkdir -p "$(dirname "$LOG_PATH")" "$(dirname "$PID_FILE")"
: > "$LOG_PATH"
: > "$READY_OUT"

say "[INFO] starting detached Dexter core"
say "[INFO] log: $LOG_PATH"
(
    cd "$ROOT_DIR" || exit 2
    nohup "$CORE_BIN" > "$LOG_PATH" 2>&1 < /dev/null &
    printf '%s\n' "$!" > "$PID_FILE"
)

PID="$(tr -cd '0-9' < "$PID_FILE")"
if [[ -z "$PID" ]]; then
    fail "Dexter core launcher PID was not written to $PID_FILE"
fi

wait_for_socket "$PID"

if [[ "$WAIT_READY" -eq 1 ]]; then
    "$ROOT_DIR/scripts/wait-for-ready.sh" \
        --cli-bin "$ROOT_DIR/src/rust-core/target/release/dexter-cli" \
        --timeout "$READY_TIMEOUT" \
        --out "$READY_OUT" \
        --label "Dexter detached core" \
        --core-pid "$PID" \
        --core-log "$LOG_PATH" \
        || exit 1
fi

if ! kill -0 "$PID" >/dev/null 2>&1; then
    fail "Dexter core exited after readiness checks"
fi
if ! socket_accepts; then
    fail "Dexter core socket stopped accepting after readiness checks"
fi

say "[INFO] Dexter core started; core pid $PID"
