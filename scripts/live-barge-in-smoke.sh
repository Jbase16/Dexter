#!/usr/bin/env bash
# scripts/live-barge-in-smoke.sh - Swift TTS cancellation race smoke.
#
# Starts the release Rust core and real Swift HUD, submits a smoke turn as
# from_voice=true so Swift receives AudioResponse frames, then has the Swift
# client send HOTKEY_ACTIVATED from the same gRPC session after the first PCM
# buffer is scheduled. Assertions prove audio was scheduled before interruption, Rust aborted
# the in-flight turn, Swift reached LISTENING, and no PCM buffer was scheduled
# after the LISTENING transition.
#
# Usage:
#   scripts/live-barge-in-smoke.sh --start-core

set -u

SOCKET="/tmp/dexter.sock"
CORE_LOG="/tmp/dexter-barge-core-smoke.log"
SWIFT_LOG="/tmp/dexter-barge-swift-smoke.log"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_PID=""
SWIFT_PID=""
SMOKE_TEXT="${DEXTER_BARGE_SMOKE_TEXT:-Tell me a long calm story in exactly twelve short sentences about debugging a tiny spaceship made from spare keyboard parts. Keep speaking until all twelve sentences are done.}"

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

if [[ "${1:-}" == "--start-core" ]]; then
    START_CORE=1
    shift
fi

say() {
    printf '[%s] %s\n' "$1" "$2"
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

cleanup() {
    if [[ -n "$SWIFT_PID" ]]; then
        kill "$SWIFT_PID" >/dev/null 2>&1 || true
        wait "$SWIFT_PID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

require_bins() {
    if [[ ! -x "$CORE_BIN" ]]; then
        say "$FAIL" "missing core binary: $CORE_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-core"
        exit 2
    fi
}

start_core_if_requested() {
    if [[ "$START_CORE" -ne 1 ]]; then
        if ! socket_accepts; then
            say "$FAIL" "no Dexter daemon accepting connections at $SOCKET"
            exit 2
        fi
        return
    fi

    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        say "$INFO" "stop it first, or run this script without --start-core against its log"
        exit 2
    fi

    : > "$CORE_LOG"
    say "$INFO" "starting release core; log: $CORE_LOG"
    RUST_LOG=info "$CORE_BIN" >> "$CORE_LOG" 2>&1 &
    CORE_PID="$!"

    local waited=0
    while [[ "$waited" -lt 90 ]]; do
        if socket_accepts; then
            break
        fi
        sleep 1
        waited=$((waited + 1))
    done
    if ! socket_accepts; then
        say "$FAIL" "core did not open $SOCKET within 90s"
        tail -40 "$CORE_LOG" || true
        exit 2
    fi

    waited=0
    while [[ "$waited" -lt 120 ]]; do
        if grep -Fq "Daemon startup warmup complete" "$CORE_LOG"; then
            say "$INFO" "core warmup complete"
            return
        fi
        sleep 1
        waited=$((waited + 1))
    done

    say "$FAIL" "core socket opened, but warmup did not complete within 120s"
    tail -80 "$CORE_LOG" || true
    exit 2
}

start_swift_smoke() {
    : > "$SWIFT_LOG"
    say "$INFO" "starting Swift barge-in smoke; log: $SWIFT_LOG"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_TEXT="$SMOKE_TEXT" \
        DEXTER_HUD_SMOKE_FROM_VOICE=1 \
        DEXTER_HUD_SMOKE_AUDIO_TRACE=1 \
        DEXTER_HUD_SMOKE_BARGE_ON_FIRST_AUDIO=1 \
        DEXTER_HUD_SMOKE_BARGE_DELAY_MS="${DEXTER_HUD_SMOKE_BARGE_DELAY_MS:-250}" \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS="${DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS:-3}" \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS="${DEXTER_HUD_SMOKE_EXIT_AFTER_SECS:-24}" \
            swift run
    ) >> "$SWIFT_LOG" 2>&1 &
    SWIFT_PID="$!"
}

wait_for_pattern() {
    local pattern="$1"
    local timeout_secs="$2"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if grep -Fq "$pattern" "$SWIFT_LOG" || grep -Fq "$pattern" "$CORE_LOG"; then
            return 0
        fi
        if [[ -n "$SWIFT_PID" ]] && ! kill -0 "$SWIFT_PID" >/dev/null 2>&1; then
            if grep -Fq "$pattern" "$SWIFT_LOG" || grep -Fq "$pattern" "$CORE_LOG"; then
                return 0
            fi
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

assert_log_absent() {
    local label="$1"
    local pattern="$2"
    if grep -Fq "$pattern" "$SWIFT_LOG" || grep -Fq "$pattern" "$CORE_LOG"; then
        say "$FAIL" "$label - unexpected log pattern: $pattern"
        return 1
    fi
    return 0
}

assert_no_audio_schedule_after_listening() {
    python3 - "$SWIFT_LOG" <<'PY'
import sys

path = sys.argv[1]
listening = False
scheduled_before = False
with open(path, "r", encoding="utf-8", errors="replace") as f:
    for line in f:
        if "entityState: listening" in line:
            listening = True
            continue
        if "[AudioPlayerSmoke] schedule" in line:
            if listening:
                print(line.rstrip())
                sys.exit(1)
            scheduled_before = True

if not scheduled_before:
    sys.exit(2)
if not listening:
    sys.exit(3)
sys.exit(0)
PY
    local rc=$?
    if [[ "$rc" -eq 1 ]]; then
        say "$FAIL" "barge-in audio gate - audio scheduled after LISTENING"
        return 1
    fi
    if [[ "$rc" -eq 2 ]]; then
        say "$FAIL" "barge-in audio gate - no audio was scheduled before interrupt"
        return 1
    fi
    if [[ "$rc" -eq 3 ]]; then
        say "$FAIL" "barge-in audio gate - LISTENING transition not observed"
        return 1
    fi
    return 0
}

run_assertions() {
    local ok=0
    local name="Swift barge-in smoke"

    wait_for_pattern "[HUDSmoke] autoSubmit" 60 || {
        say "$FAIL" "$name - smoke hook did not auto-submit within 60s"
        tail -120 "$SWIFT_LOG" || true
        return 1
    }
    wait_for_pattern "[DexterClient] sendVoiceSmokeInput enqueued to stream" 15 || {
        say "$FAIL" "$name - voice-mode smoke input was not sent"
        tail -120 "$SWIFT_LOG" || true
        return 1
    }
    wait_for_pattern "[AudioPlayerSmoke] schedule" 90 || {
        say "$FAIL" "$name - Swift never scheduled TTS audio"
        tail -160 "$SWIFT_LOG" || true
        return 1
    }
    wait_for_pattern "[HUDSmoke] bargeInAfterScheduledAudio fired" 30 || {
        say "$FAIL" "$name - synthetic hotkey did not fire after scheduled audio"
        tail -160 "$SWIFT_LOG" || true
        return 1
    }
    wait_for_pattern "Hotkey aborting in-flight generation" 20 || {
        say "$FAIL" "$name - Rust did not abort an in-flight generation"
        tail -120 "$CORE_LOG" || true
        return 1
    }
    wait_for_pattern "entityState: listening" 20 || {
        say "$FAIL" "$name - Swift did not receive LISTENING after interrupt"
        tail -160 "$SWIFT_LOG" || true
        return 1
    }

    assert_no_audio_schedule_after_listening || ok=1
    assert_log_absent "$name" "sendVoiceSmokeInput DROPPED" || ok=1
    assert_log_absent "$name" "Fatal error" || ok=1
    assert_log_absent "$name" "Session error" || ok=1

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "$name passed"
        return 0
    fi

    tail -160 "$SWIFT_LOG" || true
    tail -120 "$CORE_LOG" || true
    return 1
}

main() {
    require_bins
    start_core_if_requested
    start_swift_smoke
    run_assertions
}

main "$@"
