# Phase 47 Operator Launch Readiness

## Goal

Reduce launch and recovery friction now that Dexter is intended to behave like a
normal Dock app with visible live logs.

The problem was not one missing feature. It was scattered operator state:
model-store configuration, stale process cleanup, app installation, launch
commands, diagnostics, and recovery suggestions all existed in separate places.

## Outcome

Complete.

Dexter now has a consolidated operator readiness path:

```bash
make operator-ready
```

That command:

- stops stale Dexter processes and removes both runtime sockets;
- reasserts `OLLAMA_MODELS=/Users/jason/ollama-models` through `launchctl`;
- verifies Ollama can see the configured Dexter model set;
- builds the release Rust core and CLI;
- builds the Swift app;
- installs `~/Applications/Dexter.app`.

Normal launches now also self-heal the model-store assumption:

- the Makefile exports `OLLAMA_MODELS=/Users/jason/ollama-models` by default;
- the Dock launcher exports the same value before startup;
- the Dock launcher runs `make configure-ollama-models && make stop && make run`;
- the HUD restart Terminal path uses the same model-store reassertion before
  restarting.

Dexter also now has a low-risk diagnostic bundle:

```bash
make diagnostic-bundle
```

It writes `docs/diagnostics/latest.md` and captures process state, sockets,
launchctl model-store state, Ollama model/running-model state, disk state, Dock
launcher metadata, the installed Dock launcher command, doctor output, and the
latest live-smoke summary pointer. Full operator status/action receipt context
remains opt-in:

```bash
DEXTER_DIAGNOSTIC_INCLUDE_STATUS=1 make diagnostic-bundle
```

Finally, `dexter-cli --doctor` and `dexter-cli --status` now suggest concrete
launch commands when the daemon is down:

```text
cd /Users/jason/Developer/Dex && make open-app
cd /Users/jason/Developer/Dex && make run
```

Model-store warnings suggest:

```text
cd /Users/jason/Developer/Dex && make operator-ready
```

## Evidence

Focused shell/script checks:

```text
bash -n scripts/operator-ready.sh: PASS
bash -n scripts/diagnostic-bundle.sh: PASS
bash -n scripts/live-operator-ready-smoke.sh: PASS
bash -n scripts/live-diagnostic-bundle-smoke.sh: PASS
bash -n scripts/install-dexter-app.sh: PASS
make -n clean: PASS, removes /tmp/dexter.sock and /tmp/dexter-shell.sock
```

Focused live/operator checks:

```text
make operator-ready: PASS
make diagnostic-bundle: PASS
make live-smoke-dock-launcher: PASS
make live-smoke-operator-ready: PASS
make live-smoke-diagnostic-bundle: PASS
```

Builds and tests:

```text
cd src/swift && swift build: PASS
cargo test --bin dexter-cli: PASS, 60 passed
make smoke: PASS
```

Live doctor check while Dexter was intentionally stopped:

```text
dexter-cli --doctor: reports daemon down and prints make open-app / make run
suggestions.
```

## Remaining Work

None for this phase.

Future launch work should focus on distribution polish, not another parallel
startup path. The canonical operator prep path is now `make operator-ready`; the
canonical normal user launch is `~/Applications/Dexter.app` or `make open-app`.
