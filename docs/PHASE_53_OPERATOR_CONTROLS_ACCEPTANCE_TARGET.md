# Phase 53 — Operator Controls Acceptance Target

## Goal

Give the operator one focused command for the app-like controls work instead of
requiring a memorized list of lifecycle and placement smokes.

## Command

```bash
make live-smoke-operator-controls
```

## Coverage

The target writes the normal live-smoke markdown receipt while running:

- `live-smoke-dock-launcher`
- `live-smoke-process-control`
- `live-smoke-stop-report`
- `live-smoke-run-loop-lifecycle`
- `live-smoke-stale-swift-stop`
- `live-smoke-hud-lifecycle`
- `live-smoke-hud-placement`
- `live-smoke-placement-command`

## Why

These are the controls that make Dexter usable day to day: Dock launch metadata,
stop/restart, quit, cleanup of stale UI/core processes, click-through geometry,
and external placement command delivery for mouse/gesture tools.

## Verification

```bash
make live-smoke-operator-controls
make smoke
```
