# Dexter Operator Controls

This is the practical control surface for starting, stopping, moving, and
diagnosing Dexter during normal use.

## One-command readiness

Prepare this Mac for a clean operator launch:

```bash
cd /Users/jason/Developer/Dex
make operator-ready
```

This stops stale Dexter processes and sockets, reasserts
`OLLAMA_MODELS=/Users/jason/ollama-models` through `launchctl`, verifies Ollama
can see the configured Dexter model set, builds the release Rust core and CLI,
builds the Swift app, and installs the Dock launcher.

Use this after machine-level changes, model-storage changes, or a confusing
failed launch. It prepares Dexter; it does not open a live session.

## Dock App

Install the wrapper:

```bash
cd /Users/jason/Developer/Dex
make install-app
```

Open it:

```bash
make open-app
```

The wrapper installs `~/Applications/Dexter.app`. Opening it launches Terminal,
sets the Terminal title to `Dexter Live Logs`, prints the operator controls, and
runs:

```bash
export OLLAMA_MODELS=/Users/jason/ollama-models
make configure-ollama-models && make stop && make run
```

That gives Dexter a normal Dock/app entry while keeping the live Rust and Swift
logs visible in the associated Terminal window. The launcher reasserts the local
hot model store before startup so ordinary Dock launches and HUD restarts use
the same model-storage assumptions as `make operator-ready`.

## App Menu

The Dexter menu exposes the controls that should not require hunting for the
original `make run` terminal:

- `Dexter > New Session`
- `Dexter > Move Dexter to Mouse`
- `Dexter > Start Dexter Placement Drag`
- `Dexter > Stop Dexter Placement Drag`
- `Dexter > Restart Dexter`
- `Dexter > Quit Dexter`

`Restart Dexter` and `Quit Dexter` first show a short HUD confirmation, then
restart or terminate the Swift app and Rust core. `New Session` keeps the app
running and opens a fresh gRPC session.

Regression coverage:

```bash
make live-smoke-operator-controls
make live-smoke-hud-new-session
make live-smoke-hud-lifecycle
make live-smoke-process-control
make live-smoke-run-loop-lifecycle
make live-smoke-stale-swift-stop
make live-smoke-operator-ready
make live-smoke-diagnostic-bundle
make live-smoke-dock-launcher
```

`make live-smoke-operator-controls` is the focused acceptance slice for the
operator-facing controls: Dock launcher metadata, external stop, labeled stop
output, UI restart/quit through the normal run loop, stale Swift cleanup, and
placement click-through plus external placement command delivery.

The new-session smoke launches the real Swift app, triggers the HUD New Session
path, and verifies the daemon opens a fresh session without restarting the core.
The lifecycle smoke triggers the actual restart and quit handlers, verifies the
restart path reaches process control without opening a real Terminal loop, and
confirms the old daemon sockets are cleaned up. The process-control smoke starts
the normal `make run` tree and proves an external `make stop` terminates both
the run loop and daemon socket. The stop-report smoke starts the release core,
stops it, and verifies the stop command prints labeled process evidence such as
`dexter-core` and the process working directory instead of only raw PIDs. The
run-loop lifecycle smoke starts the exact `make run` tree used by the Dock
launcher and verifies UI restart and quit both make the parent run loop exit
and clean daemon sockets. The stale-Swift smoke recreates the orphaned SwiftPM
app shape from a failed HUD smoke and proves `make stop` kills it even after
the daemon socket is gone. The operator-ready smoke runs the consolidated prep
command, verifies stale sockets are gone, confirms the installed launcher is
executable, and confirms launchctl points at the local model store. The
diagnostic-bundle smoke proves the local report can be generated from any
current directory without starting a live session. The Dock launcher smoke
installs the wrapper into a temporary app bundle and validates its metadata and
Terminal-backed launch command without opening Terminal.

For Contacts-backed iMessage sends, `make live-smoke-message-contact` remains
opt-in because it needs a real Contacts entry, but denial mode does not send a
message. It now verifies the latest action receipt shows a resolved
Contacts-backed Messages AppleScript target and `Denied before execution.`

## Placement

Dexter no longer follows ordinary mouse movement between displays. Placement is
intentional:

- Press right `Option` to snap Dexter to the current mouse location.
- Keep right `Option` held and drag with the primary mouse button to reposition.
- Release right `Option` to save the new position.

For BetterTouchTool or other gesture tools, use:

```bash
/Users/jason/Developer/Dex/scripts/dexter-place.sh snap
```

More details are in `docs/DEXTER_PLACEMENT_CONTROLS.md`.

Regression coverage:

```bash
make live-smoke-hud-placement
```

That smoke verifies the placement command path and the important window
invariants: Dexter remains a tight `136x136` panel, transparent corners plus
top/bottom/left/right center samples pass clicks through, the orb center remains
clickable, window-background dragging stays disabled, mouse movement without
the primary button does not drag Dexter, and primary-button movement during
placement mode moves Dexter by the expected delta.

## Health

Use these commands against a running daemon:

```bash
make wait-for-ready
make doctor
make status
```

`make run` waits for `make wait-for-ready` before launching Swift. `pending`
means startup warmup is still in progress. While health is pending,
FAST/PRIMARY/EMBED rows should say `warming`. After startup warmup completes,
those same rows must say `warm`; `not warm` after warmup means Dexter needs
attention.

Health recovery guidance follows the same split:

- pending startup warmup: wait; no restart is suggested yet;
- daemon down: use `make open-app` or `make run`;
- model-store env drift: use `make operator-ready`;
- model not warm after startup: use `make operator-ready`, then restart Dexter
  from the app menu or run `make restart`;
- Ollama unreachable: open Ollama, then use `make operator-ready`;
- unexpected large resident Ollama runner: stop the runner named in the
  Suggested fixes block, then retry startup;
- degraded workers: use the HUD restart buttons or
  `dexter-cli --restart-component stt|tts|browser`.

Regression coverage:

```bash
make live-smoke-runtime-health
make live-smoke-startup-readiness
make live-smoke-hud-unavailable-health
```

`make live-smoke-runtime-health` is the focused acceptance slice for startup
and health/status behavior: readiness gating, CLI status, HUD status plus worker
restart, and HUD recovery guidance when the Rust core is unreachable.

That smoke starts the release core without Swift, verifies the socket gate,
waits for doctor-clean daemon health through `make wait-for-ready`, and confirms
the owned daemon exits without stale sockets. The unavailable-health smoke
launches the real Swift HUD with no Rust core and verifies the health surface
renders actionable recovery guidance instead of only a connection error.

Worker recovery commands restart only daemon-lifetime Python workers. They do
not unload, reload, or otherwise churn Ollama models:

```bash
make restart-stt
make restart-tts
make restart-browser
```

## Diagnostic bundle

When Dexter looks wrong and you need one artifact instead of scattered terminal
state:

```bash
make diagnostic-bundle
```

The report is written under:

```bash
docs/diagnostics/latest.md
```

It builds `dexter-cli`, then captures process state, sockets, launchctl
`OLLAMA_MODELS`, visible Ollama models/runners, disk state, Dock launcher
metadata, the installed Dock launcher command, doctor output, and the latest
live-smoke summary pointer. It does not include full operator status or recent
action receipts by default.

To include the richer status/action context when you explicitly want it:

```bash
DEXTER_DIAGNOSTIC_INCLUDE_STATUS=1 make diagnostic-bundle
```

Regression coverage:

```bash
make live-smoke-diagnostic-bundle
```

## Action safety acceptance

When you want one focused pass over Dexter's real-world action behavior without
running the full live suite:

```bash
make live-smoke-action-safety
```

That acceptance slice verifies the parts of Dexter that sit between model text
and side effects:

- external integrations fail closed and surface useful errors;
- `dexter-cli --why` can explain the latest blocked action from local evidence;
- shell, file, browser, and AppleScript action lanes hit the right policy gate;
- safe, denied, approved, and expired actions leave readable audit receipts;
- typed approval responses work and stale approvals expire;
- the HUD can show action history and explain a blocked action;
- destructive HUD actions are visibly denied and do not execute;
- long-lived subprocesses are cancelled when the operator interrupts.

The focused target deliberately excludes the opt-in Contacts/iMessage send
smokes. Those remain separate because they depend on local Contacts data and,
in the approve variant, can send a real message:

```bash
make live-smoke-message-contact
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" DEXTER_SMOKE_ALLOW_REAL_SEND=1 make live-smoke-message-contact-approve
```

To see the latest saved evidence for the focused acceptance slices without
starting Dexter:

```bash
make acceptance-status
make acceptance-status-strict
```

That report reads `docs/live-smoke-results/live-smoke-*.md` and shows the most
recent passing receipt for the combined acceptance battery plus operator
controls, runtime health, and action safety. The diagnostic bundle includes the
same section so a single report can answer which major acceptance slices have
already passed. The strict variant exits non-zero if any required acceptance
slice has no passing saved receipt.

To generate one fresh receipt for all three focused acceptance slices:

```bash
make live-smoke-acceptance
```

That command runs the union of `live-smoke-operator-controls`,
`live-smoke-runtime-health`, and `live-smoke-action-safety` without nesting
summary runs. It still leaves opt-in Contacts/iMessage tests and the broader
experimental/full-suite checks as separate commands.
