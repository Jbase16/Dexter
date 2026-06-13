#!/usr/bin/env bash
# scripts/stop-dexter.sh - stop Dexter processes owned by this checkout.
#
# Default mode stops the Swift UI and Rust core. --core-only stops only the Rust
# daemon and is used by the Swift app while it is already terminating itself.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SWIFT_DIR="$ROOT_DIR/src/swift"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
RUN_PID_FILE="/tmp/dexter-make-run.pid"
CORE_ONLY=0
QUIET=0

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --core-only)
            CORE_ONLY=1
            ;;
        --quiet)
            QUIET=1
            ;;
        *)
            printf '[FAIL] unknown argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
    shift
done

say() {
    if [[ "$QUIET" -eq 0 ]]; then
        printf '%s\n' "$1"
    fi
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

append_pid() {
    local pid="$1"
    if [[ -z "$pid" || "$pid" == "$$" || "$pid" == "$PPID" ]]; then
        return 0
    fi
    case "$pid" in
        *[!0-9]*)
            return 0
            ;;
    esac
    PIDS+=("$pid")
}

append_pgrep() {
    local pattern="$1"
    local pid
    while IFS= read -r pid; do
        append_pid "$pid"
    done < <(pgrep -f "$pattern" 2>/dev/null || true)
}

process_cwd() {
    local pid="$1"
    lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1
}

process_command() {
    local pid="$1"
    ps -p "$pid" -o command= 2>/dev/null \
        | tr '\n' ' ' \
        | sed -E 's/[[:space:]]+/ /g; s/[[:space:]]$//' \
        | cut -c 1-180
}

describe_pid() {
    local pid="$1"
    local command cwd
    command="$(process_command "$pid")"
    cwd="$(process_cwd "$pid")"

    [[ -n "$command" ]] || command="unknown command"
    if [[ -n "$cwd" ]]; then
        printf '    %s - %s (cwd: %s)\n' "$pid" "$command" "$cwd"
    else
        printf '    %s - %s\n' "$pid" "$command"
    fi
}

describe_targets() {
    local pid
    while IFS= read -r pid; do
        [[ -n "$pid" ]] || continue
        describe_pid "$pid"
    done
}

append_swift_run_from_repo() {
    local pid cwd
    while IFS= read -r pid; do
        cwd="$(process_cwd "$pid")"
        if [[ "$cwd" == "$SWIFT_DIR" ]]; then
            append_pid "$pid"
        fi
    done < <(pgrep -x swift 2>/dev/null || true)
}

append_swift_app_from_repo() {
    local pid cwd
    while IFS= read -r pid; do
        cwd="$(process_cwd "$pid")"
        if [[ "$cwd" == "$SWIFT_DIR" ]]; then
            append_pid "$pid"
        fi
    done < <(pgrep -x Dexter 2>/dev/null || true)
}

append_socket_owners() {
    local pid
    if [[ ! -e "$SOCKET" && ! -e "$SHELL_SOCKET" ]]; then
        return 0
    fi

    while IFS= read -r pid; do
        append_pid "$pid"
    done < <(lsof -t "$SOCKET" "$SHELL_SOCKET" 2>/dev/null || true)
}

append_run_loop_pid() {
    if [[ "$CORE_ONLY" -ne 0 || ! -f "$RUN_PID_FILE" ]]; then
        return 0
    fi

    local pid cwd
    pid="$(tr -cd '0-9' < "$RUN_PID_FILE")"
    if [[ -z "$pid" || "$pid" == "$$" || "$pid" == "$PPID" ]]; then
        return 0
    fi

    cwd="$(process_cwd "$pid")"
    if [[ "$cwd" == "$ROOT_DIR" ]]; then
        append_pid "$pid"
    fi
}

unique_pids() {
    if [[ "${#PIDS[@]}" -eq 0 ]]; then
        return 0
    fi
    printf '%s\n' "${PIDS[@]}" | awk '!seen[$0]++'
}

send_signal() {
    local signal="$1"
    local pid
    while IFS= read -r pid; do
        kill "-$signal" "$pid" >/dev/null 2>&1 || true
    done
}

alive_pids() {
    local pid
    while IFS= read -r pid; do
        if kill -0 "$pid" >/dev/null 2>&1; then
            printf '%s\n' "$pid"
        fi
    done
}

wait_for_exit() {
    local deadline="$1"
    local waited=0
    local remaining
    while [[ "$waited" -lt "$deadline" ]]; do
        remaining="$(alive_pids <<< "$TARGET_PIDS")"
        if [[ -z "$remaining" ]]; then
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

PIDS=()
append_run_loop_pid
append_socket_owners
append_pgrep "$ROOT_DIR/src/rust-core/target/release/dexter-core"
append_pgrep "$ROOT_DIR/src/rust-core/target/debug/dexter-core"
append_pgrep "cargo run --manifest-path $ROOT_DIR/src/rust-core/Cargo.toml"
append_pgrep "cargo run --manifest-path src/rust-core/Cargo.toml"

if [[ "$CORE_ONLY" -eq 0 ]]; then
    append_pgrep "$ROOT_DIR/src/swift/.build/.*/Dexter"
    append_pgrep "$ROOT_DIR/src/swift/.build/arm64-apple-macosx/debug/Dexter"
    append_swift_run_from_repo
    append_swift_app_from_repo
fi

TARGET_PIDS="$(unique_pids)"

if [[ -n "$TARGET_PIDS" ]]; then
    say "==> Stopping Dexter process(es): $(tr '\n' ' ' <<< "$TARGET_PIDS" | sed 's/[[:space:]]*$//')"
    if [[ "$QUIET" -eq 0 ]]; then
        describe_targets <<< "$TARGET_PIDS"
    fi
    send_signal TERM <<< "$TARGET_PIDS"
    if ! wait_for_exit 5; then
        say "==> Dexter process(es) still alive after TERM; sending KILL"
        alive_now="$(alive_pids <<< "$TARGET_PIDS")"
        if [[ -n "$alive_now" ]]; then
            send_signal KILL <<< "$alive_now"
            TARGET_PIDS="$alive_now"
            wait_for_exit 3 || true
        fi
    fi
fi

if socket_accepts; then
    say "ERROR: Dexter core is still accepting connections at $SOCKET" >&2
    lsof -nU 2>/dev/null | grep -F -- "$SOCKET" >&2 || true
    exit 1
fi

rm -f "$SOCKET" "$SHELL_SOCKET"
if [[ "$CORE_ONLY" -eq 0 ]]; then
    rm -f "$RUN_PID_FILE"
fi
say "==> Dexter stopped"
