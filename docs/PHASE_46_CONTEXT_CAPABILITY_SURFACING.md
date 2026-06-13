# Phase 46 Context Capability Surfacing

## Goal

Show the operator what Dexter can do from the current observed context without
pretending Dexter is gaining brand-new context powers.

Before this phase, Dexter already observed focused apps, focused elements,
clipboard changes, shell command context, screen-lock state, action receipts,
and daemon health. The gap was visibility: `dexter-cli --status` and the HUD
status sheet showed health and actions, but did not tell the operator which
current-context abilities were active.

## Outcome

Complete.

The daemon now owns a context capability summary derived from the existing
`ContextObserver` snapshot. It is exposed through `HealthResponse` as
`operator_context_markdown`, then rendered by both operator surfaces:

- `dexter-cli --status` prints a `Current Context` section.
- The HUD status sheet prints the same `Current Context` section.

The summary is intentionally descriptive, not permissive. It does not bypass
policy, approval, Contacts resolution, action receipts, or existing context
guards. It only tells the operator what Dexter can already use from the current
context:

- terminal focus: explain shell output, suggest commands, run local commands
  when asked, inspect workflow files
- Messages focus: draft/revise messages, resolve recipients through Contacts,
  send after approval, explain message receipts
- Contacts focus: use exact Contacts names and explain Contacts-resolution
  failures
- browser focus: summarize pages, extract links/text, click/type/navigate when
  asked, request approval for consequential browser actions
- Finder/editor/general focus: inspect files, summarize visible code/text, use
  clipboard text, and route explicit actions through the normal approval flow

If no focused app snapshot has been observed, the surfaces show a plain fallback
instead of inventing context.

## Evidence

Proto regeneration:

```text
make proto: PASS
```

Targeted Rust tests:

```text
cargo test --bin dexter-core operator_context: PASS, 5 passed
cargo test --bin dexter-cli format_operator_status_report: PASS, 3 passed
```

Full Rust tests:

```text
cargo test --bin dexter-core: PASS, 630 passed, 7 ignored
cargo test --bin dexter-cli: PASS, 49 passed
```

Builds:

```text
cargo build --release --bin dexter-core --bin dexter-cli: PASS
cd src/swift && swift build: PASS
```

Focused live smokes:

```text
live-smoke-operator-status: PASS
live-smoke-hud-health: PASS
```

Clean combined smoke summary:

```text
docs/live-smoke-results/live-smoke-20260606_212307.md
```

An earlier HUD-health smoke exposed one useful smoke fragility: the new context
section pushed `Latest Action Summary` outside the HUD smoke preview. The smoke
preview limit was expanded, then the clean combined summary above passed.

## Remaining Work

None for Phase 46.

Future context work should improve the underlying observers or action
capabilities directly. The status/HUD context summary should remain a thin
daemon-owned rendering of what Dexter already observes and can already route.
