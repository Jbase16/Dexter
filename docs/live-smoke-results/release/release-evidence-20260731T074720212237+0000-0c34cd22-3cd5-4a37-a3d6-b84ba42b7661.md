# Dexter Daily-driver v1 Release Evidence

- Run ID: `0c34cd22-3cd5-4a37-a3d6-b84ba42b7661`
- Started: `2026-07-31T07:23:58.404708+00:00`
- Finished: `2026-07-31T07:47:20.212237+00:00`
- Automated result: **FAIL**
- Release state: `AUTOMATED_FAIL`
- Manual checklist: **not recorded**
- Source SHA-256: `580afbea24d2ae012151e57676ca10e5bec05397af9984f79af530d8b7f952d5`

## Build and Unit Checks

| Check | Result | Duration |
|---|---:|---:|
| `rust_unit` | PASS | `3508 ms` |
| `rust_release` | PASS | `119 ms` |
| `rust_cli_release` | PASS | `112 ms` |
| `python_workers` | PASS | `3318 ms` |
| `proto_consistency` | PASS | `302 ms` |
| `swift_release` | PASS | `70499 ms` |

## Acceptance Targets

| Battery | Target | Result | Duration |
|---|---|---:|---:|
| `acceptance` | `live-smoke-dock-launcher` | PASS | `2000 ms` |
| `acceptance` | `live-smoke-process-control` | PASS | `5000 ms` |
| `acceptance` | `live-smoke-stop-report` | PASS | `4000 ms` |
| `acceptance` | `live-smoke-run-loop-lifecycle` | PASS | `39000 ms` |
| `acceptance` | `live-smoke-stale-swift-stop` | PASS | `11000 ms` |
| `acceptance` | `live-smoke-hud-lifecycle` | PASS | `57000 ms` |
| `acceptance` | `live-smoke-hud-placement` | PASS | `28000 ms` |
| `acceptance` | `live-smoke-placement-command` | PASS | `31000 ms` |
| `acceptance` | `live-smoke-residency-proof` | PASS | `1000 ms` |
| `acceptance` | `live-smoke-startup-readiness` | PASS | `13000 ms` |
| `acceptance` | `live-smoke-local-answers` | PASS | `23000 ms` |
| `acceptance` | `live-smoke-operator-status` | FAIL | `19000 ms` |
| `acceptance` | `live-smoke-hud-health` | PASS | `51000 ms` |
| `acceptance` | `live-smoke-hud-unavailable-health` | PASS | `25000 ms` |
| `acceptance` | `live-smoke-external-failures` | FAIL | `23000 ms` |
| `acceptance` | `live-smoke-action-diagnostic` | FAIL | `16000 ms` |
| `acceptance` | `live-smoke-context-turn-records` | PASS | `28000 ms` |
| `acceptance` | `live-smoke-shortcut-action` | PASS | `26000 ms` |
| `acceptance` | `live-smoke-window-focus` | PASS | `20000 ms` |
| `acceptance` | `live-smoke-window-inspect` | PASS | `17000 ms` |
| `acceptance` | `live-smoke-ui-snapshot` | PASS | `17000 ms` |
| `acceptance` | `live-smoke-ui-click` | PASS | `28000 ms` |
| `acceptance` | `live-smoke-ui-type` | PASS | `21000 ms` |
| `acceptance` | `live-smoke-ui-select` | PASS | `22000 ms` |
| `acceptance` | `live-smoke-ui-toggle` | PASS | `20000 ms` |
| `acceptance` | `live-smoke-ui-pick` | PASS | `22000 ms` |
| `acceptance` | `live-smoke-ui-failure-diagnostic` | PASS | `22000 ms` |
| `acceptance` | `live-smoke-action-matrix` | PASS | `31000 ms` |
| `acceptance` | `live-smoke-browser-recovery` | PASS | `19000 ms` |
| `acceptance` | `live-smoke-action-receipts` | FAIL | `20000 ms` |
| `acceptance` | `live-smoke-approval-lifecycle` | FAIL | `25000 ms` |
| `acceptance` | `live-smoke-hud-action-surfaces` | FAIL | `46000 ms` |
| `acceptance` | `live-smoke-hud-ui-failure` | PASS | `49000 ms` |
| `acceptance` | `live-smoke-hud-approval` | FAIL | `120000 ms` |
| `acceptance` | `live-smoke-action-cancel` | PASS | `30000 ms` |
| `action_safety_full` | `live-smoke-external-failures` | FAIL | `17000 ms` |
| `action_safety_full` | `live-smoke-action-diagnostic` | FAIL | `10000 ms` |
| `action_safety_full` | `live-smoke-context-turn-records` | PASS | `19000 ms` |
| `action_safety_full` | `live-smoke-shortcut-action` | PASS | `19000 ms` |
| `action_safety_full` | `live-smoke-window-focus` | PASS | `26000 ms` |
| `action_safety_full` | `live-smoke-window-inspect` | PASS | `19000 ms` |
| `action_safety_full` | `live-smoke-ui-snapshot` | PASS | `18000 ms` |
| `action_safety_full` | `live-smoke-ui-click` | PASS | `21000 ms` |
| `action_safety_full` | `live-smoke-ui-type` | PASS | `20000 ms` |
| `action_safety_full` | `live-smoke-ui-select` | PASS | `28000 ms` |
| `action_safety_full` | `live-smoke-ui-toggle` | PASS | `21000 ms` |
| `action_safety_full` | `live-smoke-ui-pick` | PASS | `22000 ms` |
| `action_safety_full` | `live-smoke-ui-failure-diagnostic` | PASS | `22000 ms` |
| `action_safety_full` | `live-smoke-action-matrix` | PASS | `33000 ms` |
| `action_safety_full` | `live-smoke-browser-recovery` | PASS | `17000 ms` |
| `action_safety_full` | `live-smoke-browser-recovery-model` | FAIL | `13000 ms` |
| `action_safety_full` | `live-smoke-action-receipts` | FAIL | `15000 ms` |
| `action_safety_full` | `live-smoke-approval-lifecycle` | FAIL | `25000 ms` |
| `action_safety_full` | `live-smoke-hud-action-surfaces` | FAIL | `1000 ms` |
| `action_safety_full` | `live-smoke-hud-ui-failure` | FAIL | `1000 ms` |
| `action_safety_full` | `live-smoke-hud-approval` | FAIL | `0 ms` |
| `action_safety_full` | `live-smoke-action-cancel` | PASS | `25000 ms` |

## Gate Errors

- acceptance smoke command exited 2
- action_safety_full smoke command exited 2
