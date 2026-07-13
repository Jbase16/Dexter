# Phase 67 — C3 Action Outcome Records

## Goal

Persist action outcome evidence into the Context Turn Record stream so C3 has
durable, privacy-safe facts to learn from later.

This phase is passive only. It does not add replay, learned scoring, model
judges, or autonomous recovery.

## Behavior

Synthetic `dexter-cli --action-json` actions now start a minimal context turn
record before execution. The record is tagged as:

- `route_category = SyntheticAction`
- `model = synthetic_action`
- `context_diagnostics.scope = ambient_only`
- `privacy_mode = redacted_preview_v1`

When the action completes or fails, the existing action-result attachment path
updates that record with the same typed diagnostic evidence used by action
receipts and `dexter-cli --why`.

## Acceptance

Run:

```bash
make live-smoke-context-turn-records
```

The smoke starts a fresh release core, drives deterministic UI and browser
failure actions, then inspects `{state_dir}/context_turns/**/*.json` for:

- a `ui_click` record with `source = ui_window`,
  `failure_kind = control_not_found`, and
  `recovery_directive = snapshot_then_replan`;
- a `browser` record with `source = browser`,
  `failure_kind = selector_not_found`, and
  `recovery_directive = extract_page_then_replan`;
- bounded target/evidence previews;
- no raw typed text payloads.

The target is part of `live-smoke-action-safety`,
`live-smoke-action-safety-full`, `live-smoke-acceptance`, and
`live-smoke-all`.
