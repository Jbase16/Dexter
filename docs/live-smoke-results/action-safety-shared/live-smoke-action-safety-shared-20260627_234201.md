# Dexter Shared-Core Action Safety Smoke

- Started: `2026-06-27T23:42:01-0700`
- Finished: `2026-06-27T23:44:50-0700`
- Duration: `2m 49s`
- Root: `/Users/jason/Developer/Dex`
- Result: `FAIL`
- Mode: `shared-core`
- Passed: `0`
- Failed: `1`
- Logs: `/Users/jason/Developer/Dex/docs/live-smoke-results/action-safety-shared/logs/20260627_234201`
- Core Log: `/Users/jason/Developer/Dex/docs/live-smoke-results/action-safety-shared/logs/20260627_234201/shared-core.log`

## Targets

| Target | Result | Duration | Log |
|---|---:|---:|---|
| `live-action-diagnostic-smoke` | FAIL | `0s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/action-safety-shared/logs/20260627_234201/live-action-diagnostic-smoke.log` |

## Failure Tails

### `live-action-diagnostic-smoke`

```text
scripts/live-action-safety-shared-smoke.sh: line 159: scripts/live-action-diagnostic-smoke.sh: Permission denied
```

## Re-run

```bash
make live-smoke-action-safety-shared
```
