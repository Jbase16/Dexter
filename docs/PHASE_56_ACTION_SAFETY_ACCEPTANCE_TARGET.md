# Phase 56 — Action Safety Acceptance Target

## Goal

Make Dexter's action-system readiness easy to prove without running the full
live regression suite.

## Changes

- Added `make live-smoke-action-safety`.
- Added `make live-smoke-action-safety-shared` as the fast day-to-day lane.
  It starts one release core and runs compatible CLI/action checks against the
  shared daemon.
- Added `make live-smoke-action-safety-full` for the full HUD/model-driven
  browser recovery sweep.
- The isolated `make live-smoke-action-safety` target runs a focused fail-fast
  summary pass over:
  - external failure handling;
  - latest-action diagnostics;
  - shell/file/browser/AppleScript policy matrix;
  - window/UI action lanes;
  - deterministic browser recovery evidence;
  - action receipts;
  - approval lifecycle;
  - long-lived action cancellation.
- Documented the target in `docs/DEXTER_OPERATOR_CONTROLS.md`.

## Non-goals

The target does not run the opt-in Contacts/iMessage smokes. Those depend on
local Contacts state and the approve variant can send a real message, so they
remain explicit one-off operator tests.

## Verification

```bash
make live-smoke-action-safety-shared
make live-smoke-action-safety
make live-smoke-action-safety-full
make diagnostic-bundle
make smoke
```
