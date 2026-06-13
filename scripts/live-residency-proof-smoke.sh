#!/usr/bin/env bash
# scripts/live-residency-proof-smoke.sh - safe cross-process residency proof.
#
# This intentionally proves the mmap+mlock mechanism on a small real Ollama blob
# (default: mxbai-embed-large) instead of wiring PRIMARY's full GGUF during a
# routine smoke run.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
PROOF_MODEL="${DEXTER_RESIDENCY_PROOF_MODEL:-mxbai-embed-large}"
EXPECTED_MODELS_DIR="${OLLAMA_MODELS:-/Users/jason/ollama-models}"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

if [[ ! -x "$CORE_BIN" ]]; then
    say "FAIL" "missing core binary: $CORE_BIN"
    say "INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-core"
    exit 2
fi

if [[ ! -d "$EXPECTED_MODELS_DIR" ]]; then
    say "FAIL" "Ollama models directory missing: $EXPECTED_MODELS_DIR"
    exit 2
fi

out="$(mktemp -t dexter-residency-proof.XXXXXX)"
cleanup() {
    rm -f "$out"
}
trap cleanup EXIT INT TERM

say "INFO" "running residency proof for $PROOF_MODEL using OLLAMA_MODELS=$EXPECTED_MODELS_DIR"
if ! OLLAMA_MODELS="$EXPECTED_MODELS_DIR" "$CORE_BIN" --prove-residency "$PROOF_MODEL" > "$out" 2>&1; then
    say "FAIL" "residency proof command failed"
    cat "$out"
    exit 1
fi

if ! grep -Fq "VERDICT: PROVEN" "$out"; then
    say "FAIL" "residency proof did not prove the mechanism"
    cat "$out"
    exit 1
fi

if ! grep -Fq "observer mincore" "$out"; then
    say "FAIL" "residency proof did not report observer mincore evidence"
    cat "$out"
    exit 1
fi

if ! grep -Fq "Δ wired" "$out"; then
    say "FAIL" "residency proof did not report wired-memory delta evidence"
    cat "$out"
    exit 1
fi

say "PASS" "live residency proof smoke passed"
