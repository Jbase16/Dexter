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
scripts/restart-dexter-ui.sh
```

That gives Dexter a normal Dock/app entry while keeping the live Rust and Swift
logs visible in the associated Terminal window. The launcher reasserts the local
hot model store and passes the Dock app path into the lifecycle script, so
ordinary Dock launches and HUD restarts use the same detached-core startup,
readiness, and cleanup path.

## App Menu

The Dexter menu exposes the controls that should not require hunting for the
original `make run` terminal:

- `Dexter > New Session`
- `Dexter > Show Dexter Status`
- `Dexter > Show Recent Actions`
- `Dexter > Explain Latest Action`
- `Dexter > Create Diagnostic Bundle`
- `Dexter > Move Dexter to Mouse`
- `Dexter > Toggle Dexter Placement Drag`
- `Dexter > Stop Dexter Placement Drag`
- `Dexter > Restart Dexter`
- `Dexter > Quit Dexter`

`Restart Dexter` and `Quit Dexter` first show a short HUD confirmation, then
restart or terminate the Swift app and Rust core. `New Session` keeps the app
running and opens a fresh gRPC session. `Create Diagnostic Bundle` writes the
same local markdown report as `make diagnostic-bundle` and shows the report
paths in the HUD.

The restart path is Terminal-backed for live logs, but the lifecycle itself is
centralized in `scripts/restart-dexter-ui.sh`: it reasserts
`OLLAMA_MODELS`, uses `make restart-core` for the detached core, tails
`/tmp/dexter-core.log`, runs `make run-swift`, and stops the core when that
Terminal-backed Swift session exits.

Regression coverage:

```bash
make live-smoke-operator-controls
make live-smoke-detached-core
make live-smoke-hud-new-session
make live-smoke-hud-lifecycle
make live-smoke-hud-diagnostic-bundle
make live-smoke-process-control
make live-smoke-run-loop-lifecycle
make live-smoke-stale-swift-stop
make live-smoke-operator-ready
make live-smoke-diagnostic-bundle
make live-smoke-dock-launcher
```

`make live-smoke-operator-controls` is the focused acceptance slice for the
operator-facing controls: Dock launcher metadata and centralized Terminal
lifecycle command, detached core lifecycle,
external stop, labeled stop output, UI restart/quit through the normal run loop,
stale Swift cleanup, and placement click-through plus external placement
command delivery.

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

For Contacts-backed iMessage sends, `make live-smoke-message-contact-dry-run`
is the safest first probe: it asks for a deliberately missing Contacts name and
verifies Dexter's deterministic text-message parser reaches Rust-side Contacts
resolution and refuses before approval or delivery. `make
live-smoke-message-contact` remains opt-in because it needs a real Contacts
entry, but denial mode does not send a message. It verifies the latest action
receipt shows a resolved Contacts-backed Messages AppleScript target and
`Denied before execution.` The local v1 proof used `Jason Phillips` in deny
mode, which resolves the recipient and proves the approval gate without sending.

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
make live-smoke-local-answers
make live-smoke-hud-unavailable-health
```

`make live-smoke-runtime-health` is the focused acceptance slice for startup
and health/status behavior: readiness gating, deterministic local answers for
RAM/CPU/Dexter Notices, CLI status, HUD status plus worker restart, and HUD
recovery guidance when the Rust core is unreachable.

That smoke starts the release core without Swift, verifies the socket gate,
waits for doctor-clean daemon health through `make wait-for-ready`, and confirms
the owned daemon exits without stale sockets. The unavailable-health smoke
launches the real Swift HUD with no Rust core and verifies the health surface
renders actionable recovery guidance instead of only a connection error.
The local-answers smoke sends ordinary operator questions like "what's using so
much RAM right now?" and "what do those Dexter Notices mean?" through
`dexter-cli` and verifies Dexter answers from macOS process/memory telemetry or
ambient-event state instead of model speculation.

Worker recovery commands restart only daemon-lifetime Python workers. They do
not unload, reload, or otherwise churn Ollama models:

```bash
make restart-stt
make restart-tts
make restart-browser
```

The HUD Status surface exposes the same worker controls directly. Open Status
from Dexter's HUD and use the restart buttons for STT, TTS, or Browser when a
worker is stale, noisy, or behaving oddly. These buttons call the daemon
`RestartComponent` RPC; they do not route through the model and do not execute
arbitrary actions.

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

When you want the fast day-to-day action safety signal, use the shared-core
lane:

```bash
make live-smoke-action-safety-shared
```

That target starts one release core, waits for doctor-ready health, then runs
the compatible CLI/action checks against the shared daemon. It covers local
action diagnostics, the synthetic action matrix, deterministic browser recovery
evidence, UI/window action receipts, action receipts, approval lifecycle, and
action cancellation. It does not prove every individual smoke can launch and
warm a fresh daemon.

When you want the isolated release-grade action safety pass:

```bash
make live-smoke-action-safety
```

That acceptance slice verifies the parts of Dexter that sit between model text
and side effects, with a fresh daemon per target and fail-fast behavior:

- external integrations fail closed and surface useful errors;
- `dexter-cli --why` can explain the latest blocked action from local evidence;
- shell, file, browser, AppleScript, window, and UI action lanes hit the right
  policy gate;
- failed UI/browser action outcomes are persisted as privacy-safe C3 turn
  records with typed diagnostics;
- deterministic browser recovery produces typed evidence instead of corrupting
  browser health;
- safe, denied, approved, and expired actions leave readable audit receipts;
- typed approval responses work and stale approvals expire;
- long-lived subprocesses are cancelled when the operator interrupts.

When you want the full action safety plus HUD/model-driven browser sweep:

```bash
make live-smoke-action-safety-full
```

That target adds the model-driven browser recovery check and Swift HUD action
surfaces, including typed UI/window failure rendering in Recent Actions and
Why. It intentionally continues after failures so the receipt shows the whole
sweep.

All action safety lanes deliberately exclude the opt-in Contacts/iMessage send
smokes. Those remain separate because they depend on local Contacts data and,
in the approve variant, can send a real message:

```bash
make live-smoke-message-contact-dry-run
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
controls, runtime health, action safety, and the full action/HUD sweep. The
diagnostic bundle includes the same section so a single report can answer which
major acceptance slices have already passed. The strict variant exits non-zero
if any required acceptance slice has no passing saved receipt.

To generate one fresh receipt for all three focused acceptance slices:

```bash
make live-smoke-acceptance
```

That command runs the union of `live-smoke-operator-controls`,
`live-smoke-runtime-health`, and `live-smoke-action-safety` without nesting
summary runs. It still leaves opt-in Contacts/iMessage tests and the broader
experimental/full-suite checks as separate commands.

For the Daily-driver v1 release gate, run:

```bash
make daily-driver-v1-gate
```

That runs the combined acceptance battery, the full action/HUD/model-driven
browser recovery sweep, strict acceptance-status verification, and then prints
the short manual checklist. To print only the manual checklist:

```bash
make daily-driver-v1-checklist
```

The v1 gate still excludes real-send Contacts/iMessage checks unless you opt in
with the explicit messaging smoke commands above. Voice remains maintenance-only
for v1: existing STT/TTS health and restart behavior must stay intact, but
richer voice UX is not a release blocker.
