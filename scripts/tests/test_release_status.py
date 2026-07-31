from __future__ import annotations

import json
import tempfile
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path

from scripts.release_checks import CHECK_SPECS
from scripts.release_status import (
    ACTION_SAFETY_FULL_TARGETS,
    ACCEPTANCE_TARGETS,
    FAIL,
    INVALID,
    MISMATCH,
    MISSING,
    PASS,
    STALE,
    evaluate_latest,
    evaluate_manifest,
)

NOW = datetime(2026, 7, 30, 12, 0, tzinfo=UTC)


def current_identity():
    return {
        "source": {"sha256": "source", "file_count": 10},
        "config_sha256": "config",
        "personality_sha256": "personality",
        "input_sha256": {"gate": "input"},
        "runtime": {
            "ollama_api_compatibility": "PASS",
            "models": [{"tag": "model", "digest": "digest", "available": True}],
            "rustc": "rustc",
        },
    }


def current_artifacts():
    return {
        "rust_core": {"sha256": "core"},
        "dexter_cli": {"sha256": "cli"},
        "swift_product": {"sha256": "swift"},
    }


def passing_manifest(*, finished_at: datetime = NOW):
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
    return {
        "schema_version": 1,
        "run_id": "current-run",
        "started_at": (finished_at - timedelta(hours=1)).isoformat(),
        "finished_at": finished_at.isoformat(),
        "result": "PASS",
        "release_state": "AUTOMATED_PASS_MANUAL_PENDING",
        "identity": {
            "source_tree_sha256": "source",
            "source_tree_file_count": 10,
            "source_tree_start_sha256": "source",
            "source_tree_end_sha256": "source",
            "config_sha256": "config",
            "personality_sha256": "personality",
            "input_sha256": {"gate": "input"},
        },
        "runtime": current_identity()["runtime"],
        "checks": [
            {"check_id": spec.check_id, "result": "PASS"}
            for spec in CHECK_SPECS
        ],
        "artifacts": current_artifacts(),
        "acceptance_targets": targets,
        "identity_changes": [],
        "gate_errors": [],
        "manual_checklist": {
            "version": 1,
            "status": "PENDING",
            "attested_at": None,
        },
    }


def evaluate(manifest):
    return evaluate_manifest(
        manifest,
        current_identity=current_identity(),
        current_artifacts=current_artifacts(),
        now=NOW,
    )


class ReleaseStatusTests(unittest.TestCase):
    def test_current_complete_pass_is_accepted(self) -> None:
        self.assertEqual(evaluate(passing_manifest()).status, PASS)

    def test_old_pass_is_stale(self) -> None:
        manifest = passing_manifest(finished_at=NOW - timedelta(hours=25))
        self.assertEqual(evaluate(manifest).status, STALE)

    def test_source_change_is_mismatch(self) -> None:
        manifest = passing_manifest()
        manifest["identity"]["source_tree_sha256"] = "old-source"
        manifest["identity"]["source_tree_start_sha256"] = "old-source"
        manifest["identity"]["source_tree_end_sha256"] = "old-source"
        self.assertEqual(evaluate(manifest).status, MISMATCH)

    def test_config_model_toolchain_and_artifact_changes_are_mismatch(self) -> None:
        cases = []
        config = passing_manifest()
        config["identity"]["config_sha256"] = "old"
        cases.append(config)
        runtime = passing_manifest()
        runtime["runtime"] = {"ollama_api_compatibility": "PASS", "models": []}
        cases.append(runtime)
        artifact = passing_manifest()
        artifact["artifacts"]["rust_core"]["sha256"] = "old"
        cases.append(artifact)
        for manifest in cases:
            with self.subTest(manifest=manifest):
                self.assertEqual(evaluate(manifest).status, MISMATCH)

    def test_missing_required_check_or_target_is_missing(self) -> None:
        missing_check = passing_manifest()
        missing_check["checks"].pop()
        self.assertEqual(evaluate(missing_check).status, MISSING)
        missing_target = passing_manifest()
        missing_target["acceptance_targets"].pop()
        self.assertEqual(evaluate(missing_target).status, MISSING)

    def test_failed_check_cannot_produce_pass(self) -> None:
        manifest = passing_manifest()
        manifest["checks"][0]["result"] = "FAIL"
        self.assertEqual(evaluate(manifest).status, FAIL)

    def test_unsupported_manifest_is_invalid(self) -> None:
        manifest = passing_manifest()
        manifest["schema_version"] = 99
        self.assertEqual(evaluate(manifest).status, INVALID)

    def test_latest_fail_is_not_replaced_by_older_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            release_directory = Path(directory)
            older = passing_manifest(finished_at=NOW - timedelta(hours=1))
            (release_directory / "release-evidence-old.json").write_text(
                json.dumps(older), encoding="utf-8"
            )
            latest = passing_manifest()
            latest["result"] = "FAIL"
            (release_directory / "latest.json").write_text(
                json.dumps(latest), encoding="utf-8"
            )
            report = evaluate_latest(
                release_directory,
                root=release_directory,
                now=NOW,
                max_age_hours=24,
            )
        self.assertEqual(report.status, FAIL)

    def test_markdown_only_evidence_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            release_directory = Path(directory)
            (release_directory / "latest.md").write_text(
                "# PASS", encoding="utf-8"
            )
            report = evaluate_latest(
                release_directory,
                root=release_directory,
                now=NOW,
                max_age_hours=24,
            )
        self.assertEqual(report.status, MISSING)


if __name__ == "__main__":
    unittest.main()
