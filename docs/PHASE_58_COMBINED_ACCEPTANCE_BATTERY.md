# Phase 58 — Combined Acceptance Battery

## Goal

Provide one command that generates a fresh live-smoke receipt for Dexter's main
operator-facing readiness surface.

## Changes

- Added `make live-smoke-acceptance`.
- The target runs the union of:
  - operator controls;
  - runtime health;
  - action safety.
- The target calls `scripts/live-smoke-summary.sh` directly with the combined
  target list, so it produces one timestamped receipt instead of nested
  summaries.
- Documented the command in `docs/DEXTER_OPERATOR_CONTROLS.md`.

## Non-goals

The combined battery does not run opt-in Contacts/iMessage send tests, the full
experimental live suite, or broad model/humor CLI coverage. Those remain
separate so the main acceptance command stays focused on app controls, startup
health, and action-side-effect safety.

## Verification

```bash
make -n live-smoke-acceptance
make live-smoke-acceptance
make acceptance-status
make diagnostic-bundle
make smoke
```
