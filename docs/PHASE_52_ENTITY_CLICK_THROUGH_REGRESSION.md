# Phase 52 — Entity Click-Through Regression

## Goal

Pin the invisible-frame fix to the exact shape that annoyed the operator:
transparent window regions above, below, left, and right of the rendered orb
must pass clicks through to the app underneath.

## Changes

- Placement smoke logging now records:
  - `cornerHit`
  - `topCenterHit`
  - `bottomCenterHit`
  - `leftCenterHit`
  - `rightCenterHit`
  - `centerHit`
- `make live-smoke-hud-placement` now asserts:
  - the entity window remains `136x136`;
  - all transparent edge samples return `false`;
  - the rendered center returns `true`;
  - `isMovableByWindowBackground` remains disabled;
  - the window itself does not set global `ignoresMouseEvents`.
- `make live-smoke-placement-command` proves the external
  `scripts/dexter-place.sh` path reaches the running Swift app through
  `DistributedNotificationCenter`.

## Why

The prior smoke only checked a corner and the center. That would catch a square
transparent blocker, but not a vertical or horizontal invisible strip through
the middle of the entity window. The new cardinal-edge samples specifically
guard against that regression.

## Verification

```bash
cd src/swift && swift build
make live-smoke-hud-placement
make live-smoke-placement-command
```
