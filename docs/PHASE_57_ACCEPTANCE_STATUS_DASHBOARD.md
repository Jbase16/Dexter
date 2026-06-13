# Phase 57 — Acceptance Status Dashboard

## Goal

Turn saved live-smoke receipts into a compact readiness view for the major
focused acceptance slices.

## Changes

- Added `scripts/acceptance-status.sh`.
- Added `make acceptance-status`.
- Added `make live-smoke-acceptance-status` for deterministic parser coverage.
- `scripts/diagnostic-bundle.sh` now embeds acceptance status after the latest
  smoke summary and recent summary index.
- `scripts/live-diagnostic-bundle-smoke.sh` verifies the diagnostic bundle
  includes acceptance status.
- Documented the command in `docs/DEXTER_OPERATOR_CONTROLS.md`.

## Acceptance Slices

The status report looks for the latest passing saved receipt that covers each
slice:

- operator controls;
- runtime health;
- action safety.

It does not start Dexter or generate new evidence. It only summarizes existing
`docs/live-smoke-results/live-smoke-*.md` receipts.

## Verification

```bash
make live-smoke-acceptance-status
make acceptance-status
make live-smoke-diagnostic-bundle
make diagnostic-bundle
make smoke
```
