# Dexter Diagnostic Bundle

- Created: `2026-06-09T04:54:19-0700`
- Root: `/Users/jason/Developer/Dex`
- Include full operator status: `0`

This report intentionally avoids full transcripts by default. Set
`DEXTER_DIAGNOSTIC_INCLUDE_STATUS=1` to include `dexter-cli --status`.

## System

```text
Tue Jun  9 04:54:19 PDT 2026
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
qwen3:8b                       500a1f067a9f    5.2 GB    5 weeks ago    
deepseek-coder-v2:16b          63fb193b3a9b    8.9 GB    5 weeks ago    
deepseek-r1:32b                edba8017331d    19 GB     5 weeks ago    
gemma4:26b                     5571076f3d70    17 GB     5 weeks ago    
mxbai-embed-large:latest       468836162de7    669 MB    5 weeks ago    
```

Exit status: `0`

## Ollama Runners

```text
NAME                        ID              SIZE      PROCESSOR    CONTEXT    UNTIL              
qwen3:8b                    500a1f067a9f    11 GB     100% GPU     40960      17 hours from now     
mxbai-embed-large:latest    468836162de7    685 MB    100% GPU     512        9 minutes from now    
```

Exit status: `0`

## Disk

```text
Filesystem      Size    Used   Avail Capacity iused ifree %iused  Mounted on
/dev/disk3s5   460Gi   314Gi    80Gi    80%    3.4M  835M    0%   /System/Volumes/Data
/dev/disk3s5   460Gi   314Gi    80Gi    80%    3.4M  835M    0%   /System/Volumes/Data
/dev/disk3s5   460Gi   314Gi    80Gi    80%    3.4M  835M    0%   /System/Volumes/Data
```

Exit status: `0`

## Dock Launcher

```text
drwxr-xr-x  3 jason  staff   96 Jun  7 03:16 /Users/jason/Applications/Dexter.app
-rwxr-xr-x  1 jason  staff  742 Jun  9 04:33 /Users/jason/Applications/Dexter.app/Contents/MacOS/DexterLauncher
com.jason.dexter.launcher
```

Exit status: `0`

## Dock Launcher Command

```text
#!/usr/bin/env zsh
set -euo pipefail

osascript <<OSA
set repoPath to "/Users/jason/Developer/Dex"
set appPath to "/Users/jason/Applications/Dexter.app"
tell application "Terminal"
    activate
    set dexterTab to do script "cd " & quoted form of repoPath & "; export OLLAMA_MODELS=/Users/jason/ollama-models; clear; echo 'Dexter live logs'; echo 'Started from: " & appPath & "'; echo 'OLLAMA_MODELS='\/Users/jason/ollama-models; echo; echo 'Use Dexter > New Session for a fresh conversation.'; echo 'Use Dexter > Restart Dexter to restart the app/core.'; echo 'Use Dexter > Quit Dexter to stop the app/core.'; echo; make configure-ollama-models && make stop && make run"
    set custom title of dexterTab to "Dexter Live Logs"
end tell
OSA
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
OK   disk state         /Users/jason/.dexter/state: 79.6 GiB available / 460.4 GiB total (ready) - 79.6 GiB available
OK   disk workspace     /Users/jason/Developer/Dex: 79.6 GiB available / 460.4 GiB total (ready) - 79.6 GiB available
OK   disk temp          /var/folders/gg/2rv51rgx3259vbndk_zcffyh0000gn/T/: 79.6 GiB available / 460.4 GiB total (ready) - 79.6 GiB available
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

- Started: `2026-06-09T04:48:22-0700`
- Finished: `2026-06-09T04:54:11-0700`
- Duration: `5m 49s`
- Root: `/Users/jason/Developer/Dex`
- Result: `PASS`
- Passed: `9`
- Failed: `0`
- Logs: `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822`

## Targets

| Target | Result | Duration | Log |
|---|---:|---:|---|
| `live-smoke-external-failures` | PASS | `30s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-external-failures.log` |
| `live-smoke-action-diagnostic` | PASS | `25s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-action-diagnostic.log` |
| `live-smoke-action-matrix` | PASS | `36s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-action-matrix.log` |
| `live-smoke-action-receipts` | PASS | `27s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-action-receipts.log` |
| `live-smoke-approval-lifecycle` | PASS | `33s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-approval-lifecycle.log` |
| `live-smoke-hud-action-history` | PASS | `47s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-hud-action-history.log` |
| `live-smoke-hud-action-diagnostic` | PASS | `46s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-hud-action-diagnostic.log` |
| `live-smoke-hud-approval` | PASS | `1m 06s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-hud-approval.log` |
| `live-smoke-action-cancel` | PASS | `39s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_044822/live-smoke-action-cancel.log` |

## Re-run

```bash
bash scripts/live-smoke-summary.sh live-smoke-external-failures live-smoke-action-diagnostic live-smoke-action-matrix live-smoke-action-receipts live-smoke-approval-lifecycle live-smoke-hud-action-history live-smoke-hud-action-diagnostic live-smoke-hud-approval live-smoke-action-cancel
```
```

Exit status: `0`

## Recent Live Smoke Index

```text
# Dexter Live Smoke Index

- Generated: `2026-06-09T04:54:11-0700`
- Root: `/Users/jason/Developer/Dex`
- Entries: `20`

| Summary | Started | Result | Passed | Failed | Duration | Targets |
|---|---:|---:|---:|---:|---:|---|
| `docs/live-smoke-results/live-smoke-20260609_044822.md` | `2026-06-09T04:48:22-0700` | PASS | `9` | `0` | `5m 49s` | `live-smoke-external-failures`, `live-smoke-action-diagnostic`, `live-smoke-action-matrix`, `live-smoke-action-receipts`, `live-smoke-approval-lifecycle`, `live-smoke-hud-action-history`, `live-smoke-hud-action-diagnostic`, `live-smoke-hud-approval`, `live-smoke-action-cancel` |
| `docs/live-smoke-results/live-smoke-20260609_044524.md` | `2026-06-09T04:45:24-0700` | PASS | `1` | `0` | `0s` | `live-smoke-dock-launcher` |
| `docs/live-smoke-results/live-smoke-20260609_044052.md` | `2026-06-09T04:40:52-0700` | PASS | `4` | `0` | `2m 40s` | `live-smoke-startup-readiness`, `live-smoke-operator-status`, `live-smoke-hud-health`, `live-smoke-hud-unavailable-health` |
| `docs/live-smoke-results/live-smoke-20260609_043640.md` | `2026-06-09T04:36:40-0700` | PASS | `4` | `0` | `2m 47s` | `live-smoke-startup-readiness`, `live-smoke-operator-status`, `live-smoke-hud-health`, `live-smoke-hud-unavailable-health` |
| `docs/live-smoke-results/live-smoke-20260609_042609.md` | `2026-06-09T04:26:09-0700` | PASS | `8` | `0` | `5m 17s` | `live-smoke-dock-launcher`, `live-smoke-process-control`, `live-smoke-stop-report`, `live-smoke-run-loop-lifecycle`, `live-smoke-stale-swift-stop`, `live-smoke-hud-lifecycle`, `live-smoke-hud-placement`, `live-smoke-placement-command` |
| `docs/live-smoke-results/live-smoke-20260609_042326.md` | `2026-06-09T04:23:26-0700` | PASS | `2` | `0` | `1m 28s` | `live-smoke-hud-placement`, `live-smoke-placement-command` |
| `docs/live-smoke-results/live-smoke-20260609_041955.md` | `2026-06-09T04:19:55-0700` | PASS | `1` | `0` | `43s` | `live-smoke-hud-placement` |
| `docs/live-smoke-results/live-smoke-20260609_041154.md` | `2026-06-09T04:11:54-0700` | PASS | `5` | `0` | `3m 35s` | `live-smoke-process-control`, `live-smoke-stop-report`, `live-smoke-run-loop-lifecycle`, `live-smoke-stale-swift-stop`, `live-smoke-hud-lifecycle` |
| `docs/live-smoke-results/live-smoke-20260609_040635.md` | `2026-06-09T04:06:35-0700` | PASS | `4` | `0` | `2m 07s` | `live-smoke-process-control`, `live-smoke-stop-report`, `live-smoke-stale-swift-stop`, `live-smoke-hud-lifecycle` |
| `docs/live-smoke-results/live-smoke-20260609_040035.md` | `2026-06-09T04:00:35-0700` | PASS | `3` | `0` | `50s` | `live-smoke-startup-readiness`, `live-smoke-diagnostic-bundle`, `live-smoke-operator-status` |
| `docs/live-smoke-results/live-smoke-20260608_072708.md` | `2026-06-08T07:27:08-0700` | PASS | `23` | `0` | `17m 54s` | `live-smoke-startup-readiness`, `live-smoke-process-control`, `live-smoke-stale-swift-stop`, `live-smoke-dock-launcher`, `live-smoke-recovery`, `live-smoke-degraded-mode`, `live-smoke-external-failures`, `live-smoke-operator-status`, `live-smoke-action-diagnostic`, `live-smoke-cli`, `live-smoke-action-matrix`, `live-smoke-action-receipts`, `live-smoke-approval-lifecycle`, `live-smoke-hud`, `live-smoke-hud-new-session`, `live-smoke-hud-lifecycle`, `live-smoke-hud-placement`, `live-smoke-hud-health`, `live-smoke-hud-action-history`, `live-smoke-hud-action-diagnostic`, `live-smoke-hud-approval`, `live-smoke-action-cancel`, `live-smoke-barge-in` |
| `docs/live-smoke-results/live-smoke-20260608_072434.md` | `2026-06-08T07:24:34-0700` | PASS | `5` | `0` | `1m 57s` | `live-smoke-process-control`, `live-smoke-stale-swift-stop`, `live-smoke-dock-launcher`, `live-smoke-hud`, `live-smoke-hud-action-diagnostic` |
| `docs/live-smoke-results/live-smoke-20260608_072355.md` | `2026-06-08T07:23:55-0700` | PASS | `2` | `0` | `30s` | `live-smoke-process-control`, `live-smoke-stale-swift-stop` |
| `docs/live-smoke-results/live-smoke-20260608_072226.md` | `2026-06-08T07:22:26-0700` | FAIL | `1` | `1` | `47s` | `live-smoke-process-control`, `live-smoke-stale-swift-stop` |
| `docs/live-smoke-results/live-smoke-20260608_071939.md` | `2026-06-08T07:19:39-0700` | FAIL | `4` | `1` | `2m 07s` | `live-smoke-process-control`, `live-smoke-stale-swift-stop`, `live-smoke-dock-launcher`, `live-smoke-hud`, `live-smoke-hud-action-diagnostic` |
| `docs/live-smoke-results/live-smoke-20260608_071641.md` | `2026-06-08T07:16:41-0700` | FAIL | `4` | `1` | `2m 08s` | `live-smoke-process-control`, `live-smoke-stale-swift-stop`, `live-smoke-dock-launcher`, `live-smoke-hud`, `live-smoke-hud-action-diagnostic` |
| `docs/live-smoke-results/live-smoke-20260608_071319.md` | `2026-06-08T07:13:19-0700` | FAIL | `4` | `1` | `2m 09s` | `live-smoke-process-control`, `live-smoke-stale-swift-stop`, `live-smoke-dock-launcher`, `live-smoke-hud`, `live-smoke-hud-action-diagnostic` |
| `docs/live-smoke-results/live-smoke-20260608_070924.md` | `2026-06-08T07:09:24-0700` | PASS | `3` | `0` | `2m 12s` | `live-smoke-hud-action-history`, `live-smoke-action-cancel`, `live-smoke-barge-in` |
| `docs/live-smoke-results/live-smoke-20260608_064951.md` | `2026-06-08T06:49:51-0700` | PASS | `22` | `0` | `18m 00s` | `live-smoke-startup-readiness`, `live-smoke-process-control`, `live-smoke-dock-launcher`, `live-smoke-recovery`, `live-smoke-degraded-mode`, `live-smoke-external-failures`, `live-smoke-operator-status`, `live-smoke-action-diagnostic`, `live-smoke-cli`, `live-smoke-action-matrix`, `live-smoke-action-receipts`, `live-smoke-approval-lifecycle`, `live-smoke-hud`, `live-smoke-hud-new-session`, `live-smoke-hud-lifecycle`, `live-smoke-hud-placement`, `live-smoke-hud-health`, `live-smoke-hud-action-history`, `live-smoke-hud-action-diagnostic`, `live-smoke-hud-approval`, `live-smoke-action-cancel`, `live-smoke-barge-in` |
| `docs/live-smoke-results/live-smoke-20260608_062116.md` | `2026-06-08T06:21:16-0700` | PASS | `5` | `0` | `2m 59s` | `live-smoke-dock-launcher`, `live-smoke-process-control`, `live-smoke-hud-new-session`, `live-smoke-hud-lifecycle`, `live-smoke-hud-placement` |

## Latest

```bash
sed -n '1,120p' docs/live-smoke-results/latest.md
```
```

Exit status: `0`

