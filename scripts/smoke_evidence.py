#!/usr/bin/env python3
"""Render live-smoke target records as atomic machine-readable evidence."""

from __future__ import annotations

import argparse
import hashlib
import os
import sys
from datetime import UTC, datetime
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
if os.fspath(REPO_ROOT) not in sys.path:
    sys.path.insert(0, os.fspath(REPO_ROOT))

from scripts.release_identity import write_json_atomic


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_target_records(results_tsv: Path, root: Path) -> list[dict[str, object]]:
    targets: list[dict[str, object]] = []
    for line_number, line in enumerate(
        results_tsv.read_text(encoding="utf-8").splitlines(), start=1
    ):
        fields = line.split("\t")
        if len(fields) != 7:
            raise ValueError(f"invalid target result record at line {line_number}")
        (
            target,
            result,
            duration_seconds,
            exit_status,
            raw_log_path,
            started_at,
            finished_at,
        ) = fields
        if result not in {"PASS", "FAIL"}:
            raise ValueError(f"invalid target result at line {line_number}: {result}")
        log_path = Path(raw_log_path)
        if not log_path.is_file():
            raise ValueError(f"target log is missing: {log_path}")
        try:
            displayed_log = log_path.resolve().relative_to(root.resolve()).as_posix()
        except ValueError:
            displayed_log = os.fspath(log_path.resolve())
        targets.append(
            {
                "target": target,
                "argv": ["make", target],
                "result": result,
                "started_at": started_at,
                "finished_at": finished_at,
                "duration_ms": int(duration_seconds) * 1000,
                "exit_status": int(exit_status),
                "log_sha256": _sha256_file(log_path),
                "log_path": displayed_log,
            }
        )
    return targets


def build_smoke_payload(
    *,
    root: Path,
    results_tsv: Path,
    started_at: str,
    finished_at: str,
    duration_seconds: int,
    mode: str,
    stop_reason: str,
) -> dict[str, object]:
    targets = load_target_records(results_tsv, root)
    passed = sum(target["result"] == "PASS" for target in targets)
    failed = len(targets) - passed
    return {
        "schema_version": 1,
        "release_run_id": os.environ.get("DEXTER_RELEASE_RUN_ID"),
        "generated_at": datetime.now(UTC).isoformat(),
        "started_at": started_at,
        "finished_at": finished_at,
        "duration_ms": duration_seconds * 1000,
        "root": os.fspath(root.resolve()),
        "result": "PASS" if failed == 0 else "FAIL",
        "mode": mode,
        "stop_reason": stop_reason,
        "passed": passed,
        "failed": failed,
        "targets": targets,
    }


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--results-tsv", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--latest", type=Path, required=True)
    parser.add_argument("--started-at", required=True)
    parser.add_argument("--finished-at", required=True)
    parser.add_argument("--duration-seconds", type=int, required=True)
    parser.add_argument("--mode", choices=("fail-fast", "continue-on-failure"), required=True)
    parser.add_argument("--stop-reason", required=True)
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    payload = build_smoke_payload(
        root=args.root,
        results_tsv=args.results_tsv,
        started_at=args.started_at,
        finished_at=args.finished_at,
        duration_seconds=args.duration_seconds,
        mode=args.mode,
        stop_reason=args.stop_reason,
    )
    write_json_atomic(args.output, payload)
    write_json_atomic(args.latest, payload)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
