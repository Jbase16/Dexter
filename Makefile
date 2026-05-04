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
SOCKET_TIMEOUT_SECS := 90

# ── Targets ────────────────────────────────────────────────────────────────────

.PHONY: all setup proto ensure-core-not-running run-core run-core-debug run-swift wait-for-core run test test-inference test-e2e cli smoke check-permissions clean help

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
## Exits 0 when the core is ready, exits 1 with a clear error after timeout.
## The timeout accommodates a cold `cargo build` on first run (~30s on Apple Silicon).
wait-for-core:
	@echo "==> Waiting for Rust core at $(SOCKET_PATH) (timeout: $(SOCKET_TIMEOUT_SECS)s)..."
	@elapsed=0; \
	while [ $$elapsed -lt $(SOCKET_TIMEOUT_SECS) ]; do \
		if python3 -c "import socket,sys; s=socket.socket(socket.AF_UNIX); s.settimeout(1); sys.exit(0 if s.connect_ex('$(SOCKET_PATH)')==0 else 1)" 2>/dev/null; then \
			echo "==> Core ready after $${elapsed}s"; \
			exit 0; \
		fi; \
		sleep 1; \
		elapsed=$$((elapsed + 1)); \
	done; \
	echo "ERROR: Rust core did not become ready within $(SOCKET_TIMEOUT_SECS)s."; \
	echo "       Check 'make run-core' output for compilation or startup errors."; \
	kill 0; \
	exit 1

## run: start both processes (requires Ollama to be running for inference). Swift shell waits for the core socket to accept
##      connections before launching — no fixed sleep, no silent race condition.
##      Ctrl-C kills both processes.
run: ensure-core-not-running
	@trap 'kill 0' INT; \
	$(MAKE) run-core & \
	$(MAKE) wait-for-core && $(MAKE) run-swift & \
	wait

## clean: remove socket file and build artifacts
##
## Does NOT delete $(SWIFT_GEN_DIR)/*.swift — those files are committed source,
## not build artifacts. They are regenerated only by `make proto`, which requires
## protoc + plugins to be installed. Deleting them here would break `swift build`
## on any machine that doesn't have protoc installed.
clean:
	rm -f $(SOCKET_PATH)
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
