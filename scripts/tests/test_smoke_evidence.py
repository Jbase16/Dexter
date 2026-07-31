from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts.smoke_evidence import build_smoke_payload


class SmokeEvidenceTests(unittest.TestCase):
    def test_target_records_become_machine_readable_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logs = root / "logs"
            logs.mkdir()
            passing_log = logs / "pass.log"
            failing_log = logs / "fail.log"
            passing_log.write_text("passed", encoding="utf-8")
            failing_log.write_text("failed", encoding="utf-8")
            results = logs / "targets.tsv"
            results.write_text(
                f"target-pass\tPASS\t2\t0\t{passing_log}"
                "\t2026-07-30T00:00:00+0000\t2026-07-30T00:00:02+0000\n"
                f"target-fail\tFAIL\t3\t9\t{failing_log}"
                "\t2026-07-30T00:00:02+0000\t2026-07-30T00:00:05+0000\n",
                encoding="utf-8",
            )
            payload = build_smoke_payload(
                root=root,
                results_tsv=results,
                started_at="2026-07-30T00:00:00+0000",
                finished_at="2026-07-30T00:00:05+0000",
                duration_seconds=5,
                mode="continue-on-failure",
                stop_reason="completed",
            )
        self.assertEqual(payload["result"], "FAIL")
        self.assertEqual(payload["passed"], 1)
        self.assertEqual(payload["failed"], 1)
        targets = payload["targets"]
        self.assertEqual(targets[0]["argv"], ["make", "target-pass"])
        self.assertEqual(targets[0]["started_at"], "2026-07-30T00:00:00+0000")
        self.assertEqual(targets[0]["exit_status"], 0)
        self.assertEqual(targets[1]["exit_status"], 9)
        self.assertEqual(len(targets[0]["log_sha256"]), 64)


if __name__ == "__main__":
    unittest.main()
