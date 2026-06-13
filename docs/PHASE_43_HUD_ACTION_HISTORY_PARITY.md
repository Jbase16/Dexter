# Phase 43 HUD Action History Parity

## Goal

Make the standalone HUD Recent Actions surface show the same latest-action
evidence as the HUD Status surface.

Before this phase, HUD Status consumed the daemon-owned latest summary, but HUD
Action History only listed recent receipts. That left the operator with less
context when opening Recent Actions directly.

## Outcome

Complete.

The HUD Action History markdown now includes:

- `Latest Action Summary`
- daemon-owned success or failure evidence
- `Recent Receipts`
- audit-log path

The Swift smoke harness now asserts those exact strings after seeding a real
safe shell action through `dexter-cli`.

## Evidence

Direct live target:

```text
make live-smoke-hud-action-history: PASS
```

Focused summary receipt:

```text
docs/live-smoke-results/live-smoke-20260527_205917.md
```

Relevant target:

```text
live-smoke-hud-action-history: PASS
```

Additional compile/static checks:

```text
bash -n scripts/live-hud-action-history-smoke.sh: PASS
cd src/swift && swift build: PASS
cargo test --bin dexter-core latest_action_summary_markdown: PASS
```

## Remaining Work

None for HUD Action History parity. The next useful work is a checkpoint phase
that records the current live evidence and leaves the repo in a clean stopped
state.
