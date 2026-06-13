# Phase 40 Operator Status

## Goal

Make `dexter-cli --status` answer the operator's immediate question:
"what just happened?" without requiring the operator to inspect raw audit logs.

This phase is intentionally observational. It does not add new policy gates and
does not change whether actions are allowed. It makes the latest action receipt,
health snapshot, and recovery hints visible in one operator-facing report.

## Outcome

Complete.

The status path now combines daemon health with recent action receipts. When an
audited action exists, the status report includes a `Latest Action Summary`
section with concrete evidence and next-step copy. The same evidence shape is
used for successful, denied, failed, timed-out, abandoned, expired, shell,
browser, AppleScript, and raw `message_send` receipts.

## Evidence

Focused live receipt:

```text
docs/live-smoke-results/live-smoke-20260527_204928.md
```

Relevant target:

```text
live-smoke-operator-status: PASS
```

Supporting checks from the same pass:

```text
cargo test --bin dexter-core: PASS, 624 passed, 7 ignored
cargo test --bin dexter-cli: PASS, 48 passed
make smoke: PASS
```

## Remaining Work

None for the Phase 40 operator-status checkpoint. Future work should add new
receipt classes to the shared Rust evidence helpers first, then let the CLI and
HUD consume them.
