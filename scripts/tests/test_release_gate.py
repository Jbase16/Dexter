from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts.release_checks import CHECK_SPECS
from scripts.release_gate import (
    GateAlreadyRunning,
    build_manifest,
    gate_lock,
    identity_changes,
    load_child_evidence,
    publish_manifest,
)


def identity(source_hash: str = "source", compatibility: str = "PASS"):
    return {
        "captured_at": "2026-07-30T00:00:00+00:00",
        "source": {"sha256": source_hash, "file_count": 10},
        "config_sha256": "config",
        "config_path": "/tmp/config.toml",
        "personality_sha256": "personality",
        "personality_path": "config/personality/default.yaml",
        "input_sha256": {"gate": "hash"},
        "runtime": {"ollama_api_compatibility": compatibility},
    }


def build_evidence(run_id: str):
    return {
        "schema_version": 1,
        "release_run_id": run_id,
        "result": "PASS",
        "checks": [
            {
                "check_id": spec.check_id,
                "result": "PASS",
                "duration_ms": 1,
            }
            for spec in CHECK_SPECS
        ],
        "artifacts": {
            "rust_core": {"sha256": "core"},
            "dexter_cli": {"sha256": "cli"},
            "swift_product": {"sha256": "swift"},
        },
    }


def smoke_evidence(run_id: str, target: str):
    return {
        "schema_version": 1,
        "release_run_id": run_id,
        "result": "PASS",
        "targets": [
            {
                "target": target,
                "result": "PASS",
                "duration_ms": 1,
            }
        ],
    }


class ReleaseGateTests(unittest.TestCase):
    def test_identity_comparison_ignores_capture_time(self) -> None:
        start = identity()
        end = identity()
        end["captured_at"] = "2026-07-30T01:00:00+00:00"
        self.assertEqual(identity_changes(start, end), [])

    def test_source_change_fails_manifest(self) -> None:
        run_id = "run"
        manifest = build_manifest(
            run_id=run_id,
            started_at="start",
            finished_at="finish",
            start_identity=identity("before"),
            end_identity=identity("after"),
            build_evidence=build_evidence(run_id),
            smoke_evidence=[
                ("acceptance", smoke_evidence(run_id, "target-a")),
                ("action_safety_full", smoke_evidence(run_id, "target-b")),
            ],
            gate_errors=[],
        )
        self.assertEqual(manifest["result"], "FAIL")
        self.assertEqual(manifest["identity_changes"], ["source"])

    def test_complete_current_run_is_automated_pass_manual_pending(self) -> None:
        run_id = "run"
        manifest = build_manifest(
            run_id=run_id,
            started_at="start",
            finished_at="finish",
            start_identity=identity(),
            end_identity=identity(),
            build_evidence=build_evidence(run_id),
            smoke_evidence=[
                ("acceptance", smoke_evidence(run_id, "target-a")),
                ("action_safety_full", smoke_evidence(run_id, "target-b")),
            ],
            gate_errors=[],
        )
        self.assertEqual(manifest["result"], "PASS")
        self.assertEqual(
            manifest["release_state"], "AUTOMATED_PASS_MANUAL_PENDING"
        )
        self.assertEqual(manifest["manual_checklist"]["status"], "PENDING")

    def test_child_evidence_must_match_current_run(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "evidence.json"
            path.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "release_run_id": "old-run",
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(Exception, "different run"):
                load_child_evidence(path, run_id="current-run", label="test")

    def test_lock_rejects_concurrent_gate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "gate.lock"
            with gate_lock(path):
                with self.assertRaises(GateAlreadyRunning):
                    with gate_lock(path):
                        pass

    def test_publish_writes_timestamped_and_latest_views(self) -> None:
        run_id = "run"
        manifest = build_manifest(
            run_id=run_id,
            started_at="2026-07-30T00:00:00+00:00",
            finished_at="2026-07-30T00:00:01+00:00",
            start_identity=identity(),
            end_identity=identity(),
            build_evidence=build_evidence(run_id),
            smoke_evidence=[
                ("acceptance", smoke_evidence(run_id, "target-a")),
                ("action_safety_full", smoke_evidence(run_id, "target-b")),
            ],
            gate_errors=[],
        )
        with tempfile.TemporaryDirectory() as directory:
            release_directory = Path(directory)
            json_path, markdown_path = publish_manifest(
                release_directory, manifest
            )
            latest = json.loads(
                (release_directory / "latest.json").read_text(encoding="utf-8")
            )
            markdown = markdown_path.read_text(encoding="utf-8")
        self.assertTrue(json_path.name.startswith("release-evidence-"))
        self.assertEqual(latest["run_id"], run_id)
        self.assertIn("Manual checklist: **not recorded**", markdown)


if __name__ == "__main__":
    unittest.main()
