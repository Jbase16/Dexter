# Phase 56 — Action Safety Acceptance Target

## Goal

Make Dexter's action-system readiness easy to prove without running the full
live regression suite.

## Changes

- Added `make live-smoke-action-safety`.
- The target runs a focused summary pass over:
  - external failure handling;
  - latest-action diagnostics;
  - shell/file/browser/AppleScript policy matrix;
  - action receipts;
  - approval lifecycle;
  - HUD action history;
  - HUD action diagnostics;
  - HUD approval denial;
  - long-lived action cancellation.
- Documented the target in `docs/DEXTER_OPERATOR_CONTROLS.md`.

## Non-goals

The target does not run the opt-in Contacts/iMessage smokes. Those depend on
local Contacts state and the approve variant can send a real message, so they
remain explicit one-off operator tests.

## Verification

```bash
make live-smoke-action-safety
make diagnostic-bundle
make smoke
```
