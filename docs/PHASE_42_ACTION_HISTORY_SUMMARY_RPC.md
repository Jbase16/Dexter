# Phase 42 Action History Summary RPC

## Goal

Move latest-action summary formatting behind the daemon boundary so Swift does
not reimplement receipt diagnosis rules.

## Outcome

Complete.

`ActionHistoryResponse` now carries `latest_action_summary_markdown`. The Rust
daemon formats that summary from the same receipt evidence used by status and
diagnostics, then the Swift HUD renders the daemon-owned markdown.

This keeps privileged/action semantics in the Rust core. Swift remains a UI
client: it requests action history and displays what the daemon says.

## Evidence

Proto regeneration:

```text
make proto: PASS
```

Targeted Rust tests:

```text
cargo test --bin dexter-core latest_action_summary_markdown: PASS, 3 passed
```

Swift compile:

```text
cd src/swift && swift build: PASS
```

Focused live receipt:

```text
docs/live-smoke-results/live-smoke-20260527_204928.md
```

Relevant targets:

```text
live-smoke-operator-status: PASS
live-smoke-hud-health: PASS
live-smoke-hud-action-history: PASS
```

## Remaining Work

None for the RPC checkpoint. Follow-up phases can improve wording in the Rust
helper without touching Swift formatting logic.
