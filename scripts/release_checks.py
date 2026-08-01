#!/usr/bin/env python3
"""Run DEX-02 build checks and write machine-readable, secret-safe evidence."""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import stat
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Mapping, Sequence

REPO_ROOT = Path(__file__).resolve().parent.parent
if os.fspath(REPO_ROOT) not in sys.path:
    sys.path.insert(0, os.fspath(REPO_ROOT))

from scripts.release_identity import write_json_atomic


@dataclass(frozen=True)
class CheckSpec:
    check_id: str
    argv: tuple[str, ...]
    working_directory: str


@dataclass(frozen=True)
class CheckEvidence:
    check_id: str
    argv: tuple[str, ...]
    working_directory: str
    started_at: str
    finished_at: str
    duration_ms: int
    exit_status: int
    result: str
    diagnostic_summary: str
    log_sha256: str
    log_path: str

    def to_dict(self) -> dict[str, object]:
        result = asdict(self)
        result["argv"] = list(self.argv)
        return result


@dataclass(frozen=True)
class ArtifactEvidence:
    path: str
    sha256: str
    size_bytes: int
    executable: bool


CHECK_SPECS = (
    CheckSpec(
        "rust_unit",
        ("cargo", "test", "-q", "--bin", "dexter-core"),
        "src/rust-core",
    ),
    CheckSpec(
        "rust_release",
        ("cargo", "build", "--release"),
        "src/rust-core",
    ),
    CheckSpec(
        "rust_cli_release",
        ("cargo", "build", "--release", "--bin", "dexter-cli"),
        "src/rust-core",
    ),
    CheckSpec(
        "python_workers",
        ("uv", "run", "pytest"),
        "src/python-workers",
    ),
    CheckSpec("proto_consistency", ("make", "proto-check"), "."),
    CheckSpec(
        "swift_release",
        ("swift", "build", "-c", "release"),
        "src/swift",
    ),
)

SECRET_ASSIGNMENT = re.compile(
    r"(?i)\b(token|password|secret|api[_-]?key)"
    r"(\s*[=:]\s*)(?:\"[^\"]*\"|'[^']*'|[^\s,;]+)"
)
MAX_DIAGNOSTIC_CHARS = 8_000
MAX_DIAGNOSTIC_LINES = 40


def _utc_now() -> str:
    return datetime.now(UTC).isoformat()


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _display_path(path: Path, root: Path) -> str:
    try:
        return path.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return os.fspath(path.resolve())


def redact_diagnostic(output: str) -> str:
    redacted = SECRET_ASSIGNMENT.sub(r"\1\2[REDACTED]", output)
    lines = redacted.splitlines()[-MAX_DIAGNOSTIC_LINES:]
    return "\n".join(lines)[-MAX_DIAGNOSTIC_CHARS:]


def run_check(
    root: Path,
    spec: CheckSpec,
    *,
    log_directory: Path,
) -> CheckEvidence:
    working_directory = (root / spec.working_directory).resolve()
    log_directory.mkdir(parents=True, exist_ok=True)
    log_path = log_directory / f"{spec.check_id}.log"
    started_at = _utc_now()
    started_monotonic = time.monotonic()

    with log_path.open("wb") as log_file:
        try:
            process = subprocess.Popen(
                spec.argv,
                cwd=working_directory,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
        except OSError:
            log_file.write(
                f"{spec.check_id}: executable unavailable: {spec.argv[0]}\n".encode()
            )
            exit_status = 127
        else:
            if process.stdout is None:
                process.kill()
                raise RuntimeError(f"{spec.check_id}: command output pipe unavailable")
            try:
                mirror = os.environ.get("DEXTER_GATE_VERBOSE") == "1"
                with process.stdout:
                    for chunk in iter(lambda: process.stdout.read(64 * 1024), b""):
                        log_file.write(chunk)
                        sys.stdout.buffer.write(chunk)
                        sys.stdout.buffer.flush()
            except BaseException:
                process.terminate()
                process.wait()
                raise
            exit_status = process.wait()
        log_file.flush()
        os.fsync(log_file.fileno())

    duration_ms = round((time.monotonic() - started_monotonic) * 1000)
    output = log_path.read_text(encoding="utf-8", errors="replace")
    return CheckEvidence(
        check_id=spec.check_id,
        argv=spec.argv,
        working_directory=_display_path(working_directory, root),
        started_at=started_at,
        finished_at=_utc_now(),
        duration_ms=duration_ms,
        exit_status=exit_status,
        result="PASS" if exit_status == 0 else "FAIL",
        diagnostic_summary=redact_diagnostic(output),
        log_sha256=_sha256_file(log_path),
        log_path=_display_path(log_path, root),
    )


def hash_artifacts(
    root: Path,
    artifact_paths: Mapping[str, Path],
) -> dict[str, ArtifactEvidence]:
    evidence: dict[str, ArtifactEvidence] = {}
    for name, path in sorted(artifact_paths.items()):
        resolved = path.resolve()
        before = resolved.stat()
        if not stat.S_ISREG(before.st_mode):
            raise RuntimeError(f"release artifact is not a regular file: {resolved}")
        sha256 = _sha256_file(resolved)
        after = resolved.stat()
        before_identity = (
            before.st_dev,
            before.st_ino,
            before.st_mode,
            before.st_size,
            before.st_mtime_ns,
            before.st_ctime_ns,
        )
        after_identity = (
            after.st_dev,
            after.st_ino,
            after.st_mode,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
        )
        if before_identity != after_identity:
            raise RuntimeError(f"release artifact changed while hashing: {resolved}")
        evidence[name] = ArtifactEvidence(
            path=_display_path(resolved, root),
            sha256=sha256,
            size_bytes=before.st_size,
            executable=bool(
                before.st_mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
            ),
        )
    return evidence


def _swift_product(root: Path) -> Path:
    candidates = {
        candidate.resolve()
        for candidate in (root / "src/swift/.build").glob("*/release/Dexter")
        if candidate.is_file()
    }
    direct = root / "src/swift/.build/release/Dexter"
    if direct.is_file():
        candidates.add(direct.resolve())
    if len(candidates) != 1:
        rendered = ", ".join(sorted(os.fspath(path) for path in candidates)) or "none"
        raise RuntimeError(f"expected one Swift release product, found: {rendered}")
    return candidates.pop()


def collect_release_artifacts(root: Path) -> dict[str, ArtifactEvidence]:
    return hash_artifacts(
        root,
        {
            "rust_core": root / "src/rust-core/target/release/dexter-core",
            "dexter_cli": root / "src/rust-core/target/release/dexter-cli",
            "swift_product": _swift_product(root),
        },
    )


def _default_output(root: Path, stamp: str) -> Path:
    return (
        root
        / "docs/live-smoke-results/release/checks"
        / f"build-checks-{stamp}.json"
    )


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run DEX-02 release build checks and emit JSON evidence."
    )
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--log-dir", type=Path)
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    root = args.root.resolve()
    stamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    output = args.output or _default_output(root, stamp)
    log_directory = args.log_dir or output.parent / f"{output.stem}-logs"
    started_at = _utc_now()
    checks: list[CheckEvidence] = []

    for spec in CHECK_SPECS:
        check = run_check(root, spec, log_directory=log_directory)
        checks.append(check)
        if check.result != "PASS":
            break

    artifacts: dict[str, ArtifactEvidence] = {}
    artifact_error: str | None = None
    if len(checks) == len(CHECK_SPECS) and all(
        check.result == "PASS" for check in checks
    ):
        try:
            artifacts = collect_release_artifacts(root)
        except (OSError, RuntimeError) as error:
            artifact_error = str(error)

    result = (
        "PASS"
        if len(checks) == len(CHECK_SPECS)
        and all(check.result == "PASS" for check in checks)
        and artifact_error is None
        else "FAIL"
    )
    payload: dict[str, object] = {
        "schema_version": 1,
        "release_run_id": os.environ.get("DEXTER_RELEASE_RUN_ID"),
        "started_at": started_at,
        "finished_at": _utc_now(),
        "result": result,
        "checks": [check.to_dict() for check in checks],
        "artifacts": {
            name: asdict(artifact) for name, artifact in artifacts.items()
        },
        "artifact_error": redact_diagnostic(artifact_error or "") or None,
    }
    write_json_atomic(output, payload)
    print(f"release check evidence: {output}")
    return 0 if result == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
