from __future__ import annotations

import json
import tempfile
import unittest
import uuid
from datetime import UTC, datetime, timedelta
from pathlib import Path

from scripts.release_attestation import (
    AttestationError,
    MANUAL_INVALID,
    MANUAL_INVALIDATED,
    MANUAL_PASS,
    MANUAL_PENDING,
    identity_fingerprint,
    load_manual_status,
    write_attestation,
)

RUN_ID = str(uuid.UUID("12345678-1234-5678-1234-567812345678"))
FINISHED = datetime(2026, 7, 31, 12, 0, tzinfo=UTC)


def manifest():
    return {
        "schema_version": 1,
        "run_id": RUN_ID,
        "finished_at": FINISHED.isoformat(),
        "result": "PASS",
        "identity": {
            "source_tree_start_sha256": "source",
            "source_tree_end_sha256": "source",
        },
        "runtime": {"ollama_api_compatibility": "PASS"},
        "artifacts": {"rust_core": {"sha256": "core"}},
        "identity_changes": [],
        "gate_errors": [],
        "manual_checklist": {
            "version": 1,
            "status": "PENDING",
            "attested_at": None,
        },
    }


class ReleaseAttestationTests(unittest.TestCase):
    def test_attestation_is_bound_to_exact_run_and_identity(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            release_directory = Path(directory)
            current = manifest()
            write_attestation(
                release_directory,
                current,
                run_id=RUN_ID,
                attested_at=FINISHED + timedelta(minutes=5),
            )
            self.assertEqual(
                load_manual_status(current, release_directory),
                MANUAL_PASS,
            )

    def test_missing_attestation_is_pending(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(
                load_manual_status(manifest(), Path(directory)),
                MANUAL_PENDING,
            )

    def test_identity_change_invalidates_attestation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            release_directory = Path(directory)
            original = manifest()
            write_attestation(
                release_directory,
                original,
                run_id=RUN_ID,
                attested_at=FINISHED + timedelta(minutes=5),
            )
            changed = manifest()
            changed["identity"]["source_tree_end_sha256"] = "changed"
            self.assertNotEqual(
                identity_fingerprint(original),
                identity_fingerprint(changed),
            )
            self.assertEqual(
                load_manual_status(changed, release_directory),
                MANUAL_INVALIDATED,
            )

    def test_malformed_attestation_is_invalid(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            release_directory = Path(directory)
            path = release_directory / "attestations" / f"{RUN_ID}.json"
            path.parent.mkdir(parents=True)
            path.write_text("{", encoding="utf-8")
            self.assertEqual(
                load_manual_status(manifest(), release_directory),
                MANUAL_INVALID,
            )

    def test_wrong_run_cannot_be_attested(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(AttestationError, "does not match"):
                write_attestation(
                    Path(directory),
                    manifest(),
                    run_id=str(uuid.uuid4()),
                    attested_at=FINISHED + timedelta(minutes=5),
                )


if __name__ == "__main__":
    unittest.main()
