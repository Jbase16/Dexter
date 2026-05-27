#!/usr/bin/env bash
# scripts/live-degraded-mode-smoke.sh - controlled startup failure/degraded-mode smoke.
#
# Starts fresh release cores under isolated HOME directories and intentionally
# breaks one dependency class at a time. The goal is not to make Dexter "work"
# under broken prerequisites; it is to prove the daemon fails closed or reports
# a precise degraded state through dexter-cli --doctor, then shuts down cleanly.

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
LOG_PREFIX="/tmp/dexter-degraded-mode"
CORE_PID=""
CURRENT_LOG=""
TEMP_DIRS=()

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

cleanup() {
    stop_core_if_owned >/dev/null 2>&1 || true
    for dir in "${TEMP_DIRS[@]:-}"; do
        rm -rf "$dir" >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT INT TERM

new_temp_dir() {
    local dir
    dir="$(mktemp -d -t dexter-degraded-mode.XXXXXX)"
    TEMP_DIRS+=("$dir")
    printf '%s\n' "$dir"
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

remove_stale_sockets() {
    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        return 1
    fi
    rm -f "$SOCKET" "$SHELL_SOCKET"
    return 0
}

require_bins() {
    if [[ ! -x "$CORE_BIN" ]]; then
        say "$FAIL" "missing core binary: $CORE_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-core --bin dexter-cli"
        exit 2
    fi
    if [[ ! -x "$CLI_BIN" ]]; then
        say "$FAIL" "missing CLI binary: $CLI_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-cli"
        exit 2
    fi
}

stop_core_if_owned() {
    if [[ -z "$CORE_PID" ]]; then
        return 0
    fi

    local pid="$CORE_PID"
    CORE_PID=""
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" >/dev/null 2>&1 || true
}

assert_sockets_clean() {
    local label="$1"
    if socket_accepts; then
        say "$FAIL" "$label - daemon still accepts connections after cleanup"
        return 1
    fi
    if [[ -e "$SOCKET" || -e "$SHELL_SOCKET" ]]; then
        say "$FAIL" "$label - stale socket files remain"
        ls -l "$SOCKET" "$SHELL_SOCKET" 2>/dev/null || true
        return 1
    fi
    return 0
}

start_core() {
    local label="$1"
    local home_dir="$2"
    local cwd="$3"

    stop_core_if_owned
    remove_stale_sockets || exit 2

    CURRENT_LOG="${LOG_PREFIX}-${label}.log"
    : > "$CURRENT_LOG"
    say "$INFO" "starting degraded-mode core '$label'; log: $CURRENT_LOG"
    (
        cd "$cwd" || exit 2
        exec env HOME="$home_dir" RUST_LOG=info "$CORE_BIN"
    ) >> "$CURRENT_LOG" 2>&1 &
    CORE_PID="$!"
}

wait_for_socket() {
    local label="$1"
    local timeout_secs="$2"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if socket_accepts; then
            return 0
        fi
        if [[ -n "$CORE_PID" ]] && ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say "$FAIL" "$label - core exited before opening socket"
            tail -80 "$CURRENT_LOG" || true
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "$label - core did not open $SOCKET within ${timeout_secs}s"
    tail -80 "$CURRENT_LOG" || true
    return 1
}

wait_for_log() {
    local label="$1"
    local pattern="$2"
    local timeout_secs="$3"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if grep -Fq "$pattern" "$CURRENT_LOG"; then
            return 0
        fi
        if [[ -n "$CORE_PID" ]] && ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say "$FAIL" "$label - core exited before log pattern: $pattern"
            tail -80 "$CURRENT_LOG" || true
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "$label - missing log pattern within ${timeout_secs}s: $pattern"
    tail -100 "$CURRENT_LOG" || true
    return 1
}

wait_for_exit() {
    local label="$1"
    local timeout_secs="$2"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if [[ -z "$CORE_PID" ]] || ! ps -p "$CORE_PID" >/dev/null 2>&1; then
            local code=0
            if [[ -n "$CORE_PID" ]]; then
                wait "$CORE_PID" >/dev/null 2>&1
                code="$?"
                CORE_PID=""
            fi
            return "$code"
        fi
        local state
        state="$(ps -p "$CORE_PID" -o stat= 2>/dev/null | tr -d '[:space:]')"
        if [[ "$state" == Z* ]]; then
            local code=0
            wait "$CORE_PID" >/dev/null 2>&1
            code="$?"
            CORE_PID=""
            return "$code"
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "$label - core did not exit within ${timeout_secs}s"
    tail -80 "$CURRENT_LOG" || true
    return 124
}

assert_contains() {
    local file="$1"
    local pattern="$2"
    local label="$3"
    if ! grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - missing: $pattern"
        cat "$file"
        return 1
    fi
    return 0
}

run_doctor_capture() {
    local home_dir="$1"
    local out_file="$2"
    HOME="$home_dir" "$CLI_BIN" --doctor > "$out_file" 2>&1
}

write_bad_ollama_config() {
    local home_dir="$1"
    mkdir -p "$home_dir/.dexter"
    cat > "$home_dir/.dexter/config.toml" <<EOF
[core]
socket_path = "$SOCKET"
state_dir = "$home_dir/.dexter/state"
personality_path = "config/personality/default.yaml"

[inference]
ollama_base_url = "http://127.0.0.1:9"
request_timeout_secs = 1
connect_timeout_secs = 1
stream_inactivity_timeout_secs = 1
EOF
}

test_malformed_config_fails_closed() {
    local home_dir
    home_dir="$(new_temp_dir)"
    mkdir -p "$home_dir/.dexter"
    printf '[inference\nollama_base_url = "http://127.0.0.1:9"\n' > "$home_dir/.dexter/config.toml"

    start_core "bad-config" "$home_dir" "$ROOT_DIR"
    if wait_for_exit "bad config" 10; then
        say "$FAIL" "bad config - core exited successfully despite malformed TOML"
        return 1
    fi

    assert_contains "$CURRENT_LOG" "malformed TOML" "bad config" || return 1
    assert_sockets_clean "bad config" || return 1
    say "$PASS" "bad config fails closed before binding sockets"
    return 0
}

test_ollama_unreachable_reports_degraded_models() {
    local home_dir doctor_out
    home_dir="$(new_temp_dir)"
    doctor_out="$(mktemp -t dexter-degraded-ollama-doctor.XXXXXX)"
    write_bad_ollama_config "$home_dir"

    start_core "ollama-unreachable" "$home_dir" "$ROOT_DIR"
    wait_for_socket "ollama unreachable" 30 || return 1
    wait_for_log "ollama unreachable" "Daemon startup warmup complete" 60 || return 1

    if run_doctor_capture "$home_dir" "$doctor_out"; then
        say "$FAIL" "ollama unreachable - doctor unexpectedly passed"
        cat "$doctor_out"
        return 1
    fi

    assert_contains "$doctor_out" "FAIL daemon health" "ollama unreachable" || return 1
    assert_contains "$doctor_out" "status degraded; attention components" "ollama unreachable" || return 1
    assert_contains "$doctor_out" "fast_model" "ollama unreachable" || return 1
    assert_contains "$doctor_out" "primary_model" "ollama unreachable" || return 1
    assert_contains "$doctor_out" "embed_model" "ollama unreachable" || return 1
    assert_contains "$doctor_out" "FAIL fast model" "ollama unreachable" || return 1
    assert_contains "$doctor_out" "FAIL primary model" "ollama unreachable" || return 1
    assert_contains "$doctor_out" "FAIL embed model" "ollama unreachable" || return 1
    assert_contains "$doctor_out" "FAIL ollama" "ollama unreachable" || return 1

    stop_core_if_owned
    assert_sockets_clean "ollama unreachable" || return 1
    say "$PASS" "Ollama outage reports degraded model health and failing doctor"
    return 0
}

test_missing_worker_paths_report_degraded_workers() {
    local home_dir bad_cwd doctor_out restart_out
    home_dir="$(new_temp_dir)"
    bad_cwd="$(new_temp_dir)"
    doctor_out="$(mktemp -t dexter-degraded-workers-doctor.XXXXXX)"
    restart_out="$(mktemp -t dexter-degraded-workers-restart.XXXXXX)"

    start_core "missing-workers" "$home_dir" "$bad_cwd"
    wait_for_socket "missing workers" 30 || return 1
    wait_for_log "missing workers" "Daemon startup warmup complete" 90 || return 1

    if run_doctor_capture "$home_dir" "$doctor_out"; then
        say "$FAIL" "missing workers - doctor unexpectedly passed"
        cat "$doctor_out"
        return 1
    fi

    assert_contains "$doctor_out" "FAIL daemon health" "missing workers" || return 1
    assert_contains "$doctor_out" "stt_worker" "missing workers" || return 1
    assert_contains "$doctor_out" "tts_worker" "missing workers" || return 1
    assert_contains "$doctor_out" "browser_worker" "missing workers" || return 1
    assert_contains "$doctor_out" "FAIL STT worker" "missing workers" || return 1
    assert_contains "$doctor_out" "FAIL TTS worker" "missing workers" || return 1
    assert_contains "$doctor_out" "FAIL browser worker" "missing workers" || return 1
    assert_contains "$doctor_out" "OK   fast model" "missing workers" || return 1
    assert_contains "$doctor_out" "OK   primary model" "missing workers" || return 1

    if HOME="$home_dir" "$CLI_BIN" --restart-component browser > "$restart_out" 2>&1; then
        say "$FAIL" "missing workers - browser restart unexpectedly succeeded"
        cat "$restart_out"
        return 1
    fi
    assert_contains "$restart_out" "FAIL restart browser" "missing workers restart" || return 1
    assert_contains "$restart_out" "Browser worker restart failed" "missing workers restart" || return 1

    stop_core_if_owned
    assert_sockets_clean "missing workers" || return 1
    say "$PASS" "missing worker paths report degraded workers and failed recovery"
    return 0
}

main() {
    require_bins
    test_malformed_config_fails_closed || exit 1
    test_ollama_unreachable_reports_degraded_models || exit 1
    test_missing_worker_paths_report_degraded_workers || exit 1
    say "$PASS" "live degraded-mode smoke passed"
}

main "$@"
