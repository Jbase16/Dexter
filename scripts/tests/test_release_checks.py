from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

from scripts.release_checks import (
    CheckSpec,
    hash_artifacts,
    redact_diagnostic,
    run_check,
)


class ReleaseCheckTests(unittest.TestCase):
    def test_successful_command_records_exact_argv_and_log_hash(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = run_check(
                root,
                CheckSpec(
                    "example",
                    (sys.executable, "-c", "print('checked')"),
                    ".",
                ),
                log_directory=root / "logs",
            )
        self.assertEqual(evidence.result, "PASS")
        self.assertEqual(evidence.exit_status, 0)
        self.assertEqual(
            evidence.argv, (sys.executable, "-c", "print('checked')")
        )
        self.assertEqual(len(evidence.log_sha256), 64)

    def test_failed_command_is_recorded_as_fail(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = run_check(
                root,
                CheckSpec("failure", (sys.executable, "-c", "raise SystemExit(7)"), "."),
                log_directory=root / "logs",
            )
        self.assertEqual(evidence.result, "FAIL")
        self.assertEqual(evidence.exit_status, 7)

    def test_missing_executable_is_structured_fail_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence = run_check(
                root,
                CheckSpec("missing", ("dexter-command-that-does-not-exist",), "."),
                log_directory=root / "logs",
            )
        self.assertEqual(evidence.result, "FAIL")
        self.assertEqual(evidence.exit_status, 127)
        self.assertIn("executable unavailable", evidence.diagnostic_summary)

    def test_diagnostic_redacts_secret_assignments(self) -> None:
        diagnostic = redact_diagnostic("token=abc password: hunter2 ordinary output")
        self.assertNotIn("abc", diagnostic)
        self.assertNotIn("hunter2", diagnostic)
        self.assertIn("ordinary output", diagnostic)

    def test_artifact_hashes_record_path_size_and_executable_bit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact = root / "artifact"
            artifact.write_bytes(b"release bytes")
            artifact.chmod(0o755)
            evidence = hash_artifacts(root, {"example": artifact})["example"]
        self.assertEqual(evidence.path, "artifact")
        self.assertEqual(evidence.size_bytes, 13)
        self.assertTrue(evidence.executable)
        self.assertEqual(len(evidence.sha256), 64)


if __name__ == "__main__":
    unittest.main()
