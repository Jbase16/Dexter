# Phase 49 Pending Health Copy

## Goal

Stop startup warmup from looking like a failure.

Dexter's health model already distinguishes:

- `pending`: startup warmup still in progress;
- `ready`: all required components are ready;
- `degraded`: warmup completed and something still needs attention.

The HUD already hides `Attention:` during pending health and labels non-warm
models as `warming`. The CLI doctor/status path still described pending
components as `attention components`, which made normal warmup look broken in
diagnostic output.

## Outcome

Complete.

`dexter-cli --doctor`, `make doctor`, `make status`, and diagnostic bundles now
describe pending startup components as:

```text
status pending; pending components fast_model,primary_model,stt_worker
```

Actual degraded health still uses attention wording. This keeps operator copy
aligned with the real state machine:

- pending means wait for warmup;
- degraded means fix something;
- ready means Dexter is ready.

## Evidence

Focused test:

```text
cd src/rust-core && cargo test --bin dexter-cli daemon_health_checks_warn_on_pending_snapshot
PASS: 1 passed
```

Full CLI test pass:

```text
cd src/rust-core && cargo test --bin dexter-cli
PASS: 60 passed
```

## Remaining Work

None for this phase.

If future health surfaces add new labels, keep this split intact: pending
components are not failures until startup warmup has completed.
