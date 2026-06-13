# Dexter Diagnostic Bundle

- Created: `2026-06-09T04:40:05-0700`
- Root: `/Users/jason/Developer/Dex`
- Include full operator status: `0`

This report intentionally avoids full transcripts by default. Set
`DEXTER_DIAGNOSTIC_INCLUDE_STATUS=1` to include `dexter-cli --status`.

## System

```text
Tue Jun  9 04:40:05 PDT 2026
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
deepseek-coder-v2:16b          63fb193b3a9b    8.9 GB    5 weeks ago    
deepseek-r1:32b                edba8017331d    19 GB     5 weeks ago    
gemma4:26b                     5571076f3d70    17 GB     5 weeks ago    
mxbai-embed-large:latest       468836162de7    669 MB    5 weeks ago    
qwen3:8b                       500a1f067a9f    5.2 GB    5 weeks ago    
```

Exit status: `0`

## Ollama Runners

```text
NAME        ID              SIZE     PROCESSOR    CONTEXT    UNTIL             
qwen3:8b    500a1f067a9f    11 GB    100% GPU     40960      17 hours from now    
```

Exit status: `0`

## Disk

```text
Filesystem      Size    Used   Avail Capacity iused ifree %iused  Mounted on
/dev/disk3s5   460Gi   314Gi    82Gi    80%    3.4M  857M    0%   /System/Volumes/Data
/dev/disk3s5   460Gi   314Gi    82Gi    80%    3.4M  857M    0%   /System/Volumes/Data
/dev/disk3s5   460Gi   314Gi    82Gi    80%    3.4M  857M    0%   /System/Volumes/Data
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
OK   disk state         /Users/jason/.dexter/state: 81.7 GiB available / 460.4 GiB total (ready) - 81.7 GiB available
OK   disk workspace     /Users/jason/Developer/Dex: 81.7 GiB available / 460.4 GiB total (ready) - 81.7 GiB available
OK   disk temp          /var/folders/gg/2rv51rgx3259vbndk_zcffyh0000gn/T/: 81.7 GiB available / 460.4 GiB total (ready) - 81.7 GiB available
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

- Started: `2026-06-09T04:36:40-0700`
- Finished: `2026-06-09T04:39:27-0700`
- Duration: `2m 47s`
- Root: `/Users/jason/Developer/Dex`
- Result: `PASS`
- Passed: `4`
- Failed: `0`
- Logs: `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_043640`

## Targets

| Target | Result | Duration | Log |
|---|---:|---:|---|
| `live-smoke-startup-readiness` | PASS | `25s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_043640/live-smoke-startup-readiness.log` |
| `live-smoke-operator-status` | PASS | `25s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_043640/live-smoke-operator-status.log` |
| `live-smoke-hud-health` | PASS | `1m 16s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_043640/live-smoke-hud-health.log` |
| `live-smoke-hud-unavailable-health` | PASS | `41s` | `/Users/jason/Developer/Dex/docs/live-smoke-results/logs/20260609_043640/live-smoke-hud-unavailable-health.log` |

## Re-run

```bash
bash scripts/live-smoke-summary.sh live-smoke-startup-readiness live-smoke-operator-status live-smoke-hud-health live-smoke-hud-unavailable-health
```
```

Exit status: `0`

