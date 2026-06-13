#!/usr/bin/env bash
set -euo pipefail

EXPECTED_MODELS_DIR="/Users/jason/ollama-models"
EXTERNAL_LIBRARY_DIR="/Volumes/BitHappens/ollama-models"

fail() {
    printf '[FAIL] %s\n' "$1" >&2
    exit 1
}

info() {
    printf '[INFO] %s\n' "$1"
}

if [[ ! -d "$EXPECTED_MODELS_DIR" ]]; then
    fail "Dexter runtime model directory missing: $EXPECTED_MODELS_DIR"
fi

if [[ ! -d "$EXTERNAL_LIBRARY_DIR" ]]; then
    info "External Ollama archive not mounted: $EXTERNAL_LIBRARY_DIR"
    info "Continuing because Dexter runtime uses the local hot set."
fi

launchctl setenv OLLAMA_MODELS "$EXPECTED_MODELS_DIR" \
    || fail "launchctl setenv OLLAMA_MODELS failed"

export OLLAMA_MODELS="$EXPECTED_MODELS_DIR"

process_value="${OLLAMA_MODELS:-unset}"
launchctl_value="$(launchctl getenv OLLAMA_MODELS 2>/dev/null || true)"
if [[ -z "$launchctl_value" ]]; then
    launchctl_value="unset"
fi

info "process OLLAMA_MODELS=$process_value"
info "launchctl OLLAMA_MODELS=$launchctl_value"

if [[ "$launchctl_value" != "$EXPECTED_MODELS_DIR" ]]; then
    fail "launchctl OLLAMA_MODELS did not stick; expected $EXPECTED_MODELS_DIR"
fi

info "Dexter Ollama runtime store is configured."
