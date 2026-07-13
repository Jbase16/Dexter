# Phase 64 — Deterministic Local Answers

## Outcome

Dexter's Daily-driver v1 readiness now includes a focused proof that ordinary
operator questions about current Mac state are answered from local evidence, not
model memory or speculation.

## Shipped

- `make live-smoke-local-answers` starts a fresh release core and asks:
  - `what's using so much RAM right now?`
  - `what's using the most CPU right now?`
  - `what do those Dexter Notices mean?`
- The smoke verifies RAM answers include top process RSS, system memory, and a
  macOS source note.
- The smoke verifies CPU answers include top CPU users and explain that CPU is
  an instantaneous process sample.
- The smoke verifies Dexter Notices are explained as local ambient trigger
  notifications and points the operator to `make why` for the underlying action
  receipt.
- `make live-smoke-runtime-health` and `make live-smoke-acceptance` now include
  the local-answers smoke.
- `make acceptance-status` now treats the local-answers smoke as part of the
  Runtime health slice.

## Deliberate Boundary

This does not make Dexter infer arbitrary system state through the model. The
covered path is intentionally deterministic: macOS process/memory telemetry and
local ambient-event state are gathered by the Rust core, then rendered directly
to the operator.

## Verification

```bash
make live-smoke-local-answers
make live-smoke-runtime-health
make acceptance-status
```
