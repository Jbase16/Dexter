#!/usr/bin/env bash
# Print focused Dexter acceptance-slice status from saved live-smoke receipts.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUMMARY_DIR="${DEXTER_SMOKE_SUMMARY_DIR:-$ROOT_DIR/docs/live-smoke-results}"
STRICT="${DEXTER_ACCEPTANCE_STRICT:-0}"

python3 - "$ROOT_DIR" "$SUMMARY_DIR" "$STRICT" <<'PY'
from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

root = Path(sys.argv[1])
summary_dir = Path(sys.argv[2])
strict = sys.argv[3].lower() in {"1", "true", "yes"}


@dataclass(frozen=True)
class Slice:
    name: str
    target: str
    required_targets: tuple[str, ...]


FOCUSED_SLICES = (
    Slice(
        "Operator controls",
        "live-smoke-operator-controls",
        (
            "live-smoke-dock-launcher",
            "live-smoke-process-control",
            "live-smoke-stop-report",
            "live-smoke-run-loop-lifecycle",
            "live-smoke-stale-swift-stop",
            "live-smoke-hud-lifecycle",
            "live-smoke-hud-placement",
            "live-smoke-placement-command",
        ),
    ),
    Slice(
        "Runtime health",
        "live-smoke-runtime-health",
        (
            "live-smoke-residency-proof",
            "live-smoke-startup-readiness",
            "live-smoke-operator-status",
            "live-smoke-hud-health",
            "live-smoke-hud-unavailable-health",
        ),
    ),
    Slice(
        "Action safety",
        "live-smoke-action-safety",
        (
            "live-smoke-external-failures",
            "live-smoke-action-diagnostic",
            "live-smoke-action-matrix",
            "live-smoke-action-receipts",
            "live-smoke-approval-lifecycle",
            "live-smoke-hud-action-history",
            "live-smoke-hud-action-diagnostic",
            "live-smoke-hud-approval",
            "live-smoke-action-cancel",
        ),
    ),
)

SLICES = (
    Slice(
        "Main acceptance battery",
        "live-smoke-acceptance",
        tuple(
            dict.fromkeys(
                target
                for acceptance_slice in FOCUSED_SLICES
                for target in acceptance_slice.required_targets
            )
        ),
    ),
    *FOCUSED_SLICES,
)


@dataclass(frozen=True)
class Summary:
    path: Path
    started: str
    finished: str
    duration: str
    result: str
    targets: frozenset[str]


def markdown_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace("|", "\\|").replace("\n", " ")


def rel(path: Path) -> str:
    try:
        return str(path.relative_to(root))
    except ValueError:
        return str(path)


def read_field(text: str, name: str) -> str:
    match = re.search(rf"^- {re.escape(name)}: `([^`]*)`", text, re.MULTILINE)
    return match.group(1) if match else ""


def parse_summary(path: Path) -> Summary | None:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None

    result = read_field(text, "Result")
    if result not in {"PASS", "FAIL"}:
        return None

    targets = frozenset(re.findall(r"^\| `([^`]+)` \| (?:PASS|FAIL) \|", text, re.MULTILINE))
    return Summary(
        path=path,
        started=read_field(text, "Started"),
        finished=read_field(text, "Finished"),
        duration=read_field(text, "Duration"),
        result=result,
        targets=targets,
    )


def load_summaries() -> list[Summary]:
    if not summary_dir.is_dir():
        return []

    summaries: list[Summary] = []
    for path in summary_dir.glob("live-smoke-*.md"):
        parsed = parse_summary(path)
        if parsed is not None:
            summaries.append(parsed)
    summaries.sort(key=lambda item: (item.started, item.path.name), reverse=True)
    return summaries


def latest_match(summaries: list[Summary], acceptance_slice: Slice) -> Summary | None:
    required = set(acceptance_slice.required_targets)
    for summary in summaries:
        if summary.result == "PASS" and required.issubset(summary.targets):
            return summary
    return None


summaries = load_summaries()
missing = 0

print("# Dexter Acceptance Status")
print()
print(f"- Summary dir: `{markdown_escape(str(summary_dir))}`")
print(f"- Strict mode: `{'true' if strict else 'false'}`")
print()
print("| Slice | Status | Last pass | Duration | Summary | Command |")
print("|---|---:|---|---:|---|---|")

for acceptance_slice in SLICES:
    match = latest_match(summaries, acceptance_slice)
    if match is None:
        missing += 1
        print(
            f"| {acceptance_slice.name} | MISSING |  |  |  | "
            f"`make {acceptance_slice.target}` |"
        )
    else:
        print(
            f"| {acceptance_slice.name} | PASS | `{markdown_escape(match.started)}` | "
            f"`{markdown_escape(match.duration)}` | `{markdown_escape(rel(match.path))}` | "
            f"`make {acceptance_slice.target}` |"
        )

print()
print("## Required Targets")
print()

for acceptance_slice in SLICES:
    print(f"### {acceptance_slice.name}")
    print()
    for target in acceptance_slice.required_targets:
        print(f"- `{target}`")
    print()

if strict and missing:
    sys.exit(1)
PY
