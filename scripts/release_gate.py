#!/usr/bin/env python3
"""DEX-02 identity-bound Daily-driver v1 release gate."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import subprocess
import sys
import uuid
from contextlib import contextmanager
from datetime import UTC, datetime
from pathlib import Path
from typing import Iterator, Mapping, Sequence

REPO_ROOT = Path(__file__).resolve().parent.parent
if os.fspath(REPO_ROOT) not in sys.path:
    sys.path.insert(0, os.fspath(REPO_ROOT))

from scripts.release_checks import CHECK_SPECS
from scripts.release_attestation import CHECKLIST_VERSION
from scripts.release_identity import (
    ReleaseIdentity,
    atomic_write_text,
    collect_release_identity,
    write_json_atomic,
)

SCHEMA_VERSION = 1
REQUIRED_ARTIFACTS = frozenset({"rust_core", "dexter_cli", "swift_product"})
SMOKE_BATTERIES = (
    ("acceptance", "live-smoke-acceptance"),
    ("action_safety_full", "live-smoke-action-safety-full"),
)


class GateAlreadyRunning(RuntimeError):
    pass


class GateEvidenceError(RuntimeError):
    pass


def _utc_now() -> str:
    return datetime.now(UTC).isoformat()


@contextmanager
def gate_lock(path: Path) -> Iterator[None]:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+", encoding="utf-8") as lock_file:
        try:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise GateAlreadyRunning(
                f"another Daily-driver v1 gate owns {path}"
            ) from error
        lock_file.seek(0)
        lock_file.truncate()
        lock_file.write(f"pid={os.getpid()} started_at={_utc_now()}\n")
        lock_file.flush()
        os.fsync(lock_file.fileno())
        try:
            yield
        finally:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)


def identity_binding(identity: ReleaseIdentity | Mapping[str, object]) -> dict[str, object]:
    payload = identity.to_dict() if isinstance(identity, ReleaseIdentity) else dict(identity)
    payload.pop("captured_at", None)
    return payload


def identity_changes(
    start: ReleaseIdentity | Mapping[str, object],
    end: ReleaseIdentity | Mapping[str, object],
) -> list[str]:
    start_payload = identity_binding(start)
    end_payload = identity_binding(end)
    fields = (
        "source",
        "config_sha256",
        "config_path",
        "personality_sha256",
        "personality_path",
        "input_sha256",
        "runtime",
    )
    return [
        field for field in fields if start_payload.get(field) != end_payload.get(field)
    ]


def load_child_evidence(
    path: Path,
    *,
    run_id: str,
    label: str,
) -> dict[str, object]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise GateEvidenceError(f"{label} evidence is missing or invalid") from error
    if not isinstance(payload, dict):
        raise GateEvidenceError(f"{label} evidence is not a JSON object")
    if payload.get("schema_version") != SCHEMA_VERSION:
        raise GateEvidenceError(f"{label} evidence has an unsupported schema")
    if payload.get("release_run_id") != run_id:
        raise GateEvidenceError(f"{label} evidence belongs to a different run")
    return payload


def _valid_build_evidence(build: Mapping[str, object]) -> bool:
    if build.get("result") != "PASS":
        return False
    raw_checks = build.get("checks")
    raw_artifacts = build.get("artifacts")
    if not isinstance(raw_checks, list) or not isinstance(raw_artifacts, dict):
        return False
    required_checks = {spec.check_id for spec in CHECK_SPECS}
    passed_checks = {
        check.get("check_id")
        for check in raw_checks
        if isinstance(check, dict) and check.get("result") == "PASS"
    }
    return required_checks <= passed_checks and REQUIRED_ARTIFACTS <= set(raw_artifacts)


def _valid_smoke_evidence(smoke: Mapping[str, object]) -> bool:
    targets = smoke.get("targets")
    return (
        smoke.get("result") == "PASS"
        and isinstance(targets, list)
        and bool(targets)
        and all(
            isinstance(target, dict) and target.get("result") == "PASS"
            for target in targets
        )
    )


def build_manifest(
    *,
    run_id: str,
    started_at: str,
    finished_at: str,
    start_identity: ReleaseIdentity | Mapping[str, object],
    end_identity: ReleaseIdentity | Mapping[str, object],
    build_evidence: Mapping[str, object] | None,
    smoke_evidence: Sequence[tuple[str, Mapping[str, object]]],
    gate_errors: Sequence[str],
) -> dict[str, object]:
    start = identity_binding(start_identity)
    end = identity_binding(end_identity)
    changes = identity_changes(start, end)
    build = dict(build_evidence or {})
    artifacts = build.get("artifacts")
    if not isinstance(artifacts, dict):
        artifacts = {}

    acceptance_targets: list[dict[str, object]] = []
    for battery, evidence in smoke_evidence:
        targets = evidence.get("targets")
        if not isinstance(targets, list):
            continue
        for target in targets:
            if isinstance(target, dict):
                acceptance_targets.append({"battery": battery, **target})

    runtime = start.get("runtime")
    runtime_compatible = (
        isinstance(runtime, dict)
        and runtime.get("ollama_api_compatibility") == "PASS"
    )
    smoke_complete = (
        len(smoke_evidence) == len(SMOKE_BATTERIES)
        and all(_valid_smoke_evidence(evidence) for _, evidence in smoke_evidence)
    )
    result = (
        "PASS"
        if not gate_errors
        and not changes
        and runtime_compatible
        and _valid_build_evidence(build)
        and smoke_complete
        else "FAIL"
    )

    source = end.get("source")
    start_source = start.get("source")
    if not isinstance(source, dict):
        source = {}
    if not isinstance(start_source, dict):
        start_source = {}

    def artifact_hash(name: str) -> object:
        artifact = artifacts.get(name)
        return artifact.get("sha256") if isinstance(artifact, dict) else None

    return {
        "schema_version": SCHEMA_VERSION,
        "run_id": run_id,
        "started_at": started_at,
        "finished_at": finished_at,
        "result": result,
        "release_state": (
            "AUTOMATED_PASS_MANUAL_PENDING"
            if result == "PASS"
            else "AUTOMATED_FAIL"
        ),
        "identity": {
            "source_tree_sha256": source.get("sha256"),
            "source_tree_file_count": source.get("file_count"),
            "source_tree_start_sha256": start_source.get("sha256"),
            "source_tree_end_sha256": source.get("sha256"),
            "config_sha256": end.get("config_sha256"),
            "personality_sha256": end.get("personality_sha256"),
            "input_sha256": end.get("input_sha256"),
            "rust_core_binary_sha256": artifact_hash("rust_core"),
            "dexter_cli_binary_sha256": artifact_hash("dexter_cli"),
            "swift_product_sha256": artifact_hash("swift_product"),
        },
        "runtime": runtime,
        "checks": build.get("checks", []),
        "artifacts": artifacts,
        "acceptance_targets": acceptance_targets,
        "identity_changes": changes,
        "gate_errors": list(gate_errors),
        "manual_checklist": {
            "version": CHECKLIST_VERSION,
            "status": "PENDING",
            "attested_at": None,
        },
    }


def render_manifest_markdown(manifest: Mapping[str, object]) -> str:
    identity = manifest.get("identity")
    checks = manifest.get("checks")
    targets = manifest.get("acceptance_targets")
    if not isinstance(identity, dict):
        identity = {}
    if not isinstance(checks, list):
        checks = []
    if not isinstance(targets, list):
        targets = []

    lines = [
        "# Dexter Daily-driver v1 Release Evidence",
        "",
        f"- Run ID: `{manifest.get('run_id')}`",
        f"- Started: `{manifest.get('started_at')}`",
        f"- Finished: `{manifest.get('finished_at')}`",
        f"- Automated result: **{manifest.get('result')}**",
        f"- Release state: `{manifest.get('release_state')}`",
        "- Manual checklist: **not recorded**",
        f"- Source SHA-256: `{identity.get('source_tree_sha256')}`",
        "",
        "## Build and Unit Checks",
        "",
        "| Check | Result | Duration |",
        "|---|---:|---:|",
    ]
    for check in checks:
        if not isinstance(check, dict):
            continue
        lines.append(
            f"| `{check.get('check_id')}` | {check.get('result')} | "
            f"`{check.get('duration_ms')} ms` |"
        )
    lines.extend(
        [
            "",
            "## Acceptance Targets",
            "",
            "| Battery | Target | Result | Duration |",
            "|---|---|---:|---:|",
        ]
    )
    for target in targets:
        if not isinstance(target, dict):
            continue
        lines.append(
            f"| `{target.get('battery')}` | `{target.get('target')}` | "
            f"{target.get('result')} | `{target.get('duration_ms')} ms` |"
        )
    changes = manifest.get("identity_changes")
    errors = manifest.get("gate_errors")
    if isinstance(changes, list) and changes:
        lines.extend(["", "## Identity Changes", ""])
        lines.extend(f"- `{change}`" for change in changes)
    if isinstance(errors, list) and errors:
        lines.extend(["", "## Gate Errors", ""])
        lines.extend(f"- {error}" for error in errors)
    lines.append("")
    return "\n".join(lines)


def publish_manifest(
    release_directory: Path,
    manifest: Mapping[str, object],
) -> tuple[Path, Path]:
    finished_at = str(manifest["finished_at"])
    timestamp = (
        finished_at.replace("-", "")
        .replace(":", "")
        .replace("+00:00", "Z")
        .replace(".", "")
    )
    run_id = str(manifest["run_id"])
    stem = f"release-evidence-{timestamp}-{run_id}"
    json_path = release_directory / f"{stem}.json"
    markdown_path = release_directory / f"{stem}.md"
    markdown = render_manifest_markdown(manifest)

    write_json_atomic(json_path, manifest)
    atomic_write_text(markdown_path, markdown)
    atomic_write_text(release_directory / "latest.md", markdown)
    write_json_atomic(release_directory / "latest.json", manifest)
    return json_path, markdown_path


def _run_command(
    argv: Sequence[str],
    *,
    root: Path,
    environment: Mapping[str, str],
) -> int:
    try:
        return subprocess.run(
            argv,
            cwd=root,
            env=dict(environment),
            check=False,
        ).returncode
    except OSError:
        return 127


def run_gate(root: Path) -> tuple[int, Path | None]:
    root = root.resolve()
    release_directory = root / "docs/live-smoke-results/release"
    lock_path = release_directory / ".gate.lock"

    with gate_lock(lock_path):
        run_id = str(uuid.uuid4())
        started_at = _utc_now()
        run_directory = release_directory / "runs" / run_id
        run_directory.mkdir(parents=True, exist_ok=False)
        environment = dict(os.environ)
        environment["DEXTER_RELEASE_RUN_ID"] = run_id
        gate_errors: list[str] = []

        start_identity = collect_release_identity(root)
        build_path = run_directory / "build-checks.json"
        build_status = _run_command(
            (
                sys.executable,
                "scripts/release_checks.py",
                "--output",
                os.fspath(build_path),
                "--log-dir",
                os.fspath(run_directory / "build-logs"),
            ),
            root=root,
            environment=environment,
        )
        try:
            build_evidence = load_child_evidence(
                build_path, run_id=run_id, label="build checks"
            )
        except GateEvidenceError as error:
            build_evidence = None
            gate_errors.append(str(error))
        if build_status != 0:
            gate_errors.append(f"build checks command exited {build_status}")
        if build_status != 0 and build_evidence is not None:
            print("Build checks failed; live smokes will not run.", file=sys.stderr)

        smoke_evidence: list[tuple[str, Mapping[str, object]]] = []
        if build_status == 0 and _valid_build_evidence(build_evidence or {}):
            for battery, target in SMOKE_BATTERIES:
                smoke_path = run_directory / f"{battery}.json"
                smoke_environment = dict(environment)
                smoke_environment["DEXTER_SMOKE_SUMMARY_JSON_FILE"] = os.fspath(
                    smoke_path
                )
                smoke_status = _run_command(
                    ("make", target),
                    root=root,
                    environment=smoke_environment,
                )
                if smoke_status != 0:
                    gate_errors.append(
                        f"{battery} smoke command exited {smoke_status}"
                    )
                try:
                    evidence = load_child_evidence(
                        smoke_path,
                        run_id=run_id,
                        label=f"{battery} smoke",
                    )
                except GateEvidenceError as error:
                    gate_errors.append(str(error))
                    continue
                smoke_evidence.append((battery, evidence))

        end_identity = collect_release_identity(root)
        finished_at = _utc_now()
        manifest = build_manifest(
            run_id=run_id,
            started_at=started_at,
            finished_at=finished_at,
            start_identity=start_identity,
            end_identity=end_identity,
            build_evidence=build_evidence,
            smoke_evidence=smoke_evidence,
            gate_errors=gate_errors,
        )
        json_path, _ = publish_manifest(release_directory, manifest)

        print(f"Release evidence: {json_path}")
        print(f"Automated result: {manifest['result']}")
        print("Manual checklist: not recorded")
        _run_command(
            ("bash", "scripts/daily-driver-v1-checklist.sh"),
            root=root,
            environment=environment,
        )
        return (0 if manifest["result"] == "PASS" else 1), json_path


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run the identity-bound Daily-driver v1 release gate."
    )
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    try:
        status, _ = run_gate(args.root)
    except GateAlreadyRunning as error:
        print(f"release gate unavailable: {error}", file=sys.stderr)
        return 2
    return status


if __name__ == "__main__":
    raise SystemExit(main())
