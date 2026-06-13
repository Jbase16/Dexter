# Phase 51 — Stop / Restart Observability

## Goal

Make Dexter's stop path more useful when the operator is stuck and needs to
recover without hunting through Activity Monitor or stale terminal windows.

## Changes

- `scripts/stop-dexter.sh` now prints one labeled line per target process in
  non-quiet mode:
  - PID
  - command line
  - current working directory when macOS exposes it
- Quiet mode remains quiet for app-initiated shutdown and cleanup hooks.
- `make live-smoke-stop-report` starts the release core, runs the stop script,
  and verifies:
  - the stop output contains a process summary;
  - the target is identified as `dexter-core`;
  - the target working directory is visible;
  - daemon sockets are removed and no longer accept connections.
- `make live-smoke-run-loop-lifecycle` starts the exact `make run` tree used by
  the Dock launcher and verifies UI restart and quit both make the parent run
  loop exit and clean the daemon sockets.

## Why

The stop path already avoided broad process killing by checking repo-owned
process shapes. The missing piece was operator-facing evidence. When Dexter is
in a bad state, `make stop` should show what it found and stopped, not just a
list of numbers.

## Verification

```bash
bash -n scripts/stop-dexter.sh
bash -n scripts/live-stop-report-smoke.sh
bash -n scripts/live-run-loop-lifecycle-smoke.sh
make live-smoke-stop-report
make live-smoke-run-loop-lifecycle
```
