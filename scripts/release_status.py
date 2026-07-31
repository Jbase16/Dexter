#!/usr/bin/env python3
"""Consume authoritative DEX-02 release evidence."""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import asdict, dataclass, replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Mapping, Sequence

REPO_ROOT = Path(__file__).resolve().parent.parent
if os.fspath(REPO_ROOT) not in sys.path:
    sys.path.insert(0, os.fspath(REPO_ROOT))

from scripts.release_checks import CHECK_SPECS, collect_release_artifacts
from scripts.release_attestation import (
    MANUAL_INVALIDATED,
    MANUAL_PASS,
    load_manual_status,
)
from scripts.release_identity import SourceIdentityError, collect_release_identity

SCHEMA_VERSION = 1

# A gate PASS is intended to be a same-day release decision, not durable proof
# for a later source/runtime state. Twenty-four hours spans normal overnight
# development while preventing an older successful run from authorizing release.
STRICT_FRESHNESS_HOURS = 24.0

PASS = "PASS"
STALE = "STALE"
MISMATCH = "MISMATCH"
FAIL = "FAIL"
MISSING = "MISSING"
INVALID = "INVALID"

ACCEPTANCE_TARGETS = frozenset(
    {
        "live-smoke-dock-launcher",
        "live-smoke-process-control",
        "live-smoke-stop-report",
        "live-smoke-run-loop-lifecycle",
        "live-smoke-stale-swift-stop",
        "live-smoke-hud-lifecycle",
        "live-smoke-hud-placement",
        "live-smoke-placement-command",
        "live-smoke-residency-proof",
        "live-smoke-startup-readiness",
        "live-smoke-local-answers",
        "live-smoke-operator-status",
        "live-smoke-hud-health",
        "live-smoke-hud-unavailable-health",
        "live-smoke-external-failures",
        "live-smoke-action-diagnostic",
        "live-smoke-context-turn-records",
        "live-smoke-shortcut-action",
        "live-smoke-window-focus",
        "live-smoke-window-inspect",
        "live-smoke-ui-snapshot",
        "live-smoke-ui-click",
        "live-smoke-ui-type",
        "live-smoke-ui-select",
        "live-smoke-ui-toggle",
        "live-smoke-ui-pick",
        "live-smoke-ui-failure-diagnostic",
        "live-smoke-action-matrix",
        "live-smoke-browser-recovery",
        "live-smoke-action-receipts",
        "live-smoke-approval-lifecycle",
        "live-smoke-hud-action-surfaces",
        "live-smoke-hud-ui-failure",
        "live-smoke-hud-approval",
        "live-smoke-action-cancel",
    }
)

ACTION_SAFETY_FULL_TARGETS = frozenset(
    {
        "live-smoke-external-failures",
        "live-smoke-action-diagnostic",
        "live-smoke-context-turn-records",
        "live-smoke-shortcut-action",
        "live-smoke-window-focus",
        "live-smoke-window-inspect",
        "live-smoke-ui-snapshot",
        "live-smoke-ui-click",
        "live-smoke-ui-type",
        "live-smoke-ui-select",
        "live-smoke-ui-toggle",
        "live-smoke-ui-pick",
        "live-smoke-ui-failure-diagnostic",
        "live-smoke-action-matrix",
        "live-smoke-browser-recovery",
        "live-smoke-action-receipts",
        "live-smoke-approval-lifecycle",
        "live-smoke-hud-action-surfaces",
        "live-smoke-hud-ui-failure",
        "live-smoke-hud-approval",
        "live-smoke-action-cancel",
    }
)

REQUIRED_TARGETS_BY_BATTERY = {
    "acceptance": ACCEPTANCE_TARGETS,
    "action_safety_full": ACTION_SAFETY_FULL_TARGETS,
}
REQUIRED_CHECK_IDS = frozenset(spec.check_id for spec in CHECK_SPECS)
REQUIRED_ARTIFACTS = frozenset({"rust_core", "dexter_cli", "swift_product"})


@dataclass(frozen=True)
class StatusReport:
    status: str
    reasons: tuple[str, ...]
    run_id: str | None = None
    finished_at: str | None = None
    age_hours: float | None = None
    automated_result: str | None = None
    manual_status: str | None = None


def _parse_timestamp(value: object) -> datetime:
    if not isinstance(value, str) or not value:
        raise ValueError("finished_at is missing")
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError("finished_at must include a timezone")
    return parsed.astimezone(UTC)


def _base_manifest_error(manifest: Mapping[str, object]) -> str | None:
    if manifest.get("schema_version") != SCHEMA_VERSION:
        return "unsupported or missing schema_version"
    if not isinstance(manifest.get("run_id"), str) or not manifest.get("run_id"):
        return "run_id is missing"
    if manifest.get("result") not in {PASS, FAIL}:
        return "result must be PASS or FAIL"
    try:
        _parse_timestamp(manifest.get("finished_at"))
    except (TypeError, ValueError) as error:
        return str(error)
    if not isinstance(manifest.get("identity"), dict):
        return "identity object is missing"
    if not isinstance(manifest.get("runtime"), dict):
        return "runtime object is missing"
    if not isinstance(manifest.get("checks"), list):
        return "checks array is missing"
    if not isinstance(manifest.get("acceptance_targets"), list):
        return "acceptance_targets array is missing"
    if not isinstance(manifest.get("manual_checklist"), dict):
        return "manual_checklist object is missing"
    return None


def _required_evidence_status(
    manifest: Mapping[str, object],
) -> tuple[str | None, list[str]]:
    checks = manifest["checks"]
    targets = manifest["acceptance_targets"]
    artifacts = manifest.get("artifacts")
    identity = manifest["identity"]
    if not isinstance(identity, dict):
        return INVALID, ["identity object is invalid"]

    check_results: dict[str, object] = {}
    for check in checks if isinstance(checks, list) else []:
        if isinstance(check, dict) and isinstance(check.get("check_id"), str):
            check_results[check["check_id"]] = check.get("result")
    missing_checks = sorted(REQUIRED_CHECK_IDS - set(check_results))
    if missing_checks:
        return MISSING, [f"missing checks: {', '.join(missing_checks)}"]
    failed_checks = sorted(
        check_id
        for check_id in REQUIRED_CHECK_IDS
        if check_results.get(check_id) != PASS
    )
    if failed_checks:
        return FAIL, [f"failed checks: {', '.join(failed_checks)}"]

    target_results: dict[tuple[str, str], object] = {}
    for target in targets if isinstance(targets, list) else []:
        if not isinstance(target, dict):
            continue
        battery = target.get("battery")
        name = target.get("target")
        if isinstance(battery, str) and isinstance(name, str):
            target_results[(battery, name)] = target.get("result")
    missing_targets: list[str] = []
    failed_targets: list[str] = []
    for battery, required_targets in REQUIRED_TARGETS_BY_BATTERY.items():
        for target in sorted(required_targets):
            key = (battery, target)
            if key not in target_results:
                missing_targets.append(f"{battery}/{target}")
            elif target_results[key] != PASS:
                failed_targets.append(f"{battery}/{target}")
    if missing_targets:
        return MISSING, [f"missing targets: {', '.join(missing_targets)}"]
    if failed_targets:
        return FAIL, [f"failed targets: {', '.join(failed_targets)}"]

    if not isinstance(artifacts, dict):
        return MISSING, ["artifacts object is missing"]
    missing_artifacts = sorted(REQUIRED_ARTIFACTS - set(artifacts))
    if missing_artifacts:
        return MISSING, [f"missing artifacts: {', '.join(missing_artifacts)}"]
    missing_hashes = sorted(
        name
        for name in REQUIRED_ARTIFACTS
        if not isinstance(artifacts.get(name), dict)
        or not artifacts[name].get("sha256")
    )
    if missing_hashes:
        return MISSING, [f"missing artifact hashes: {', '.join(missing_hashes)}"]

    start_hash = identity.get("source_tree_start_sha256")
    end_hash = identity.get("source_tree_end_sha256")
    if not start_hash or start_hash != end_hash:
        return FAIL, ["source changed during the gate"]
    if manifest.get("identity_changes"):
        return FAIL, ["identity changed during the gate"]
    if manifest.get("gate_errors"):
        return FAIL, ["gate recorded execution errors"]
    return None, []


def _current_mismatches(
    manifest: Mapping[str, object],
    current_identity: Mapping[str, object],
    current_artifacts: Mapping[str, Mapping[str, object]],
) -> list[str]:
    expected_identity = manifest["identity"]
    expected_runtime = manifest["runtime"]
    if not isinstance(expected_identity, dict) or not isinstance(expected_runtime, dict):
        return ["manifest identity is invalid"]
    current_source = current_identity.get("source")
    if not isinstance(current_source, dict):
        return ["current source identity is unavailable"]

    mismatches: list[str] = []
    comparisons = (
        ("source", expected_identity.get("source_tree_sha256"), current_source.get("sha256")),
        ("config", expected_identity.get("config_sha256"), current_identity.get("config_sha256")),
        (
            "personality",
            expected_identity.get("personality_sha256"),
            current_identity.get("personality_sha256"),
        ),
        ("gate inputs", expected_identity.get("input_sha256"), current_identity.get("input_sha256")),
        ("runtime", expected_runtime, current_identity.get("runtime")),
    )
    for label, expected, current in comparisons:
        if expected != current:
            mismatches.append(label)

    expected_artifacts = manifest.get("artifacts")
    if not isinstance(expected_artifacts, dict):
        mismatches.append("artifacts")
        return mismatches
    for name in sorted(REQUIRED_ARTIFACTS):
        expected = expected_artifacts.get(name)
        current = current_artifacts.get(name)
        expected_hash = expected.get("sha256") if isinstance(expected, dict) else None
        current_hash = current.get("sha256") if isinstance(current, Mapping) else None
        if expected_hash != current_hash:
            mismatches.append(f"artifact:{name}")
    return mismatches


def evaluate_manifest(
    manifest: Mapping[str, object],
    *,
    current_identity: Mapping[str, object],
    current_artifacts: Mapping[str, Mapping[str, object]],
    now: datetime,
    max_age_hours: float = STRICT_FRESHNESS_HOURS,
) -> StatusReport:
    base_error = _base_manifest_error(manifest)
    if base_error:
        return StatusReport(INVALID, (base_error,))

    finished = _parse_timestamp(manifest["finished_at"])
    age = now.astimezone(UTC) - finished
    report_fields = {
        "run_id": str(manifest["run_id"]),
        "finished_at": str(manifest["finished_at"]),
        "age_hours": age.total_seconds() / 3600,
        "automated_result": str(manifest["result"]),
        "manual_status": str(manifest["manual_checklist"].get("status", "UNKNOWN")),
    }
    if age < timedelta(minutes=-5):
        return StatusReport(INVALID, ("finished_at is in the future",), **report_fields)
    if manifest["result"] == FAIL:
        return StatusReport(FAIL, ("latest gate result is FAIL",), **report_fields)

    evidence_status, evidence_reasons = _required_evidence_status(manifest)
    if evidence_status:
        return StatusReport(
            evidence_status, tuple(evidence_reasons), **report_fields
        )

    mismatches = _current_mismatches(
        manifest, current_identity, current_artifacts
    )
    if mismatches:
        return StatusReport(
            MISMATCH,
            (f"current identity differs: {', '.join(mismatches)}",),
            **report_fields,
        )
    if age > timedelta(hours=max_age_hours):
        return StatusReport(
            STALE,
            (f"evidence is older than {max_age_hours:g} hours",),
            **report_fields,
        )
    return StatusReport(PASS, (), **report_fields)


def evaluate_latest(
    release_directory: Path,
    *,
    root: Path,
    now: datetime,
    max_age_hours: float,
) -> StatusReport:
    latest = release_directory / "latest.json"
    if not latest.is_file():
        return StatusReport(
            MISSING,
            ("authoritative latest.json evidence is missing",),
        )
    try:
        payload = json.loads(latest.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return StatusReport(INVALID, ("latest.json cannot be parsed",))
    if not isinstance(payload, dict):
        return StatusReport(INVALID, ("latest.json is not a JSON object",))

    base_error = _base_manifest_error(payload)
    if base_error:
        return StatusReport(INVALID, (base_error,))
    if payload["result"] == FAIL:
        report = evaluate_manifest(
            payload,
            current_identity={},
            current_artifacts={},
            now=now,
            max_age_hours=max_age_hours,
        )
        return replace(
            report,
            manual_status=load_manual_status(payload, release_directory),
        )
    try:
        current_identity = collect_release_identity(root).to_dict()
        current_artifacts = {
            name: asdict(artifact)
            for name, artifact in collect_release_artifacts(root).items()
        }
    except (OSError, RuntimeError, SourceIdentityError) as error:
        report = StatusReport(
            MISMATCH,
            (f"current identity cannot be collected: {type(error).__name__}",),
            run_id=str(payload.get("run_id")),
            finished_at=str(payload.get("finished_at")),
            automated_result=str(payload.get("result")),
            manual_status=str(payload["manual_checklist"].get("status", "UNKNOWN")),
        )
    else:
        report = evaluate_manifest(
            payload,
            current_identity=current_identity,
            current_artifacts=current_artifacts,
            now=now,
            max_age_hours=max_age_hours,
        )
    manual_status = load_manual_status(payload, release_directory)
    if report.status == MISMATCH and manual_status == MANUAL_PASS:
        manual_status = MANUAL_INVALIDATED
    return replace(report, manual_status=manual_status)


def render_report(report: StatusReport, release_directory: Path, strict: bool) -> str:
    lines = [
        "# Dexter Acceptance Status",
        "",
        f"- Evidence: `{release_directory / 'latest.json'}`",
        f"- Strict mode: `{'true' if strict else 'false'}`",
        f"- Automated evidence: **{report.status}**",
        f"- Run ID: `{report.run_id or 'none'}`",
        f"- Finished: `{report.finished_at or 'unknown'}`",
        (
            f"- Age: `{report.age_hours:.2f} hours`"
            if report.age_hours is not None
            else "- Age: `unknown`"
        ),
        f"- Gate result: `{report.automated_result or 'unknown'}`",
        f"- Manual checklist: **{report.manual_status or 'not recorded'}**",
    ]
    if report.reasons:
        lines.extend(["", "## Reasons", ""])
        lines.extend(f"- {reason}" for reason in report.reasons)
    lines.append("")
    return "\n".join(lines)


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Report authoritative Daily-driver v1 release evidence."
    )
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    parser.add_argument(
        "--release-dir",
        type=Path,
        default=REPO_ROOT / "docs/live-smoke-results/release",
    )
    parser.add_argument("--strict", action="store_true")
    parser.add_argument("--max-age-hours", type=float)
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    if args.max_age_hours is not None and args.max_age_hours <= 0:
        print("--max-age-hours must be positive", file=sys.stderr)
        return 2
    max_age = (
        STRICT_FRESHNESS_HOURS
        if args.strict or args.max_age_hours is None
        else args.max_age_hours
    )
    report = evaluate_latest(
        args.release_dir,
        root=args.root,
        now=datetime.now(UTC),
        max_age_hours=max_age,
    )
    print(render_report(report, args.release_dir, args.strict), end="")
    return 0 if not args.strict or report.status == PASS else 1


if __name__ == "__main__":
    raise SystemExit(main())
