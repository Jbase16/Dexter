#!/usr/bin/env bash
# Give the rebuilt Rust core a stable code identity so macOS TCC grants survive builds.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_BIN="${DEXTER_CORE_BIN:-$ROOT_DIR/src/rust-core/target/release/dexter-core}"
IDENTITY="${DEXTER_CODE_SIGN_IDENTITY:-}"

if [[ ! -x "$CORE_BIN" ]]; then
    printf '[FAIL] Dexter core binary is missing or not executable: %s\n' "$CORE_BIN" >&2
    exit 1
fi

if [[ -z "$IDENTITY" ]]; then
    IDENTITY="$(
        security find-identity -v -p codesigning 2>/dev/null \
            | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' \
            | head -1
    )"
fi

if [[ -z "$IDENTITY" ]]; then
    printf '[FAIL] No Apple Development signing identity is available.\n' >&2
    printf '       Set DEXTER_CODE_SIGN_IDENTITY to a valid identity from: security find-identity -v -p codesigning\n' >&2
    exit 1
fi

codesign \
    --force \
    --sign "$IDENTITY" \
    --identifier com.jason.dexter.core \
    --timestamp=none \
    "$CORE_BIN"
codesign --verify --strict "$CORE_BIN"

printf '[INFO] Signed Dexter core as com.jason.dexter.core with %s\n' "$IDENTITY"
