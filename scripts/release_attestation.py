#!/usr/bin/env python3
"""Create and validate optional run-bound Daily-driver v1 attestations."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import uuid
from datetime import UTC, datetime
from pathlib import Path
from typing import Mapping

REPO_ROOT = Path(__file__).resolve().parent.parent
if os.fspath(REPO_ROOT) not in sys.path:
    sys.path.insert(0, os.fspath(REPO_ROOT))

from scripts.release_identity import write_json_atomic

ATTESTATION_SCHEMA_VERSION = 1
CHECKLIST_VERSION = 1
MANUAL_PASS = "PASS"
MANUAL_PENDING = "PENDING"
MANUAL_INVALID = "INVALID"
MANUAL_INVALIDATED = "INVALIDATED"


class AttestationError(RuntimeError):
    pass


def identity_fingerprint(manifest: Mapping[str, object]) -> str:
    bound_identity = {
        "run_id": manifest.get("run_id"),
        "identity": manifest.get("identity"),
        "runtime": manifest.get("runtime"),
        "artifacts": manifest.get("artifacts"),
    }
    encoded = json.dumps(
        bound_identity,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _parse_timestamp(value: object) -> datetime:
    if not isinstance(value, str) or not value:
        raise ValueError("timestamp is missing")
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError("timestamp must include a timezone")
    return parsed.astimezone(UTC)


def _validate_run_id(run_id: str) -> None:
    try:
        uuid.UUID(run_id)
    except (ValueError, AttributeError) as error:
        raise AttestationError("run ID must be an exact UUID from latest.json") from error


def build_attestation(
    manifest: Mapping[str, object],
    *,
    run_id: str,
    attested_at: datetime,
) -> dict[str, object]:
    _validate_run_id(run_id)
    if manifest.get("schema_version") != 1:
        raise AttestationError("latest manifest schema is unsupported")
    if manifest.get("run_id") != run_id:
        raise AttestationError("run ID does not match the latest manifest")
    if manifest.get("result") != "PASS":
        raise AttestationError("manual attestation requires an automated PASS")
    manual = manifest.get("manual_checklist")
    if not isinstance(manual, dict) or manual.get("version") != CHECKLIST_VERSION:
        raise AttestationError("manual checklist version does not match")
    if manifest.get("identity_changes") or manifest.get("gate_errors"):
        raise AttestationError("manifest identity or gate execution is not stable")
    identity = manifest.get("identity")
    if not isinstance(identity, dict):
        raise AttestationError("manifest identity is missing")
    if (
        not identity.get("source_tree_start_sha256")
        or identity.get("source_tree_start_sha256")
        != identity.get("source_tree_end_sha256")
    ):
        raise AttestationError("source changed during the gate")
    return {
        "schema_version": ATTESTATION_SCHEMA_VERSION,
        "run_id": run_id,
        "checklist_version": CHECKLIST_VERSION,
        "attested_at": attested_at.astimezone(UTC).isoformat(),
        "identity_sha256": identity_fingerprint(manifest),
    }


def attestation_path(release_directory: Path, run_id: str) -> Path:
    _validate_run_id(run_id)
    return release_directory / "attestations" / f"{run_id}.json"


def write_attestation(
    release_directory: Path,
    manifest: Mapping[str, object],
    *,
    run_id: str,
    attested_at: datetime,
) -> Path:
    payload = build_attestation(
        manifest,
        run_id=run_id,
        attested_at=attested_at,
    )
    path = attestation_path(release_directory, run_id)
    write_json_atomic(path, payload)
    return path


def load_manual_status(
    manifest: Mapping[str, object],
    release_directory: Path,
) -> str:
    run_id = manifest.get("run_id")
    if not isinstance(run_id, str):
        return MANUAL_INVALID
    try:
        path = attestation_path(release_directory, run_id)
    except AttestationError:
        return MANUAL_INVALID
    if not path.is_file():
        return MANUAL_PENDING
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return MANUAL_INVALID
    if not isinstance(payload, dict):
        return MANUAL_INVALID
    if payload.get("schema_version") != ATTESTATION_SCHEMA_VERSION:
        return MANUAL_INVALID
    if payload.get("run_id") != run_id:
        return MANUAL_INVALIDATED
    if payload.get("checklist_version") != CHECKLIST_VERSION:
        return MANUAL_INVALIDATED
    if payload.get("identity_sha256") != identity_fingerprint(manifest):
        return MANUAL_INVALIDATED
    try:
        attested_at = _parse_timestamp(payload.get("attested_at"))
        finished_at = _parse_timestamp(manifest.get("finished_at"))
    except ValueError:
        return MANUAL_INVALID
    if attested_at < finished_at:
        return MANUAL_INVALIDATED
    return MANUAL_PASS


def _load_latest(release_directory: Path) -> dict[str, object]:
    path = release_directory / "latest.json"
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise AttestationError("authoritative latest.json is missing or invalid") from error
    if not isinstance(payload, dict):
        raise AttestationError("authoritative latest.json is not an object")
    return payload


def attest_latest(
    *,
    root: Path,
    release_directory: Path,
    run_id: str,
    confirmed: bool,
    now: datetime,
) -> Path:
    if not confirmed:
        raise AttestationError("attestation requires explicit --confirm")
    manifest = _load_latest(release_directory)
    if manifest.get("run_id") != run_id:
        raise AttestationError("run ID does not match authoritative latest.json")

    # Imported lazily so the status consumer can import load_manual_status
    # without creating a module cycle.
    from scripts.release_status import PASS, STRICT_FRESHNESS_HOURS, evaluate_latest

    report = evaluate_latest(
        release_directory,
        root=root,
        now=now,
        max_age_hours=STRICT_FRESHNESS_HOURS,
    )
    if report.status != PASS:
        raise AttestationError(
            f"manual attestation requires current automated PASS, got {report.status}"
        )
    return write_attestation(
        release_directory,
        manifest,
        run_id=run_id,
        attested_at=now,
    )


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Attest the manual checklist for an exact release run."
    )
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    parser.add_argument(
        "--release-dir",
        type=Path,
        default=REPO_ROOT / "docs/live-smoke-results/release",
    )
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--confirm", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    try:
        path = attest_latest(
            root=args.root,
            release_directory=args.release_dir,
            run_id=args.run_id,
            confirmed=args.confirm,
            now=datetime.now(UTC),
        )
    except AttestationError as error:
        print(f"manual attestation not recorded: {error}", file=sys.stderr)
        return 1
    print(f"Manual checklist attested: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
