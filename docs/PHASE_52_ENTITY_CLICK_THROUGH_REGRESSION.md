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
  - the window-level mouse-event gate enables events only while Dexter is
    intentionally interactive.
- `make live-smoke-placement-command` proves the external
  `scripts/dexter-place.sh` path reaches the running Swift app through
  `DistributedNotificationCenter`.

## Why

The prior smoke only checked a corner and the center. That would catch a square
transparent blocker, but not a vertical or horizontal invisible strip through
the middle of the entity window. The new cardinal-edge samples specifically
guard against that regression.

Phase 53 tightened the same fix at the window-server boundary: the panel now
keeps `ignoresMouseEvents` enabled while the cursor is outside the rendered orb,
then disables it only while the pointer is over Dexter or while an intentional
placement/drag is active. That preserves click-through behavior even when
AppKit would otherwise treat the transparent top-level panel as a rectangle
before view-level hit testing can decline the event.

## Verification

```bash
cd src/swift && swift build
make live-smoke-hud-placement
make live-smoke-placement-command
```
