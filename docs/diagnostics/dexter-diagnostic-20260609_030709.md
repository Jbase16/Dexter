# Dexter Diagnostic Bundle

- Created: `2026-06-09T03:07:09-0700`
- Root: `/Users/jason/Developer/Dex`
- Include full operator status: `0`

This report intentionally avoids full transcripts by default. Set
`DEXTER_DIAGNOSTIC_INCLUDE_STATUS=1` to include `dexter-cli --status`.

## System

```text
Tue Jun  9 03:07:09 PDT 2026
ProductName:		macOS
ProductVersion:		26.5
BuildVersion:		25F5058e
Darwin Mac.attlocal.net 25.5.0 Darwin Kernel Version 25.5.0: Tue Apr 14 21:52:16 PDT 2026; root:xnu-12377.120.99.0.7~25/RELEASE_ARM64_T6041 arm64
```

Exit status: `0`

## Dexter Processes

```text
```

Exit status: `0`

## Dexter Sockets

```text
```

Exit status: `0`

## Ollama Environment

```text
process OLLAMA_MODELS=/Users/jason/ollama-models
launchctl OLLAMA_MODELS=/Users/jason/ollama-models
```

Exit status: `0`

## Model Stores

```text
drwxr-xr-x    5 jason  staff      160 May 24 13:23 /Users/jason/ollama-models
drwx------    1 jason  staff  1048576 Nov 30  2025 /Volumes/BitHappens/ollama-models
drwxr-xr-x@ 118 jason  staff     3776 Jun  8 08:56 /Volumes/ByteMe
```

Exit status: `0`

## Ollama Models

```text
NAME                           ID              SIZE      MODIFIED    
sentinel-9b-god-tier:latest    6c5cee01c68a    18 GB     3 weeks ago    
mxbai-embed-large:latest       468836162de7    669 MB    5 weeks ago    
qwen3:8b                       500a1f067a9f    5.2 GB    5 weeks ago    
deepseek-coder-v2:16b          63fb193b3a9b    8.9 GB    5 weeks ago    
deepseek-r1:32b                edba8017331d    19 GB     5 weeks ago    
gemma4:26b                     5571076f3d70    17 GB     5 weeks ago    
```

Exit status: `0`

## Ollama Runners

```text
NAME          ID              SIZE     PROCESSOR    CONTEXT    UNTIL               
gemma4:26b    5571076f3d70    17 GB    100% GPU     262144     15 minutes from now    
```

Exit status: `0`

## Disk

```text
Filesystem      Size    Used   Avail Capacity iused ifree %iused  Mounted on
/dev/disk3s5   460Gi   313Gi    82Gi    80%    3.4M  865M    0%   /System/Volumes/Data
/dev/disk3s5   460Gi   313Gi    82Gi    80%    3.4M  865M    0%   /System/Volumes/Data
/dev/disk3s5   460Gi   313Gi    82Gi    80%    3.4M  865M    0%   /System/Volumes/Data
```

Exit status: `0`

## Dock Launcher

```text
drwxr-xr-x  3 jason  staff   96 Jun  7 03:16 /Users/jason/Applications/Dexter.app
-rwxr-xr-x  1 jason  staff  742 Jun  9 03:03 /Users/jason/Applications/Dexter.app/Contents/MacOS/DexterLauncher
com.jason.dexter.launcher
```

Exit status: `0`

## Doctor

```text
Dexter Doctor

OK   config             /Users/jason/.dexter/config.toml loaded; Ollama http://localhost:11434
OK   cli binary         /Users/jason/Developer/Dex/src/rust-core/target/release/dexter-cli
OK   core binary        /Users/jason/Developer/Dex/src/rust-core/target/release/dexter-core
FAIL core socket file   /tmp/dexter.sock missing; start the daemon first
WARN shell socket file  /tmp/dexter-shell.sock missing; shell context may be unavailable
FAIL daemon ping        connect failed: tonic Channel connect failed: transport error: No such file or directory (os error 2): No such file or directory (os error 2)
OK   disk state         /Users/jason/.dexter/state: 82.4 GiB available / 460.4 GiB total (ready) - 82.4 GiB available
OK   disk workspace     /Users/jason/Developer/Dex: 82.4 GiB available / 460.4 GiB total (ready) - 82.4 GiB available
OK   disk temp          /var/folders/gg/2rv51rgx3259vbndk_zcffyh0000gn/T/: 82.4 GiB available / 460.4 GiB total (ready) - 82.4 GiB available
FAIL daemon health      connect failed: tonic Channel connect failed: transport error: No such file or directory (os error 2): No such file or directory (os error 2)
OK   ollama             http://localhost:11434 reachable; 6 models
OK   ollama launch      Ollama.app active
OK   ollama models dir  launchctl OLLAMA_MODELS=/Users/jason/ollama-models
OK   ollama runners     no large unexpected resident runners

Suggested fixes:
  cd /Users/jason/Developer/Dex && make open-app
  cd /Users/jason/Developer/Dex && make run

Result: FAIL - fix failed checks before relying on Dexter.
```

Exit status: `1`

## Operator Status

Skipped by default. Re-run with:

```bash
DEXTER_DIAGNOSTIC_INCLUDE_STATUS=1 make diagnostic-bundle
```

## Latest Live Smoke Summary

```text
# Dexter Live Smoke Summary

- Started: `2026-06-08T07:27:08-0700`
- Finished: `2026-06-08T07:45:02-0700`
- Duration: `17m 54s`
- Root: `/Users/jason/Developer/Dex`
- Result: `PASS`
- Passed: `23`
- Failed: `0`
- Logs: `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708`

## Targets

| Target | Result | Duration | Log |
|---|---:|---:|---|
| `live-smoke-startup-readiness` | PASS | `20s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-startup-readiness.log` |
| `live-smoke-process-control` | PASS | `24s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-process-control.log` |
| `live-smoke-stale-swift-stop` | PASS | `17s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-stale-swift-stop.log` |
| `live-smoke-dock-launcher` | PASS | `0s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-dock-launcher.log` |
| `live-smoke-recovery` | PASS | `29s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-recovery.log` |
| `live-smoke-degraded-mode` | PASS | `18s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-degraded-mode.log` |
| `live-smoke-external-failures` | PASS | `31s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-external-failures.log` |
| `live-smoke-operator-status` | PASS | `22s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-operator-status.log` |
| `live-smoke-action-diagnostic` | PASS | `23s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-action-diagnostic.log` |
| `live-smoke-cli` | PASS | `5m 08s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-cli.log` |
| `live-smoke-action-matrix` | PASS | `33s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-action-matrix.log` |
| `live-smoke-action-receipts` | PASS | `27s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-action-receipts.log` |
| `live-smoke-approval-lifecycle` | PASS | `31s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-approval-lifecycle.log` |
| `live-smoke-hud` | PASS | `52s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-hud.log` |
| `live-smoke-hud-new-session` | PASS | `39s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-hud-new-session.log` |
| `live-smoke-hud-lifecycle` | PASS | `1m 16s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-hud-lifecycle.log` |
| `live-smoke-hud-placement` | PASS | `40s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-hud-placement.log` |
| `live-smoke-hud-health` | PASS | `1m 07s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-hud-health.log` |
| `live-smoke-hud-action-history` | PASS | `39s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-hud-action-history.log` |
| `live-smoke-hud-action-diagnostic` | PASS | `40s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-hud-action-diagnostic.log` |
| `live-smoke-hud-approval` | PASS | `53s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-hud-approval.log` |
| `live-smoke-action-cancel` | PASS | `36s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-action-cancel.log` |
| `live-smoke-barge-in` | PASS | `49s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260608_072708/live-smoke-barge-in.log` |

## Re-run

```bash
bash scripts/live-smoke-summary.sh live-smoke-startup-readiness live-smoke-process-control live-smoke-stale-swift-stop live-smoke-dock-launcher live-smoke-recovery live-smoke-degraded-mode live-smoke-external-failures live-smoke-operator-status live-smoke-action-diagnostic live-smoke-cli live-smoke-action-matrix live-smoke-action-receipts live-smoke-approval-lifecycle live-smoke-hud live-smoke-hud-new-session live-smoke-hud-lifecycle live-smoke-hud-placement live-smoke-hud-health live-smoke-hud-action-history live-smoke-hud-action-diagnostic live-smoke-hud-approval live-smoke-action-cancel live-smoke-barge-in
```
```

Exit status: `0`

