#!/usr/bin/env bash
# Verify acceptance-status against isolated authoritative release evidence.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dexter-acceptance-release.XXXXXX")"
EMPTY_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dexter-acceptance-empty.XXXXXX")"
OUT="$(mktemp "${TMPDIR:-/tmp}/dexter-acceptance-status.out.XXXXXX")"
EMPTY_OUT="$(mktemp "${TMPDIR:-/tmp}/dexter-acceptance-status-empty.out.XXXXXX")"
STRICT_EMPTY_OUT="$(mktemp "${TMPDIR:-/tmp}/dexter-acceptance-strict-empty.out.XXXXXX")"

cleanup() {
    rm -rf "$RELEASE_DIR" "$EMPTY_DIR"
    rm -f "$OUT" "$EMPTY_OUT" "$STRICT_EMPTY_OUT"
}
trap cleanup EXIT

fail() {
    printf '[FAIL] %s\n' "$1" >&2
    exit 1
}

require_contains() {
    local file="$1"
    local pattern="$2"
    local message="$3"

    if ! rg -q -- "$pattern" "$file"; then
        printf '[DEBUG] missing pattern: %s\n' "$pattern" >&2
        cat "$file" >&2 || true
        fail "$message"
    fi
}

python3 - "$ROOT_DIR" "$RELEASE_DIR/latest.json" <<'PY'
from __future__ import annotations

import json
import sys
from dataclasses import asdict
from datetime import UTC, datetime
from pathlib import Path

root = Path(sys.argv[1])
output = Path(sys.argv[2])
sys.path.insert(0, str(root))

from scripts.release_checks import CHECK_SPECS, collect_release_artifacts
from scripts.release_identity import collect_release_identity
from scripts.release_status import ACCEPTANCE_TARGETS, ACTION_SAFETY_FULL_TARGETS

identity = collect_release_identity(root).to_dict()
source = identity["source"]
artifacts = {
    name: asdict(artifact)
    for name, artifact in collect_release_artifacts(root).items()
}
targets = [
    {"battery": "acceptance", "target": target, "result": "PASS"}
    for target in sorted(ACCEPTANCE_TARGETS)
]
targets.extend(
    {
        "battery": "action_safety_full",
        "target": target,
        "result": "PASS",
    }
    for target in sorted(ACTION_SAFETY_FULL_TARGETS)
)
now = datetime.now(UTC).isoformat()
manifest = {
    "schema_version": 1,
    "run_id": "00000000-0000-4000-8000-000000000002",
    "started_at": now,
    "finished_at": now,
    "result": "PASS",
    "release_state": "AUTOMATED_PASS_MANUAL_PENDING",
    "identity": {
        "source_tree_sha256": source["sha256"],
        "source_tree_file_count": source["file_count"],
        "source_tree_start_sha256": source["sha256"],
        "source_tree_end_sha256": source["sha256"],
        "config_sha256": identity["config_sha256"],
        "personality_sha256": identity["personality_sha256"],
        "input_sha256": identity["input_sha256"],
    },
    "runtime": identity["runtime"],
    "checks": [
        {"check_id": spec.check_id, "result": "PASS"}
        for spec in CHECK_SPECS
    ],
    "artifacts": artifacts,
    "acceptance_targets": targets,
    "identity_changes": [],
    "gate_errors": [],
    "manual_checklist": {
        "version": 1,
        "status": "PENDING",
        "attested_at": None,
    },
}
output.write_text(json.dumps(manifest), encoding="utf-8")
PY

DEXTER_RELEASE_EVIDENCE_DIR="$RELEASE_DIR" \
    DEXTER_ACCEPTANCE_STRICT=1 \
    bash "$ROOT_DIR/scripts/acceptance-status.sh" > "$OUT"

require_contains "$OUT" '# Dexter Acceptance Status' "acceptance status missing title"
require_contains "$OUT" 'Automated evidence: \*\*PASS\*\*' "passing evidence was not accepted"
require_contains "$OUT" 'Run ID: `00000000-0000-4000-8000-000000000002`' "run ID missing"
require_contains "$OUT" 'Gate result: `PASS`' "gate result missing"
require_contains "$OUT" 'Manual checklist: \*\*PENDING\*\*' "manual state missing"

DEXTER_RELEASE_EVIDENCE_DIR="$EMPTY_DIR" \
    bash "$ROOT_DIR/scripts/acceptance-status.sh" > "$EMPTY_OUT"
require_contains "$EMPTY_OUT" 'Automated evidence: \*\*MISSING\*\*' \
    "empty non-strict run should report missing evidence"

if DEXTER_RELEASE_EVIDENCE_DIR="$EMPTY_DIR" \
    DEXTER_ACCEPTANCE_STRICT=1 \
    bash "$ROOT_DIR/scripts/acceptance-status.sh" >"$STRICT_EMPTY_OUT" 2>&1; then
    fail "strict acceptance status should fail when evidence is missing"
fi

printf '[PASS] acceptance status smoke passed\n'
