# Phase 41 Shared Action Evidence

## Goal

Stop duplicating action-failure explanation logic across the CLI, daemon, and
HUD. Dexter should have one Rust-owned source of truth for local action evidence
so operator-facing surfaces cannot drift silently.

## Outcome

Complete.

Audited action evidence is centralized in the Rust action-evidence path. The
daemon `ActionDiagnostic` path and the `dexter-cli --why` offline fallback both
use that shared logic. The evidence includes the action target, receipt outcome,
human-readable cause, and a concrete next step when one is available.

This phase deliberately does not add new censorship or broad deny rules. It
only explains what the Rust action system already decided or observed.

## Evidence

Targeted Rust coverage:

```text
cargo test --bin dexter-core latest_action_summary_markdown: PASS
```

Full local coverage:

```text
cargo test --bin dexter-core: PASS, 624 passed, 7 ignored
cargo test --bin dexter-cli: PASS, 48 passed
```

Live receipt:

```text
docs/live-smoke-results/live-smoke-20260527_204928.md
```

Relevant target:

```text
live-smoke-operator-status: PASS
```

## Remaining Work

None for the shared-evidence checkpoint. New action types should extend the
Rust helper first and then expose the resulting copy through existing RPCs.
