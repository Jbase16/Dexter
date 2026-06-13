# Phase 60 — Acceptance Status Combined Row

## Goal

Show the combined acceptance battery explicitly in `make acceptance-status`.

## Changes

- `scripts/acceptance-status.sh` now reports `Main acceptance battery` above
  the three focused slices.
- The combined row looks for a passing receipt containing the full union of
  operator controls, runtime health, and action safety targets.
- `scripts/live-acceptance-status-smoke.sh` now includes an isolated combined
  receipt fixture and asserts the combined row is present.
- Updated `docs/DEXTER_OPERATOR_CONTROLS.md`.

## Verification

```bash
make live-smoke-acceptance-status
make acceptance-status
make smoke
```
