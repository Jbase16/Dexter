#!/usr/bin/env bash
# scripts/restart-dexter-ui.sh - Terminal-backed full Dexter restart.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_LOG="${DEXTER_CORE_LOG:-/tmp/dexter-core.log}"
RESTART_DELAY_SECS="${DEXTER_RESTART_DELAY_SECS:-1}"
TAIL_PID=""

cleanup() {
    if [[ -n "$TAIL_PID" ]]; then
        kill "$TAIL_PID" >/dev/null 2>&1 || true
        wait "$TAIL_PID" >/dev/null 2>&1 || true
    fi
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

cd "$ROOT_DIR" || exit 2
export OLLAMA_MODELS="${OLLAMA_MODELS:-/Users/jason/ollama-models}"

clear
printf 'Dexter live logs\n'
printf 'Restarting Dexter...\n'
if [[ -n "${DEXTER_STARTED_FROM:-}" ]]; then
    printf 'Started from: %s\n' "$DEXTER_STARTED_FROM"
fi
printf 'OLLAMA_MODELS=%s\n\n' "$OLLAMA_MODELS"
printf 'Use Dexter > New Session for a fresh conversation.\n'
printf 'Use Dexter > Restart Dexter to restart the app/core.\n'
printf 'Use Dexter > Quit Dexter to stop the app/core.\n\n'

case "$RESTART_DELAY_SECS" in
    ''|*[!0-9]*)
        RESTART_DELAY_SECS=1
        ;;
esac

# Give the previous Swift process a short window to terminate and release its
# own core cleanup before starting the new detached core.
sleep "$RESTART_DELAY_SECS"

make configure-ollama-models || exit 1
make restart-core || exit 1

if [[ -f "$CORE_LOG" ]]; then
    printf '\n==> Tailing Rust core log: %s\n\n' "$CORE_LOG"
    tail -n +1 -f "$CORE_LOG" &
    TAIL_PID="$!"
fi

make run-swift
