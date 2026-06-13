# Phase 54 — HUD Unavailable Health Recovery

## Goal

When the Swift HUD is alive but the Rust core is unreachable, the health surface
should tell the operator how to recover. A raw connection error is not enough.

## Changes

- `DexterClient.unavailableHealthMarkdown(reason:)` now includes:
  - Restart Dexter from the HUD or Dexter menu;
  - terminal fallbacks for `make open-app` and `make run`.
- Added `make live-smoke-hud-unavailable-health`.
- Added `make live-smoke-runtime-health` as the focused acceptance target for
  startup readiness, CLI status, HUD health/recovery, and no-core HUD recovery.
- Added `scripts/live-hud-unavailable-health-smoke.sh`, which launches the real
  Swift HUD without a Rust core and verifies the unavailable-health markdown is
  rendered with actionable recovery copy.

## Verification

```bash
bash -n scripts/live-hud-unavailable-health-smoke.sh
make live-smoke-hud-unavailable-health
make live-smoke-runtime-health
make smoke
```
