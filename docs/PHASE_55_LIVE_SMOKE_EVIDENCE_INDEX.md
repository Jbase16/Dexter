# Phase 55 — Live Smoke Evidence Index

## Goal

Keep recent live-smoke evidence discoverable after multiple focused acceptance
slices run. `latest.md` is useful, but it is only one moving pointer.

## Changes

- `scripts/live-smoke-summary.sh` now rebuilds
  `docs/live-smoke-results/index.md` after every summary run.
- The index lists the most recent 20 timestamped summaries with:
  - summary path;
  - start time;
  - result;
  - passed and failed counts;
  - duration;
  - target names.
- `scripts/diagnostic-bundle.sh` now embeds the live-smoke index after the
  latest summary.
- `scripts/live-diagnostic-bundle-smoke.sh` verifies the diagnostic bundle
  includes both the latest summary and the recent-summary index.

## Why

Focused acceptance targets are useful only if their receipts remain easy to
find. Without an index, running `make live-smoke-runtime-health` hides the
previous `make live-smoke-operator-controls` receipt from the diagnostic bundle
even though both timestamped files still exist.

## Verification

```bash
bash scripts/live-smoke-summary.sh live-smoke-dock-launcher
make diagnostic-bundle
make live-smoke-diagnostic-bundle
make smoke
```
