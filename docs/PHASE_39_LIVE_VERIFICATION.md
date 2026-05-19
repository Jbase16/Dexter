# Phase 39 Live Verification Checkpoint

## Context

Phase 39 is a checkpoint phase, not a feature phase. The goal is to record the
first broad live-verification pass after the action policy, CLI smoke harness,
Humor Engine, Contacts message path, HUD approval path, action cancellation, and
barge-in regressions were all exercised together.

This document is intentionally evidence-oriented. It captures what was run, what
passed, and what remains outside the checkpoint.

Checkpoint date: 2026-05-16.

---

## Outcome

The live stack passed.

The following checks completed successfully:

```bash
bash -n scripts/live-cli-smoke.sh
cd src/rust-core && cargo fmt --check
cd src/rust-core && cargo test --bin dexter-core --no-fail-fast
make live-smoke-action-matrix
make live-smoke-cli
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact
make live-smoke-hud-approval
make live-smoke-hud
make live-smoke-action-cancel
make live-smoke-barge-in
make live-smoke-all
make smoke
```

Observed results:

| Check | Result | Notes |
|---|---:|---|
| `bash -n scripts/live-cli-smoke.sh` | PASS | Shell smoke harness syntax valid. |
| `cargo fmt --check` | PASS | Rust formatting clean. |
| `cargo test --bin dexter-core --no-fail-fast` | PASS | 584 passed, 7 ignored. |
| `make live-smoke-action-matrix` | PASS | Deterministic action matrix green. |
| `make live-smoke-cli` | PASS | Natural-language CLI live smoke green. |
| `make live-smoke-message-contact` | PASS | Contacts-backed message path reached approval and auto-denied. |
| `make live-smoke-hud-approval` | PASS | Real Swift HUD destructive-action approval smoke green. |
| `make live-smoke-hud` | PASS | Real Swift HUD baseline lifecycle smoke green. |
| `make live-smoke-action-cancel` | PASS | Long-running action subprocess killed on hotkey cancel. |
| `make live-smoke-barge-in` | PASS | Swift/TTS barge-in smoke green. |
| `make live-smoke-all` | PASS | Full live suite green, each target with its own daemon. |
| `make smoke` | PASS | Rust check, Swift build, and Python worker tests green. |

After the live smoke runs, `/tmp/dexter.sock` was checked and no Dexter daemon
was left accepting connections.

---

## What This Proves

### Model routing and Humor Engine

`make live-smoke-cli` verifies:

- Dirty joke follow-ups remain in the Humor Engine.
- Identity joke variation follow-ups remain in the Humor Engine.
- `step-dad joke` is treated as a literal dad-joke request, while explicit
  NSFW dad-joke phrasing routes as dirty humor.
- Normal chat still uses the standard router and does not get captured by the
  Humor Engine.

### Natural-language action emission

`make live-smoke-cli` still includes model-emission coverage for:

- Safe shell action result surfacing.
- Destructive shell action approval and denial.
- Destructive shell action approval and execution.
- Browser action health.
- Routine browser navigation/type/extract.
- Destructive browser click approval and denial.
- Destructive browser click approval and execution.

This is intentionally retained because deterministic action tests cannot prove
that the local model can still emit usable action blocks.

### Deterministic Rust-side action boundary

`make live-smoke-action-matrix` now drives exact `ActionSpec` JSON through
`dexter-cli --action-json`.

This verifies the Rust policy/approval/execution boundary without relying on a
local model to generate the correct JSON shape.

Covered action lanes:

- Shell:
  - safe shell executes immediately and surfaces output
  - destructive shell auto-denies without execution
  - destructive shell auto-approves and executes only the reviewed command
- File:
  - `file_read` executes immediately and surfaces content
  - cautious temp `file_write` executes immediately
  - protected `file_write` auto-denies without mutation
  - destructive temp `file_write` auto-approves and mutates only the fixture
- Browser:
  - navigate/type/extract execute without approval
  - destructive click auto-denies without DOM mutation
  - destructive click auto-approves and mutates only the local fixture
- AppleScript:
  - benign script executes without approval
  - destructive script auto-denies without mutation
  - destructive script auto-approves and mutates only the fixture

### Contacts-backed message send

`DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact`
verifies:

- Dexter uses Contacts-backed recipient resolution.
- A known contact reaches the message approval path.
- Auto-deny prevents sending.

This is deliberately not implemented as synthetic `ActionSpec` JSON. Directly
injecting a `message_send` action would bypass the orchestrator's Contacts
resolution and recipient-integrity path, which is the security-sensitive part
being tested.

### Swift HUD lifecycle and approval parity

`make live-smoke-hud` verifies the real Swift HUD can launch, connect to the
release Rust core, submit a typed turn through the HUD path, and observe the
expected client/core lifecycle.

`make live-smoke-hud-approval` verifies the real Swift HUD receives and handles a
destructive action approval request without executing it in deny mode.

### Cancellation and barge-in

`make live-smoke-action-cancel` verifies a long-running action subprocess is
actually killed after hotkey cancellation. This is stronger than checking that
Dexter merely changes UI state.

`make live-smoke-barge-in` verifies the Swift/TTS interruption path does not
leave audio buffers or entity state stuck after barge-in.

### Broad build/runtime sanity

`make smoke` verifies:

- Rust core type-checks with `cargo check`.
- Swift builds.
- Python worker tests pass.

---

## What This Does Not Prove

This checkpoint does not claim:

- A real approved iMessage was sent during this exact checkpoint. The Contacts
  smoke was run in deny mode. A real-send path had been exercised previously,
  but this document records only this checkpoint's commands.
- Long-duration soak reliability. The suite starts and stops multiple daemons,
  but it is not an all-day memory, worker, or thermal stability test.
- Audio input correctness through the real microphone. Barge-in and TTS
  interruption are covered; full STT recognition quality is not.
- External website reliability. Browser tests use controlled fixtures where
  appropriate to avoid confusing external-network flakiness with Dexter bugs.
- Security review completeness. The action boundary is regression-tested, but
  this is not a full threat-model audit.

---

## Why `--action-json` Exists

The old action smoke path asked the model to emit an action, then checked whether
the Rust core handled it correctly. That mixed two different questions:

1. Can the model emit a usable action block?
2. Does Rust correctly classify, gate, execute, approve, deny, and report that
   action?

`dexter-cli --action-json` separates those questions. It sends an exact
`ActionSpec` through a dev-only synthetic `UIAction` envelope:

```json
{
  "source": "dexter-cli",
  "kind": "action_json",
  "action_json": {
    "type": "shell",
    "args": ["echo", "hi"]
  }
}
```

The orchestrator parses that envelope, validates the inner `ActionSpec`, then
submits it to the same `ActionEngine` policy and execution path used for model
actions.

This path is intentionally not exposed through Swift UI. It exists to make live
regression tests deterministic.

Natural-language action smokes remain in `make live-smoke-cli` so model action
emission is still covered separately.

---

## Verification Commands For Future Re-Runs

Fast local confidence:

```bash
make smoke
```

Rust-only confidence:

```bash
cd src/rust-core && cargo test --bin dexter-core --no-fail-fast
```

Deterministic action boundary:

```bash
make live-smoke-action-matrix
```

Main live CLI behavior:

```bash
make live-smoke-cli
```

Contacts-backed message approval without sending:

```bash
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact
```

Real Swift HUD approval:

```bash
make live-smoke-hud-approval
```

Full live suite:

```bash
make live-smoke-all
```

---

## Next Phase Candidate

The next useful phase should be observability and operator diagnostics, not more
policy surface area.

Recommended focus:

- Startup health summary: model warm state, worker availability, browser/TTS/STT
  status, and degraded-mode reasons.
- Operator-facing diagnostic command: "why didn't Dexter act?" or equivalent.
- Clearer local logs for action refusal causes, Contacts resolution failures,
  worker degradation, and off-host refusal.
- Optional smoke summary artifact written to a local file after live suites.

The policy gate and primary live workflows are now covered well enough to move
from "can this path survive?" to "can the operator understand what happened?"
