# ── Paths ──────────────────────────────────────────────────────────────────────

PROTO_DIR     := src/shared/proto
PROTO_FILE    := $(PROTO_DIR)/dexter.proto
SWIFT_GEN_DIR := src/swift/Sources/Dexter/Bridge/generated
RUST_CORE_DIR := src/rust-core
SWIFT_DIR     := src/swift

# grpc-swift 2.x ships as protoc-gen-grpc-swift-2 (not protoc-gen-grpc-swift).
# The plugin flag is --grpc-swift-2_out accordingly.
PROTOC_GEN_SWIFT     := $(shell which protoc-gen-swift)
PROTOC_GEN_GRPC_SWIFT := $(shell find /opt/homebrew/Cellar/protoc-gen-grpc-swift -name "protoc-gen-grpc-swift-2" 2>/dev/null | head -1)

# ── Runtime constants ─────────────────────────────────────────────────────────
#
# SOCKET_PATH must match constants::SOCKET_PATH in src/rust-core/src/constants.rs.
# SOCKET_TIMEOUT_SECS must match constants::SOCKET_TIMEOUT_SECS.
# 90 seconds accommodates a cold release cargo build on first run.

SOCKET_PATH         := /tmp/dexter.sock
SHELL_SOCKET_PATH   := /tmp/dexter-shell.sock
SOCKET_TIMEOUT_SECS := 90
READY_TIMEOUT_SECS  := 300
RUN_PID_FILE        := /tmp/dexter-make-run.pid
OLLAMA_MODELS      ?= /Users/jason/ollama-models
export OLLAMA_MODELS

# ── Targets ────────────────────────────────────────────────────────────────────

.PHONY: all setup proto ensure-core-not-running run-core run-core-debug run-swift wait-for-core wait-for-ready run stop restart operator-ready ready acceptance-status acceptance-status-strict diagnostic-bundle install-app open-app configure-ollama-models test test-inference test-e2e cli doctor status why events triggers inbox ack-event actions-last actions-recent restart-stt restart-tts restart-browser live-smoke-startup-readiness live-smoke-process-control live-smoke-stop-report live-smoke-run-loop-lifecycle live-smoke-stale-swift-stop live-smoke-operator-ready live-smoke-acceptance-status live-smoke-diagnostic-bundle live-smoke-dock-launcher live-smoke-recovery live-smoke-degraded-mode live-smoke-residency-proof live-smoke-ambient-events live-smoke-ambient-actions live-smoke-ambient-inbox live-smoke-ambient-trigger-actions live-smoke-external-failures live-smoke-operator-status live-smoke-action-diagnostic live-smoke-shortcut-action live-smoke-window-focus live-smoke-window-inspect live-smoke-ui-snapshot live-smoke-ui-click live-smoke-ui-type live-smoke-ui-select live-smoke-ui-toggle live-smoke-ui-pick live-smoke-cli live-smoke-action-matrix live-smoke-action-receipts live-smoke-approval-lifecycle live-smoke-message-contact live-smoke-message-contact-approve live-smoke-hud live-smoke-hud-new-session live-smoke-hud-lifecycle live-smoke-hud-placement live-smoke-placement-command live-smoke-hud-health live-smoke-hud-unavailable-health live-smoke-hud-action-history live-smoke-hud-action-diagnostic live-smoke-hud-approval live-smoke-action-cancel live-smoke-barge-in live-smoke-operator-controls live-smoke-runtime-health live-smoke-action-safety live-smoke-acceptance live-smoke-all live-smoke-summary smoke check-permissions clean help

## help: print this help message
help:
	@echo "Usage: make <target>"
	@echo ""
	@grep -E '^## [a-z][a-z-]*:' $(MAKEFILE_LIST) \
		| sed 's/^## /  /' \
		| column -t -s ':'

all: proto

## setup: verify all required toolchains and protoc plugins are available
##        (run `make check-permissions` to verify macOS TCC permissions)
setup:
	@bash scripts/setup.sh

## proto: compile dexter.proto → Swift and Rust artifacts
##
## grpc-swift 2.x ships the plugin as protoc-gen-grpc-swift-2 with flag --grpc-swift-2_out.
## (The old protoc-gen-grpc-swift generated grpc-swift 1.x code — wrong for this project.)
proto: $(PROTO_FILE)
	@echo "==> Generating Swift proto artifacts → $(SWIFT_GEN_DIR)"
	@mkdir -p $(SWIFT_GEN_DIR)
	protoc \
		--proto_path=$(PROTO_DIR) \
		--plugin=protoc-gen-swift=$(PROTOC_GEN_SWIFT) \
		--plugin=protoc-gen-grpc-swift-2=$(PROTOC_GEN_GRPC_SWIFT) \
		--swift_out=$(SWIFT_GEN_DIR) \
		--grpc-swift-2_out=$(SWIFT_GEN_DIR) \
		$(PROTO_FILE)
	@echo "==> Rust proto artifacts compiled by build.rs during cargo build"
	@echo "==> Proto generation complete"

## test: run the Rust core unit test suite (offline-safe, no Ollama required)
test:
	cd $(RUST_CORE_DIR) && cargo test

## test-e2e: run all integration tests (requires live Ollama + models)
##
## Includes Phase 4 InferenceEngine, Phase 6 orchestrator e2e, Phase 15 memory sample,
## and any future integration tests. All are marked #[ignore] in cargo test.
##
## Prerequisites:
##   1. Ollama running:       ollama serve
##   2. phi3:mini available:  ollama pull phi3:mini  (used by e2e session test)
##   3. For memory test:      any Apple Silicon Mac (vm_stat must be present)
##
## To run only unit tests: make test
## To run both:            make test && make test-e2e
test-e2e:
	cd $(RUST_CORE_DIR) && cargo test -- --ignored

## test-inference: deprecated alias for test-e2e
test-inference:
	@echo "⚠  test-inference is deprecated — use 'make test-e2e' instead"
	$(MAKE) test-e2e

## ensure-core-not-running: fail if another Dexter core already owns the socket
ensure-core-not-running:
	@if python3 -c "import socket,sys; s=socket.socket(socket.AF_UNIX); s.settimeout(1); sys.exit(0 if s.connect_ex('$(SOCKET_PATH)')==0 else 1)" 2>/dev/null; then \
		echo "ERROR: A Dexter core is already accepting connections at $(SOCKET_PATH)."; \
		echo "       Stop the existing core/UI first, then run 'make run' again."; \
		echo "       Current owner:"; \
		lsof -nU 2>/dev/null | grep -F -- '$(SOCKET_PATH)' || true; \
		exit 1; \
	fi

## run-core: start the Rust daemon in release mode (same artifact family used by CLI/live smoke)
run-core:
	cargo run --manifest-path $(RUST_CORE_DIR)/Cargo.toml --release --bin dexter-core

## run-core-debug: start the Rust daemon in debug mode intentionally
run-core-debug:
	cargo run --manifest-path $(RUST_CORE_DIR)/Cargo.toml --bin dexter-core

## cli: build the dexter-cli release binary (Phase 38 dev tool).
##
## Sends ClientEvent::TextInput events to the running daemon's gRPC socket —
## same socket Swift uses. Useful for scripted regression tests, dev-loop
## verification without starting Swift, and Phase 38b structured-action
## harnessing. See AGENTS.md "dexter-cli" section for usage.
##
## Builds release-mode for fast startup (debug build is ~3x slower to launch).
cli:
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-cli
	@echo "==> dexter-cli ready: $(RUST_CORE_DIR)/target/release/dexter-cli --help"

## doctor: build dexter-cli and run the lightweight daemon diagnostic
doctor: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --doctor

## status: build dexter-cli and print health plus recent action receipts
status: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --status

## why: build dexter-cli and explain why the latest action did or did not run
why: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --why

## events: build dexter-cli and print recent ambient event records
events: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --events --limit 20

## triggers: build dexter-cli and print ambient trigger definitions
triggers: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --triggers

## inbox: build dexter-cli and print unacknowledged ambient notices
inbox: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --inbox --limit 20

## ack-event: acknowledge an ambient notice by EVENT_ID, e.g. make ack-event EVENT_ID=...
ack-event: cli
	@test -n "$(EVENT_ID)" || (echo "ERROR: EVENT_ID is required, e.g. make ack-event EVENT_ID=..." >&2; exit 2)
	$(RUST_CORE_DIR)/target/release/dexter-cli --ack-event "$(EVENT_ID)"

## actions-last: build dexter-cli and print the latest local action receipt
actions-last: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --actions last

## actions-recent: build dexter-cli and print recent local action receipts
actions-recent: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --actions recent --limit 20

## restart-stt: restart the daemon-lifetime STT worker, then print post-restart health
restart-stt: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --restart-component stt

## restart-tts: restart the daemon-lifetime TTS worker, then print post-restart health
restart-tts: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --restart-component tts

## restart-browser: restart the daemon-lifetime browser worker, then print post-restart health
restart-browser: cli
	$(RUST_CORE_DIR)/target/release/dexter-cli --restart-component browser

## live-smoke-startup-readiness: verify make run gates Swift launch on doctor-ready health
##
## Starts a fresh release core without Swift, verifies the socket gate, waits for
## doctor-clean daemon health through `make wait-for-ready`, then confirms the
## owned daemon exits without stale sockets.
live-smoke-startup-readiness: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-startup-readiness-smoke.sh

## live-smoke-process-control: verify external make stop terminates a normal make run tree
##
## Starts the normal `make run` process tree, waits until Swift launch starts,
## then runs `make stop` from outside the tree and verifies the parent run loop
## exits and the daemon socket is gone.
live-smoke-process-control: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-process-control-smoke.sh

## live-smoke-ambient-events: verify daemon startup/health transitions write ambient events
live-smoke-ambient-events: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ambient-events-smoke.sh

## live-smoke-ambient-actions: verify action outcomes write ambient events
live-smoke-ambient-actions: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ambient-actions-smoke.sh

## live-smoke-ambient-inbox: verify trigger matches surface in HUD and become acknowledged
live-smoke-ambient-inbox: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ambient-inbox-smoke.sh

## live-smoke-ambient-trigger-actions: verify ask-approval/start-task trigger follow-up events
live-smoke-ambient-trigger-actions: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ambient-trigger-actions-smoke.sh

## live-smoke-stop-report: verify make stop prints labeled process evidence
live-smoke-stop-report: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	bash scripts/live-stop-report-smoke.sh

## live-smoke-run-loop-lifecycle: verify UI quit/restart exit the normal make run tree
live-smoke-run-loop-lifecycle: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-run-loop-lifecycle-smoke.sh

## live-smoke-stale-swift-stop: verify make stop kills stale repo-owned SwiftPM app processes
live-smoke-stale-swift-stop: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	bash scripts/live-stale-swift-stop-smoke.sh

## live-smoke-operator-ready: verify consolidated operator prep command and installed launcher
live-smoke-operator-ready: ensure-core-not-running
	bash scripts/live-operator-ready-smoke.sh

## live-smoke-diagnostic-bundle: verify diagnostic bundle generation from any cwd
live-smoke-diagnostic-bundle:
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-cli
	bash scripts/live-diagnostic-bundle-smoke.sh

## live-smoke-acceptance-status: verify acceptance status parsing from saved receipts
live-smoke-acceptance-status:
	bash scripts/live-acceptance-status-smoke.sh

## live-smoke-dock-launcher: validate the Dock app wrapper without opening Terminal
##
## Installs Dexter.app into a temporary directory, validates bundle metadata,
## verifies launcher shell syntax, and confirms the Terminal-backed command
## reasserts the model store before `make stop && make run`.
live-smoke-dock-launcher:
	bash scripts/live-dock-launcher-smoke.sh

## live-smoke-recovery: run automated worker recovery smoke (starts Rust core, no Swift UI)
##
## Builds release-mode dexter-core + dexter-cli, starts the core with logs at
## /tmp/dexter-recovery-smoke.log, waits for doctor OK, restarts browser/TTS/STT,
## verifies doctor after every restart, then stops the core.
live-smoke-recovery: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-recovery-smoke.sh --start-core

## live-smoke-degraded-mode: run controlled dependency-failure diagnostics smoke
##
## Starts isolated release cores with malformed config, unreachable Ollama, and
## missing worker paths. Verifies failures are explicit in doctor output and that
## every daemon exits without leaving stale sockets behind.
live-smoke-degraded-mode: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-degraded-mode-smoke.sh

## live-smoke-residency-proof: prove cross-process residency pinning on a safe-sized model blob
live-smoke-residency-proof:
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	bash scripts/live-residency-proof-smoke.sh

## live-smoke-external-failures: run deterministic external-integration failure smoke
##
## Starts a fresh release core with short test-only failure knobs, then verifies
## message_send cannot bypass Contacts resolution, AppleScript errors/timeouts
## are surfaced, and a failed screencapture demotes Vision to PRIMARY.
live-smoke-external-failures: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-external-failures-smoke.sh

## live-smoke-operator-status: run unified operator-status smoke
##
## Starts a fresh release core, writes a safe synthetic action receipt, and
## verifies `dexter-cli --status` prints coherent health, suggestions/result,
## and recent action context.
live-smoke-operator-status: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-operator-status-smoke.sh --start-core

## live-smoke-action-diagnostic: run latest-action explanation smoke
##
## Starts a fresh release core, drives a blocked synthetic action, then verifies
## `dexter-cli --why` explains the failure from local audit/session evidence.
live-smoke-action-diagnostic: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-action-diagnostic-smoke.sh --start-core

## live-smoke-shortcut-action: verify macOS Shortcut actions route through approval and audit
##
## Drives an exact synthetic `shortcut` ActionSpec and auto-denies it. This
## proves the lane parses, requests approval, records readable receipts, and
## does not execute an opaque Shortcut without operator approval.
live-smoke-shortcut-action: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-shortcut-action-smoke.sh

## live-smoke-window-focus: verify structured window focus executes and audits
##
## Drives an exact synthetic `window_focus` ActionSpec against Finder. This
## proves Dexter can bring a local app/window forward without model-written raw
## AppleScript and records a readable receipt for subsequent GUI targeting.
live-smoke-window-focus: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-window-focus-smoke.sh

## live-smoke-window-inspect: verify structured window inspection executes and audits
##
## Drives an exact synthetic `window_inspect` ActionSpec for the frontmost app.
## This proves Dexter can gather read-only window evidence before GUI work.
live-smoke-window-inspect: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-window-inspect-smoke.sh

## live-smoke-ui-snapshot: verify structured UI snapshot executes and audits
##
## Drives an exact synthetic `ui_snapshot` ActionSpec for the frontmost app.
## This proves Dexter can gather bounded read-only Accessibility control
## evidence before choosing a GUI interaction strategy.
live-smoke-ui-snapshot: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ui-snapshot-smoke.sh

## live-smoke-ui-click: verify structured UI click executes and audits
##
## Drives an exact synthetic `ui_click` ActionSpec against a temporary dialog.
## This proves Dexter can press one unambiguous visible control without model-
## written raw AppleScript or coordinate clicks.
live-smoke-ui-click: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ui-click-smoke.sh

## live-smoke-ui-type: verify structured UI text entry executes and audits
##
## Drives an exact synthetic `ui_type` ActionSpec against a temporary AppKit text
## field fixture. This proves Dexter can enter text into one unambiguous text field
## without model-written raw AppleScript keystrokes.
live-smoke-ui-type: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ui-type-smoke.sh

## live-smoke-ui-select: verify structured UI option selection executes and audits
##
## Drives an exact synthetic `ui_select` ActionSpec against a temporary AppKit
## pop-up fixture. This proves Dexter can choose one unambiguous visible option
## without model-written raw AppleScript menu scripts.
live-smoke-ui-select: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ui-select-smoke.sh

## live-smoke-ui-toggle: verify structured UI toggle state setting executes and audits
##
## Drives an exact synthetic `ui_toggle` ActionSpec against a temporary AppKit
## checkbox fixture. This proves Dexter can set one unambiguous visible toggle
## to a requested final state without blindly pressing it.
live-smoke-ui-toggle: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ui-toggle-smoke.sh

## live-smoke-ui-pick: verify structured UI row/item selection executes and audits
##
## Drives an exact synthetic `ui_pick` ActionSpec against a temporary AppKit
## table fixture. This proves Dexter can select one unambiguous visible row/item
## without model-written raw AppleScript selection scripts.
live-smoke-ui-pick: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-ui-pick-smoke.sh

## live-smoke-cli: run automated CLI live regressions (starts Rust core, no Swift UI)
##
## Builds release-mode dexter-core + dexter-cli, starts the core with logs at
## /tmp/dexter-cli-smoke.log, drives typed inputs through dexter-cli, then
## asserts routing logs for Humor Engine and normal chat behavior.
live-smoke-cli: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-cli-smoke.sh --start-core

## live-smoke-action-matrix: run deep CLI action approval matrix (shell/file/browser/AppleScript)
##
## Starts a fresh release core and drives the high-consequence action lanes
## separately from live-smoke-cli so the baseline CLI smoke stays short and less
## vulnerable to long Ollama runs.
live-smoke-action-matrix: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-cli-smoke.sh --start-core --action-matrix

## live-smoke-action-receipts: run live audit receipt regression (starts Rust core, no Swift UI)
##
## Drives safe, denied, and approved synthetic actions, then asserts
## `dexter-cli --actions recent/last` can inspect the resulting audit receipts.
live-smoke-action-receipts: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-action-receipts-smoke.sh --start-core

## live-smoke-approval-lifecycle: run typed + expired approval lifecycle regression
##
## Starts a release core with a short approval timeout, then verifies typed
## yes/no/cancel approval responses, delayed stale approvals, live receipts,
## audit history formatting, and the Rust-side timeout refusal.
live-smoke-approval-lifecycle: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-approval-lifecycle-smoke.sh --start-core

## live-smoke-message-contact: opt-in Contacts-backed iMessage approval smoke
##
## Requires DEXTER_SMOKE_CONTACT_NAME to name an existing non-self Contacts entry
## with a reachable phone or iMessage email. The test auto-denies the approval
## request, so it verifies Contacts resolution + approval gating without sending.
##
## Example:
##   DEXTER_SMOKE_CONTACT_NAME="Some Test Contact" make live-smoke-message-contact
live-smoke-message-contact: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-message-contact-smoke.sh --start-core

## live-smoke-message-contact-approve: opt-in real iMessage send approval smoke
##
## Requires DEXTER_SMOKE_CONTACT_NAME and DEXTER_SMOKE_ALLOW_REAL_SEND=1. This
## auto-approves the Messages approval request and sends the smoke-test message,
## so keep it out of broad smoke suites.
##
## Example:
##   DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" DEXTER_SMOKE_ALLOW_REAL_SEND=1 make live-smoke-message-contact-approve
live-smoke-message-contact-approve: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	DEXTER_SMOKE_APPROVAL_MODE=approve bash scripts/live-message-contact-smoke.sh --start-core

## live-smoke-hud: run automated Swift HUD live regression (starts Rust core + Swift UI)
##
## Builds release-mode dexter-core, starts it with logs at
## /tmp/dexter-hud-core-smoke.log, launches the real Swift app with a
## test-only DEXTER_HUD_SMOKE hook, submits one typed turn through
## HUDWindow.onTextSubmit, and asserts the HUD/client lifecycle logs.
live-smoke-hud: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	bash scripts/live-hud-smoke.sh --start-core

## live-smoke-hud-new-session: verify the Swift HUD can start a fresh daemon session
live-smoke-hud-new-session: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	DEXTER_HUD_SMOKE_NEW_SESSION=1 DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=8 bash scripts/live-hud-smoke.sh --start-core

## live-smoke-hud-lifecycle: run actual Swift HUD restart/quit lifecycle regression
live-smoke-hud-lifecycle: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	DEXTER_HUD_SMOKE_LIFECYCLE_ACTION=restart DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=8 bash scripts/live-hud-smoke.sh --start-core
	DEXTER_HUD_SMOKE_LIFECYCLE_ACTION=quit DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=8 bash scripts/live-hud-smoke.sh --start-core

## live-smoke-hud-placement: verify placement commands and transparent click-through invariants
live-smoke-hud-placement: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	DEXTER_HUD_SMOKE_PLACEMENT_SEQUENCE="snap,start,synthetic-nodrag:32:18,synthetic-drag:32:18,stop" DEXTER_HUD_SMOKE_EXIT_AFTER_SECS=8 bash scripts/live-hud-smoke.sh --start-core

## live-smoke-placement-command: verify external dexter-place.sh notifications reach Swift
live-smoke-placement-command: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	bash scripts/live-placement-command-smoke.sh

## live-smoke-hud-health: run Swift HUD status + worker restart regression
##
## Builds release-mode dexter-core, starts it with logs at
## /tmp/dexter-hud-health-core-smoke.log, launches the real Swift app with a
## test-only status hook, fetches HUD health plus recent actions, restarts the
## browser worker via RestartComponent, and asserts post-restart health returns
## to the HUD.
live-smoke-hud-health: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-hud-health-smoke.sh --start-core

## live-smoke-hud-unavailable-health: verify HUD recovery copy when Rust core is down
live-smoke-hud-unavailable-health: ensure-core-not-running
	cd $(SWIFT_DIR) && swift build
	bash scripts/live-hud-unavailable-health-smoke.sh

## live-smoke-hud-action-history: run Swift HUD recent-actions regression
##
## Builds release-mode dexter-core + dexter-cli, starts the core, creates a
## real action audit entry through the CLI, then asks the Swift HUD to fetch
## Recent Actions through the ActionHistory RPC.
live-smoke-hud-action-history: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-hud-action-history-smoke.sh --start-core

## live-smoke-hud-action-diagnostic: run Swift HUD latest-action explanation regression
##
## Builds release-mode dexter-core + dexter-cli, starts the core, creates a
## blocked raw message_send receipt through the CLI, then asks the Swift HUD to
## explain it using Health + ActionHistory + latest session evidence.
live-smoke-hud-action-diagnostic: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-hud-action-diagnostic-smoke.sh --start-core

## live-smoke-hud-approval: run Swift HUD approval-required action regression
##
## Builds release-mode dexter-core, starts it with logs at
## /tmp/dexter-hud-approval-core-smoke.log, launches the real Swift app with
## DEXTER_HUD_SMOKE_ACTION_APPROVAL=deny, and asserts the HUD receives and
## denies a destructive ActionRequest visibly, remembers the denial in context,
## and does not execute it.
live-smoke-hud-approval: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	bash scripts/live-hud-approval-smoke.sh --start-core

## live-smoke-action-cancel: run long-lived subprocess cancellation regression
##
## Builds release-mode dexter-core + dexter-cli, starts the core with logs at
## /tmp/dexter-action-cancel-smoke.log, runs a long-lived safe shell action,
## sends HotkeyActivated after FOCUSED in the same CLI session, and asserts the
## OS subprocess is gone.
live-smoke-action-cancel: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core --bin dexter-cli
	bash scripts/live-action-cancel-smoke.sh --start-core

## live-smoke-barge-in: run Swift TTS cancellation race regression
##
## Builds release-mode dexter-core, starts it with logs at
## /tmp/dexter-barge-core-smoke.log, launches the real Swift app in smoke mode,
## sends a fromVoice text turn so TTS reaches AudioPlayer, then interrupts after
## the first audio frame and asserts no buffer schedules after LISTENING.
live-smoke-barge-in: ensure-core-not-running
	cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-core
	bash scripts/live-barge-in-smoke.sh --start-core

## live-smoke-all: run the full live regression suite in a safe sequence
##
## Each target starts and stops its own release core. Keep this explicit rather
## than sharing one daemon so a leaked worker/socket in one smoke fails the next
## target instead of being hidden by shared process state.
live-smoke-all:
	$(MAKE) live-smoke-startup-readiness
	$(MAKE) live-smoke-process-control
	$(MAKE) live-smoke-stop-report
	$(MAKE) live-smoke-run-loop-lifecycle
	$(MAKE) live-smoke-stale-swift-stop
	$(MAKE) live-smoke-operator-ready
	$(MAKE) live-smoke-diagnostic-bundle
	$(MAKE) live-smoke-dock-launcher
	$(MAKE) live-smoke-recovery
	$(MAKE) live-smoke-degraded-mode
	$(MAKE) live-smoke-residency-proof
	$(MAKE) live-smoke-external-failures
	$(MAKE) live-smoke-operator-status
	$(MAKE) live-smoke-action-diagnostic
	$(MAKE) live-smoke-shortcut-action
	$(MAKE) live-smoke-window-focus
	$(MAKE) live-smoke-window-inspect
	$(MAKE) live-smoke-ui-snapshot
	$(MAKE) live-smoke-ui-click
	$(MAKE) live-smoke-ui-type
	$(MAKE) live-smoke-ui-select
	$(MAKE) live-smoke-ui-toggle
	$(MAKE) live-smoke-ui-pick
	$(MAKE) live-smoke-cli
	$(MAKE) live-smoke-action-matrix
	$(MAKE) live-smoke-action-receipts
	$(MAKE) live-smoke-approval-lifecycle
	$(MAKE) live-smoke-hud
	$(MAKE) live-smoke-hud-new-session
	$(MAKE) live-smoke-hud-lifecycle
	$(MAKE) live-smoke-hud-placement
	$(MAKE) live-smoke-placement-command
	$(MAKE) live-smoke-hud-health
	$(MAKE) live-smoke-hud-unavailable-health
	$(MAKE) live-smoke-hud-action-history
	$(MAKE) live-smoke-hud-action-diagnostic
	$(MAKE) live-smoke-hud-approval
	$(MAKE) live-smoke-action-cancel
	$(MAKE) live-smoke-barge-in

## live-smoke-summary: run live smokes and write a markdown receipt
##
## By default this runs the same target sequence as live-smoke-all, but captures
## each target's terminal output under docs/live-smoke-results/logs/<timestamp>/
## and writes docs/live-smoke-results/latest.md. To run a smaller pass:
##
##   DEXTER_SMOKE_SUMMARY_TARGETS="live-smoke-action-diagnostic live-smoke-operator-status" make live-smoke-summary
live-smoke-summary:
	bash scripts/live-smoke-summary.sh

## live-smoke-operator-controls: run the focused Dock/lifecycle/placement acceptance slice
##
## This is the fastest high-signal check for the operator-facing controls that
## should make Dexter feel like a normal app: installed launcher metadata,
## external stop/restart behavior, UI quit/restart, stale Swift cleanup, and
## placement click-through plus external placement command delivery.
live-smoke-operator-controls:
	bash scripts/live-smoke-summary.sh \
		live-smoke-dock-launcher \
		live-smoke-process-control \
		live-smoke-stop-report \
		live-smoke-run-loop-lifecycle \
		live-smoke-stale-swift-stop \
		live-smoke-hud-lifecycle \
		live-smoke-hud-placement \
		live-smoke-placement-command

## live-smoke-runtime-health: run the focused startup/status/HUD-health acceptance slice
##
## Verifies startup readiness, CLI operator status, HUD health with worker
## recovery, and HUD recovery guidance when the Rust core is unreachable.
live-smoke-runtime-health:
	bash scripts/live-smoke-summary.sh \
		live-smoke-residency-proof \
		live-smoke-startup-readiness \
		live-smoke-operator-status \
		live-smoke-hud-health \
		live-smoke-hud-unavailable-health

## live-smoke-action-safety: run the focused action policy/receipt/HUD acceptance slice
##
## Verifies action policy gates, external failure handling, local action
## diagnostics, audit receipts, approval lifecycle, HUD action history/Why,
## visible HUD approval denial, and long-lived subprocess cancellation.
live-smoke-action-safety:
	bash scripts/live-smoke-summary.sh \
		live-smoke-external-failures \
		live-smoke-action-diagnostic \
		live-smoke-shortcut-action \
		live-smoke-window-focus \
		live-smoke-window-inspect \
		live-smoke-ui-snapshot \
		live-smoke-ui-click \
		live-smoke-ui-type \
		live-smoke-ui-select \
		live-smoke-ui-toggle \
		live-smoke-ui-pick \
		live-smoke-action-matrix \
		live-smoke-action-receipts \
		live-smoke-approval-lifecycle \
		live-smoke-hud-action-history \
		live-smoke-hud-action-diagnostic \
		live-smoke-hud-approval \
		live-smoke-action-cancel

## live-smoke-acceptance: run the combined operator/runtime/action acceptance battery
##
## Produces one receipt covering the three focused acceptance slices without the
## extra experimental, opt-in, or broad full-suite targets.
live-smoke-acceptance:
	bash scripts/live-smoke-summary.sh \
		live-smoke-dock-launcher \
		live-smoke-process-control \
		live-smoke-stop-report \
		live-smoke-run-loop-lifecycle \
		live-smoke-stale-swift-stop \
		live-smoke-hud-lifecycle \
		live-smoke-hud-placement \
		live-smoke-placement-command \
		live-smoke-residency-proof \
		live-smoke-startup-readiness \
		live-smoke-operator-status \
		live-smoke-hud-health \
		live-smoke-hud-unavailable-health \
		live-smoke-external-failures \
		live-smoke-action-diagnostic \
		live-smoke-shortcut-action \
		live-smoke-window-focus \
		live-smoke-window-inspect \
		live-smoke-ui-snapshot \
		live-smoke-ui-click \
		live-smoke-ui-type \
		live-smoke-ui-select \
		live-smoke-ui-toggle \
		live-smoke-ui-pick \
		live-smoke-action-matrix \
		live-smoke-action-receipts \
		live-smoke-approval-lifecycle \
		live-smoke-hud-action-history \
		live-smoke-hud-action-diagnostic \
		live-smoke-hud-approval \
		live-smoke-action-cancel

## run-swift: start the Swift UI shell (requires run-core already running)
run-swift:
	cd $(SWIFT_DIR) && swift run

## wait-for-core: block until the newly launched Rust core socket is accepting connections.
##
## Uses a Python one-liner (socket.connect_ex) rather than `nc -z -U` because
## BSD netcat on macOS has a known bug where -z and -U combined always return
## non-zero even on a successful connect. Python's socket.connect_ex() is a
## direct syscall wrapper with no such issue.
##
## A stale socket file from a previous crash is correctly distinguished: connect_ex
## returns ECONNREFUSED (111) for a dead socket, 0 for a live one.
##
## Exits 0 when the socket is accepting connections, exits 1 with a clear error after timeout.
## The timeout accommodates a cold `cargo build` on first run (~30s on Apple Silicon).
wait-for-core:
	@echo "==> Waiting for Rust core socket at $(SOCKET_PATH) (timeout: $(SOCKET_TIMEOUT_SECS)s)..."
	@elapsed=0; \
	while [ $$elapsed -lt $(SOCKET_TIMEOUT_SECS) ]; do \
		if python3 -c "import socket,sys; s=socket.socket(socket.AF_UNIX); s.settimeout(1); sys.exit(0 if s.connect_ex('$(SOCKET_PATH)')==0 else 1)" 2>/dev/null; then \
			echo "==> Core socket accepting connections after $${elapsed}s"; \
			exit 0; \
		fi; \
		sleep 1; \
		elapsed=$$((elapsed + 1)); \
	done; \
	echo "ERROR: Rust core did not become ready within $(SOCKET_TIMEOUT_SECS)s."; \
	echo "       Check 'make run-core' output for compilation or startup errors."; \
	kill 0; \
	exit 1

## wait-for-ready: block until daemon health is ready, models/workers are warm, and doctor has no warnings.
wait-for-ready:
	@echo "==> Waiting for Dexter health readiness (timeout: $(READY_TIMEOUT_SECS)s)..."
	@cd $(RUST_CORE_DIR) && cargo build --release --bin dexter-cli >/dev/null
	@elapsed=0; \
	while [ $$elapsed -lt $(READY_TIMEOUT_SECS) ]; do \
		$(RUST_CORE_DIR)/target/release/dexter-cli --doctor >/tmp/dexter-wait-ready.out 2>&1 || true; \
		if grep -Fq "OK   daemon health      status ready" /tmp/dexter-wait-ready.out && grep -Fq "Result: OK - no failed checks." /tmp/dexter-wait-ready.out; then \
			echo "==> Dexter health ready after $${elapsed}s"; \
			exit 0; \
		fi; \
		sleep 2; \
		elapsed=$$((elapsed + 2)); \
	done; \
	echo "ERROR: Dexter health did not become ready within $(READY_TIMEOUT_SECS)s."; \
	echo "       Last doctor report:"; \
	cat /tmp/dexter-wait-ready.out 2>/dev/null || true; \
	kill 0; \
	exit 1

## run: start both processes (requires Ollama to be running for inference). Swift waits for
##      the core socket and then doctor-clean daemon readiness before launching.
##      Ctrl-C kills both processes.
run: ensure-core-not-running
	@core_pid=""; ui_pid=""; \
	echo $$$$ > $(RUN_PID_FILE); \
	cleanup() { \
		rm -f $(RUN_PID_FILE); \
		if [ -n "$$ui_pid" ]; then kill "$$ui_pid" >/dev/null 2>&1 || true; fi; \
		if [ -n "$$core_pid" ]; then kill "$$core_pid" >/dev/null 2>&1 || true; fi; \
		wait "$$ui_pid" >/dev/null 2>&1 || true; \
		wait "$$core_pid" >/dev/null 2>&1 || true; \
	}; \
	trap cleanup INT TERM EXIT; \
	$(MAKE) run-core & core_pid=$$!; \
	($(MAKE) wait-for-core && $(MAKE) wait-for-ready && $(MAKE) run-swift) & ui_pid=$$!; \
	wait

## stop: terminate any running Dexter UI/core processes and remove stale sockets
stop:
	@bash scripts/stop-dexter.sh

## restart: stop Dexter, then start the normal terminal-backed run loop
restart: stop run

## operator-ready: clean stale state, configure models, install app, and build launch artifacts
operator-ready:
	@bash scripts/operator-ready.sh

## ready: alias for operator-ready
ready: operator-ready

## acceptance-status: print latest focused live-smoke acceptance evidence
acceptance-status:
	@bash scripts/acceptance-status.sh

## acceptance-status-strict: fail if focused acceptance evidence is missing
acceptance-status-strict:
	@DEXTER_ACCEPTANCE_STRICT=1 bash scripts/acceptance-status.sh

## diagnostic-bundle: build dexter-cli and write one local launch/model/process diagnostic markdown report
diagnostic-bundle: cli
	@bash scripts/diagnostic-bundle.sh

## install-app: install a Dock-launchable Dexter.app wrapper in ~/Applications
install-app:
	@bash scripts/install-dexter-app.sh

## open-app: install and open the Dock-launchable Dexter.app wrapper
open-app: install-app
	@open "$$HOME/Applications/Dexter.app"

## configure-ollama-models: set launchctl OLLAMA_MODELS to Dexter's local runtime store
configure-ollama-models:
	@bash scripts/configure-ollama-models-env.sh

## clean: remove socket file and build artifacts
##
## Does NOT delete $(SWIFT_GEN_DIR)/*.swift — those files are committed source,
## not build artifacts. They are regenerated only by `make proto`, which requires
## protoc + plugins to be installed. Deleting them here would break `swift build`
## on any machine that doesn't have protoc installed.
clean:
	rm -f $(SOCKET_PATH) $(SHELL_SOCKET_PATH)
	cd $(RUST_CORE_DIR) && cargo clean

## setup-python: Install Python worker dependencies (kokoro, faster-whisper, playwright)
setup-python:
	cd src/python-workers && uv sync
	cd src/python-workers && uv run playwright install chromium
	@echo "Python workers ready. Models download automatically on first use."

## test-python: Run Python worker unit tests (no live models required)
test-python:
	cd src/python-workers && uv run pytest -v

## smoke: fast syntax+type check across all three layers (no Ollama required, < 60s)
##
## Uses `cargo check` (not build) — full type/borrow checking without producing artifacts.
## swift build is incremental and fast on warm cache. pytest validates Python workers.
## Run this before pushing any change to verify nothing is broken across all layers.
smoke:
	@echo "==> Rust type check"
	cd $(RUST_CORE_DIR) && cargo check
	@echo "==> Swift build"
	cd $(SWIFT_DIR) && swift build 2>&1 | tail -8
	@echo "==> Python worker tests"
	cd src/python-workers && uv run pytest -q
	@echo "==> Smoke check passed"

## check-permissions: check macOS TCC permissions required by Dexter (Accessibility, Microphone)
check-permissions:
	@bash scripts/permissions.sh
