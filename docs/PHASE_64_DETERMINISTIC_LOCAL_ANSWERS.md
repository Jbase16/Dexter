# Phase 64 — Deterministic Local Answers

## Outcome

Dexter's Daily-driver v1 readiness now includes a focused proof that ordinary
operator questions about current Mac state are answered from local evidence, not
model memory or speculation. The evidence boundary runs before statistical model
routing, and every persisted turn identifies the exact core process and binary
that served it.

## Shipped

- A centralized `RequiredLocalEvidenceRequest` gate handles status, permissions,
  RAM/CPU process reports, Dexter Notices, HUD controls, and read-only UI/window
  inspection before FAST/PRIMARY routing.
- RAM and CPU refresh follow-ups take a new macOS `top` sample instead of
  repeating prior assistant text.
- Numbered process follow-ups resolve the PID from the saved host report, and a
  later truthfulness challenge rechecks that PID through Rust instead of asking
  a model to defend or retract the measurement.
- Read-only window-title and UI-snapshot requests become typed Rust `ActionSpec`
  operations and return the executor receipt without a model continuation.
- Current visual-description requests require a fresh screenshot, attach it to
  the genuine user message, and demote before generation if capture fails.
- `ContextTurnRecord` now persists a runtime attestation (PID, executable path,
  BLAKE3 executable content identity) and privacy-safe evidence records.
  Screenshot evidence stores only a BLAKE3 payload hash, never image bytes.
- `make live-smoke-local-answers` starts a fresh release core and asks:
  - `what's using so much RAM right now?`
  - `what's using the most CPU right now?`
  - `what do those Dexter Notices mean?`
  - the exact failed-session status, permissions, RAM refresh, CPU, PID,
    truthfulness-challenge, Why-panel, and invented-HUD-command sequence
  - a read-only Safari window inspection
- The smoke verifies RAM answers use Activity Monitor-style process footprint
  from macOS `top`, not misleading `ps` RSS.
- The smoke verifies CPU answers use the second sample from macOS `top`.
- The smoke verifies Dexter Notices are explained as local ambient trigger
  notifications and points the operator to `make why` for the underlying action
  receipt.
- The smoke fails if HUD contract or read-only UI facts reach model routing.
- The smoke verifies the persisted status record matches the PID and executable
  path of the release core it launched.
- `make live-smoke-vision-grounding` is a focused live-model check for the exact
  current-Safari visual request. It requires useful output plus a persisted
  screenshot hash and matching runtime identity.
- `make live-smoke-runtime-health` and `make live-smoke-acceptance` now include
  the local-answers smoke.
- `make acceptance-status` now treats the local-answers smoke as part of the
  Runtime health slice.

## Deliberate Boundary

This does not make Dexter infer arbitrary system state through the model. Typed
Mac state and Accessibility facts are gathered by the Rust core and rendered
directly. Pixel-level descriptions still use the Vision model, but only after a
fresh capture is attached and recorded as turn evidence.

## Verification

```bash
make live-smoke-local-answers
make live-smoke-vision-grounding
make live-smoke-runtime-health
make acceptance-status
```
