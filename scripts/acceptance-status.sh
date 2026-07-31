#!/usr/bin/env bash
# Report authoritative DEX-02 release evidence status.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_DIR="${DEXTER_RELEASE_EVIDENCE_DIR:-$ROOT_DIR/docs/live-smoke-results/release}"
STRICT="${DEXTER_ACCEPTANCE_STRICT:-0}"

args=(
    --root "$ROOT_DIR"
    --release-dir "$RELEASE_DIR"
)

case "$STRICT" in
    1|true|TRUE|yes|YES)
        args+=(--strict)
        ;;
    *)
        if [[ -n "${DEXTER_ACCEPTANCE_MAX_AGE_HOURS:-}" ]]; then
            args+=(--max-age-hours "$DEXTER_ACCEPTANCE_MAX_AGE_HOURS")
        fi
        ;;
esac

exec python3 "$ROOT_DIR/scripts/release_status.py" "${args[@]}"
