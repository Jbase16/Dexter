# Phase 59 — Operator Ready Acceptance Guidance

## Goal

Make `make operator-ready` point the operator toward the current acceptance
workflow after it finishes preparing the machine.

## Changes

- `scripts/operator-ready.sh` now prints:
  - `make acceptance-status`;
  - `make live-smoke-acceptance`;
  - `make live-smoke-operator-controls`;
  - `make live-smoke-runtime-health`;
  - `make live-smoke-action-safety`.
- `scripts/live-operator-ready-smoke.sh` now captures the command output and
  verifies those guidance lines are present.

## Why

Operator readiness used to point only at the controls slice. Dexter now has
separate runtime-health and action-safety evidence, plus a combined acceptance
battery, so the recovery/setup command should expose that directly.

## Verification

```bash
make live-smoke-operator-ready
make smoke
```
