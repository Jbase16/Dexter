# Phase 61 — Diagnostic Index Clip

## Goal

Keep the diagnostic bundle's live-smoke index section focused on evidence.

## Changes

- `scripts/diagnostic-bundle.sh` now embeds `docs/live-smoke-results/index.md`
  only up to the index's trailing `## Latest` helper block.
- `scripts/live-diagnostic-bundle-smoke.sh` verifies the diagnostic report does
  not embed that helper command.

## Why

The standalone index includes a useful `## Latest` helper showing how to inspect
`latest.md`. Inside a diagnostic bundle, that nested helper looked like a stray
diagnostic section. The bundle now shows the index table and omits the helper.

## Verification

```bash
make live-smoke-diagnostic-bundle
make diagnostic-bundle
make smoke
```
