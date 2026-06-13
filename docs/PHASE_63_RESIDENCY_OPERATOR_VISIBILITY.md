# Phase 63 — Residency Operator Visibility

## Outcome

Dexter's model residency mechanism is now visible through the normal operator
health path instead of being only a daemon log line or one-off proof command.

## Shipped

- `HealthResponse` carries residency mode, PRIMARY pinned state, wired bytes,
  and lock-poison status.
- `dexter-cli --doctor` and `dexter-cli --status` print a `model residency`
  health row.
- `make live-smoke-residency-proof` proves the cross-process mmap+mlock
  mechanism on a safe-sized real model blob (`mxbai-embed-large` by default).
- `make live-smoke-runtime-health`, `make live-smoke-acceptance`, and
  `make live-smoke-all` include the safe residency proof target.
- `docs/OLLAMA_MODEL_STORAGE.md` documents the local/external model split,
  residency modes, operator visibility, and the safe proof command.

## Deliberate Boundary

This does not claim pinning alone eliminates PRIMARY cold-loads. The default
remains `pin_keepalive`: wire PRIMARY and keep the 30-second keepalive backstop.
Moving to `pin_retire_keepalive` still requires a controlled idle-pressure
discriminator proving PRIMARY remains resident while Ollama continues to list it
as loaded.
