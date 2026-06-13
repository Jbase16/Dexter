# Phase 62 — Strict Acceptance Status Target

## Goal

Expose a Make target that fails when required acceptance evidence is missing.

## Changes

- Added `make acceptance-status-strict`.
- The target runs `scripts/acceptance-status.sh` with
  `DEXTER_ACCEPTANCE_STRICT=1`.
- `scripts/operator-ready.sh` now prints the strict command alongside the
  human-readable status command.
- `scripts/live-operator-ready-smoke.sh` verifies the strict command is shown.
- Updated `docs/DEXTER_OPERATOR_CONTROLS.md`.

## Verification

```bash
make acceptance-status-strict
make live-smoke-operator-ready
make smoke
```
