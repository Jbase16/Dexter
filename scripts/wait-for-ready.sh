#!/usr/bin/env bash
set -euo pipefail

CLI_BIN=""
TIMEOUT_SECS="${DEXTER_READY_TIMEOUT_SECS:-300}"
OUT_FILE="/tmp/dexter-wait-ready.out"
LABEL="Dexter health"
CORE_PID=""
CORE_LOG=""
OLLAMA_FAIL_FAST_GRACE_SECS="${DEXTER_READY_OLLAMA_FAIL_FAST_GRACE_SECS:-12}"
ALLOW_PERMISSION_DEGRADED="${DEXTER_READY_ALLOW_PERMISSION_DEGRADED:-0}"

usage() {
    cat <<'USAGE'
Usage: scripts/wait-for-ready.sh --cli-bin PATH [options]

Options:
  --timeout SECS                 Maximum time to wait for doctor-ready health.
  --out PATH                     File to write the latest doctor report.
  --label TEXT                   Human-readable subject for status messages.
  --core-pid PID                 Optional core PID; fail if it exits while waiting.
  --core-log PATH                Optional core log tail to print on failure.
  --ollama-fail-fast-grace SECS  Grace period before failing fast on unreachable Ollama.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cli-bin)
            CLI_BIN="${2:-}"
            shift 2
            ;;
        --timeout)
            TIMEOUT_SECS="${2:-}"
            shift 2
            ;;
        --out)
            OUT_FILE="${2:-}"
            shift 2
            ;;
        --label)
            LABEL="${2:-}"
            shift 2
            ;;
        --core-pid)
            CORE_PID="${2:-}"
            shift 2
            ;;
        --core-log)
            CORE_LOG="${2:-}"
            shift 2
            ;;
        --ollama-fail-fast-grace)
            OLLAMA_FAIL_FAST_GRACE_SECS="${2:-}"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "[FAIL] unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

fail() {
    echo "[FAIL] $*" >&2
    if [[ -f "$OUT_FILE" ]]; then
        echo "[INFO] last doctor report:" >&2
        cat "$OUT_FILE" >&2 || true
    fi
    if [[ -n "$CORE_LOG" && -f "$CORE_LOG" ]]; then
        echo "[INFO] core log tail:" >&2
        tail -120 "$CORE_LOG" >&2 || true
    fi
    exit 2
}

permission_degraded_only() {
    [[ "$ALLOW_PERMISSION_DEGRADED" == "1" ]] || return 1
    grep -Fq "FAIL daemon health      status degraded; attention components" "$OUT_FILE" || return 1
    grep -Fq "OK   fast model" "$OUT_FILE" || return 1
    grep -Fq "OK   primary model" "$OUT_FILE" || return 1
    grep -Fq "OK   embed model" "$OUT_FILE" || return 1
    grep -Fq "OK   STT worker" "$OUT_FILE" || return 1
    grep -Fq "OK   TTS worker" "$OUT_FILE" || return 1
    grep -Fq "OK   browser worker" "$OUT_FILE" || return 1

    local unexpected_failures
    unexpected_failures="$(
        grep '^FAIL ' "$OUT_FILE" \
            | grep -Ev '^FAIL (daemon health|Accessibility permission|Screen Recording permission)[[:space:]]' \
            || true
    )"
    [[ -z "$unexpected_failures" ]] || return 1

    local health_line
    health_line="$(grep -F "FAIL daemon health" "$OUT_FILE" | head -1)"
    [[ "$health_line" == *"accessibility_permission"* || "$health_line" == *"screen_recording_permission"* ]] || return 1
    [[ "$health_line" != *"fast_model"* ]]
    [[ "$health_line" != *"primary_model"* ]]
    [[ "$health_line" != *"embed_model"* ]]
    [[ "$health_line" != *"stt_worker"* ]]
    [[ "$health_line" != *"tts_worker"* ]]
    [[ "$health_line" != *"browser_worker"* ]]
}

is_positive_int() {
    [[ "$1" =~ ^[0-9]+$ ]] && [[ "$1" -gt 0 ]]
}

if [[ -z "$CLI_BIN" ]]; then
    fail "--cli-bin is required"
fi
if [[ ! -x "$CLI_BIN" ]]; then
    fail "dexter-cli is not executable at $CLI_BIN"
fi
if ! is_positive_int "$TIMEOUT_SECS"; then
    fail "--timeout must be a positive integer, got '$TIMEOUT_SECS'"
fi
if ! is_positive_int "$OLLAMA_FAIL_FAST_GRACE_SECS"; then
    fail "--ollama-fail-fast-grace must be a positive integer, got '$OLLAMA_FAIL_FAST_GRACE_SECS'"
fi
if [[ -n "$CORE_PID" && ! "$CORE_PID" =~ ^[0-9]+$ ]]; then
    fail "--core-pid must be numeric when provided, got '$CORE_PID'"
fi

: > "$OUT_FILE"

elapsed=0
while [[ "$elapsed" -lt "$TIMEOUT_SECS" ]]; do
    "$CLI_BIN" --doctor >"$OUT_FILE" 2>&1 || true
    if grep -Fq "OK   daemon health      status ready" "$OUT_FILE" \
        && grep -Fq "Result: OK - no failed checks." "$OUT_FILE"; then
        echo "[INFO] ${LABEL} doctor-ready after ${elapsed}s"
        exit 0
    fi
    if permission_degraded_only; then
        echo "[INFO] ${LABEL} operational after ${elapsed}s; macOS permissions still require attention"
        exit 0
    fi

    if [[ -n "$CORE_PID" ]] && ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
        fail "${LABEL} exited before doctor-ready health"
    fi

    if [[ "$elapsed" -ge "$OLLAMA_FAIL_FAST_GRACE_SECS" ]] \
        && grep -Fq "FAIL ollama" "$OUT_FILE" \
        && grep -Fqi "unreachable" "$OUT_FILE"; then
        echo "[FAIL] ${LABEL} cannot become ready because Ollama is unreachable." >&2
        echo "[INFO] Recovery: run 'open -a Ollama', wait for http://localhost:11434/api/tags, then rerun the smoke." >&2
        fail "Ollama unreachable during readiness wait"
    fi

    sleep 2
    elapsed=$((elapsed + 2))
done

fail "${LABEL} did not become doctor-ready within ${TIMEOUT_SECS}s"
