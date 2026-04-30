# Phase 2 — Foundation
## Implementation Plan
### Authored: 2026-03-06

---

## 1. What Phase 2 Delivers

Phase 2 builds the load-bearing scaffolding that every subsequent phase imports from.
Nothing functional (inference, orchestration, context) is implemented here.
This phase is deliberately unglamorous — its value is that Phase 3 and beyond never
have to argue about where constants live, how config is loaded, or what log format to use.

**Three problems being solved:**

1. **Magic values scattered in source** — `SOCKET_PATH`, `CORE_VERSION`, buffer sizes,
   timeouts are currently hard-coded in `main.rs` and `server.rs`. Future phases
   adding new magic values will compound the problem. Phase 2 extracts all of them
   into a single `constants.rs` with named, typed, documented declarations.

2. **No runtime configurability** — the system currently cannot be tuned without
   recompiling. Model identifiers, socket path, log level, and personality config path
   are all compile-time constants. Phase 2 adds a TOML config loader with explicit
   defaults and validated fallback behavior so operators can adjust without recompiling.

3. **Logging setup is inline in main.rs** — and it will never be reusable there.
   Phase 2 moves it into `logging.rs` where future phases can access it
   (e.g., the orchestrator resetting log spans on session restart).

**Also delivered:**

- `config/personality/default.yaml` — the operator personality profile specified in
  IMPLEMENTATION_PLAN.md §7.2. Required from Phase 5 onward.
- `scripts/setup.sh` — environment checker that validates developer toolchain and
  reports installation instructions for anything missing.
- Makefile `test` target and `help` target.
- `~/.dexter/state/` directory creation on Rust core startup.

---

## 2. What Phase 2 Explicitly Does NOT Do

- **No module stubs.** `inference/`, `context/`, `action/`, `retrieval/`, `voice/`,
  `personality/`, `session/` modules are created when implemented. No placeholder files.
- **No proto changes.** `dexter.proto` is untouched. Proto finalization is Phase 3.
- **No Swift changes.** The Swift shell is Phase 11 + 12. `DexterClient.swift`
  stays as-is.
- **No Python workers.** Worker scaffolding comes later.
- **No Ollama integration.** `InferenceEngine` is Phase 4.
- **No orchestration.** `CoreOrchestrator` is Phase 6.

---

## 3. Files

### 3.1 New Files

| Path | Description |
|------|-------------|
| `src/rust-core/src/constants.rs` | All named constants |
| `src/rust-core/src/config.rs` | TOML config loader |
| `src/rust-core/src/logging.rs` | Tracing subscriber setup |
| `config/personality/default.yaml` | Default operator personality profile |
| `scripts/setup.sh` | Toolchain environment checker |

### 3.2 Modified Files

| Path | Change |
|------|--------|
| `src/rust-core/Cargo.toml` | Add `serde`, `toml`, `dirs` crates |
| `src/rust-core/src/main.rs` | Use `constants`, `config`, `logging` modules; create `~/.dexter/state/` |
| `src/rust-core/src/ipc/server.rs` | Import `CORE_VERSION`, `SESSION_CHANNEL_CAPACITY` from `constants` |
| `Makefile` | Add `test`, `help` targets; `setup` target calls `scripts/setup.sh` |

---

## 4. Detailed Specifications

### 4.1 `src/rust-core/src/constants.rs`

All named constants. Comments explain units and derivation.
No magic values permitted anywhere else in the codebase after Phase 2.

```
SOCKET_PATH             = "/tmp/dexter.sock"
SOCKET_TIMEOUT_SECS     = 30          // cold cargo build worst case on Apple Silicon
CORE_VERSION            = env!("CARGO_PKG_VERSION")  // injected from Cargo.toml
DEXTER_STATE_DIR        = ".dexter/state"  // relative to home_dir()
DEXTER_CONFIG_FILENAME  = "config.toml"    // relative to home_dir()/.dexter/
PERSONALITY_CONFIG_PATH = "config/personality/default.yaml"  // relative to cwd (project root)
```

`SESSION_CHANNEL_CAPACITY` is intentionally absent. The literal `16` in Phase 1's
`server.rs` session handler is a Phase 1 stub that Phase 6 replaces entirely with the
real orchestrator event loop. Naming a constant for a value in code scheduled for
wholesale replacement is false permanence — it implies the constant has authority it
doesn't have. Channel capacity constants are defined in Phase 6 when the channels
are designed, with the right names for the right channels.

### 4.2 `src/rust-core/src/config.rs`

**Purpose:** Load operator configuration from `~/.dexter/config.toml`. Return validated
defaults if the file is absent. Fail with structured error if the file is present but
malformed (do not silently ignore bad config).

**Config file location:** `~/.dexter/config.toml` — resolved via `dirs::home_dir()`.

**Schema (Rust struct → TOML):**

```toml
# ~/.dexter/config.toml — all fields optional; defaults shown

[core]
socket_path       = "/tmp/dexter.sock"
state_dir         = "/Users/<you>/.dexter/state"   # computed from home_dir if absent
personality_path  = "config/personality/default.yaml"

[models]
fast    = "qwen3:8b"
primary = "mistral-small:24b"
heavy   = "deepseek-r1:32b"
code    = "deepseek-coder-v2:16b"
vision  = "llama3.2-vision:11b"
embed   = "mxbai-embed-large"

[logging]
level  = "info"          # trace | debug | info | warn | error
format = "auto"          # auto | json | pretty
                         # auto = json if stdout is not a TTY, pretty if TTY
```

**Behavior contract:**

- File absent → defaults used; logged at `INFO`: `"No config at {path} — using defaults"`
- File present, valid → config loaded; logged at `DEBUG`: `"Config loaded from {path}"`
- File present, malformed TOML → logged at `ERROR` with parse error detail; `process::exit(1)`
- Any field absent in file → that field's default is used

**Public API (used by `main.rs`):**

```rust
pub fn load() -> anyhow::Result<DexterConfig>
pub struct DexterConfig {
    pub core: CoreConfig,
    pub models: ModelConfig,
    pub logging: LoggingConfig,
}
```

All structs derive `Debug`, `Deserialize`, and provide `Default`.
All defaults implemented via `impl Default` (not `#[serde(default)]` on fields).

### 4.3 `src/rust-core/src/logging.rs`

**Purpose:** Initialize the `tracing` subscriber. Called once in `main` before any other
component. Never panics — returns `anyhow::Result`.

**Format selection:**

```
if config.logging.format == LogFormat::Json
   OR (config.logging.format == LogFormat::Auto AND stdout is not a TTY)
→ JSON (for log aggregation, piping to files, production)

if config.logging.format == LogFormat::Pretty
   OR (config.logging.format == LogFormat::Auto AND stdout is a TTY)
→ pretty-print (for interactive development)
```

TTY detection: `atty::is(atty::Stream::Stdout)` — add `atty = "0.2"` dependency.

**Log level:** From `config.logging.level`, parsed into `EnvFilter`. If the
`RUST_LOG` environment variable is set, it takes precedence (standard convention).

**Public API:**

```rust
pub fn init(config: &LoggingConfig) -> anyhow::Result<()>
```

### 4.4 `src/rust-core/src/main.rs` (updated)

Changes from current:
1. Declare `mod constants; mod config; mod logging;` at top
2. Remove inline `pub const SOCKET_PATH` and `pub const CORE_VERSION` definitions
   (they move to `constants.rs`)
3. Call `logging::init(&cfg.logging)?` as the first action in `main`
4. Call `config::load()?` to get `DexterConfig`
5. Use `cfg.core.socket_path` (from config) rather than `constants::SOCKET_PATH`
   directly — config can override the default
6. Call `dirs::ensure_state_dir(&cfg.core.state_dir)?` to create `~/.dexter/state/`
   on startup. This is a free function in `config.rs`, not a standalone module.
7. Log startup with version AND config path AND socket path

### 4.5 `src/rust-core/src/ipc/server.rs` (updated)

- Replace `use crate::CORE_VERSION` with `use crate::constants::CORE_VERSION`
- The literal `16` in `mpsc::channel::<...>(16)` stays as-is — it belongs to a
  Phase 1 stub that Phase 6 replaces wholesale. It is not a candidate for a named
  constant until the Phase 6 channel design is done.
- No other changes

### 4.6 `config/personality/default.yaml`

Full operator personality profile as specified in IMPLEMENTATION_PLAN.md §7.2.
This file is read by Phase 5 (PersonalityLayer). Writing it in Phase 2 means
Phase 5 has no bootstrapping ceremony and no "create config first" step.

```yaml
name: "Dexter"
version: "1.0"

system_prompt_prefix: |
  You are Dexter. You run at the system level, not inside any application.
  You share a screen with your operator. You are aware of what's happening
  on the machine without being told — you observe it.

  Your communication style is dry, sharp, and occasionally immature when
  the moment clearly calls for it. You shift to fully serious mode when
  something requires it — without announcing the transition. No "switching
  to serious mode now." The way a competent person would just do it.

  You do not make things up. When you are uncertain about a fact, you say so
  explicitly and then retrieve or acknowledge the limit. Confident hallucination
  is not something you do.

tone_directives:
  - "Never use corporate hedging language ('I'd be happy to', 'Certainly!', 'Great question!')"
  - "Sarcasm is appropriate when clearly warranted. Overuse kills it — use it precisely."
  - "Match the operator's register. Formal when they are formal. Blunt when they are blunt."
  - "One-word answers are fine when one word is all the question deserves."

response_style:
  max_verbosity: "medium"
  code_always_formatted: true
  never_pad_to_seem_thorough: true

anti_patterns:
  - "Starting responses with 'Certainly!', 'Of course!', 'Great!', 'Sure!'"
  - "Ending with 'Let me know if you need anything else!'"
  - "Over-explaining when the operator clearly already understands the concept"
  - "Using passive voice to avoid directness"

lora_adapter_path: null   # Path to fine-tuned LoRA adapter. null = base model only.
                          # Set in Phase 5+ when operator-specific adapter is trained.
```

### 4.7 `scripts/setup.sh`

Executable shell script. Checks that all required tools are present and reachable.
Reports each check explicitly. Exits 1 on any failure with an installation command.
Does NOT attempt to install anything — it only diagnoses and instructs.

**Checks:**

| Tool | Minimum | Install instruction on failure |
|------|---------|-------------------------------|
| `rustc` | 1.92.0 | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| `cargo` | 1.92.0 | (same, comes with rustc) |
| `swift` | 6.0 | Install Xcode from App Store |
| `python3` | 3.12 | `brew install python@3.12` |
| `protoc` | any | `brew install protobuf` |
| `protoc-gen-swift` | any | `brew install swift-protobuf` |
| `protoc-gen-grpc-swift-2` | any | `brew install grpc-swift` |
| `ollama` | any | `brew install ollama` |

Also creates `~/.dexter/state/` if it doesn't exist (idempotent).

Exit codes: `0` = all checks pass, `1` = at least one tool missing.

### 4.8 Makefile additions

**`test` target:**
```makefile
test:
	cd $(RUST_CORE_DIR) && cargo test
```

**`help` target (default if no target given):**
Prints each target with its `##`-prefixed comment. The existing targets already have
`##` comments — the help target just prints them.

**`setup` target** (replaces inline checks with script delegation):
```makefile
setup:
	@bash scripts/setup.sh
```

---

## 5. New Cargo Dependencies

```toml
serde = { version = "1",   features = ["derive"] }   # Config deserialization
toml  = "0.8"                                          # TOML file format
dirs  = "5"                                            # home_dir() resolution
atty  = "0.2"                                          # TTY detection for log format
```

All are zero-unsafe, actively maintained, and have no transitive dependency concerns.

---

## 6. Acceptance Criteria

Phase 2 is complete when **all** of the following pass:

| # | Test | Pass condition |
|---|------|----------------|
| 1 | `cargo build` | Zero warnings, zero errors |
| 2 | `make test` | `cargo test` exits 0 (no tests yet is acceptable) |
| 3 | `make setup` | Exits 0 with all tools present; clear pass/fail per tool |
| 4 | `make run-core` — no config file | Logs "using defaults" at INFO; `~/.dexter/state/` created; core starts normally |
| 5 | `make run-core` — with valid config file | Logs "config loaded from {path}" at DEBUG; values from file override defaults |
| 6 | `make run-core` — with malformed config | Logs structured error at ERROR; exits 1; no panic, no unwrap backtrace |
| 7 | Zero magic values | `grep` on `main.rs` and `server.rs` finds no string literals (path, version) or bare integer literals that are constants |
| 8 | `make run` | Both processes start and disc turns green (regression check against Phase 1) |
| 9 | `config/personality/default.yaml` | File is valid YAML; all required keys present |

---

## 7. Dependency Order Within Phase

1. `constants.rs` — no imports, no dependencies
2. `config.rs` — imports `constants.rs`
3. `logging.rs` — imports `config.rs` (LoggingConfig)
4. `main.rs` update — imports all three; must compile after them
5. `server.rs` update — imports `constants.rs`; independent of config/logging
6. `config/personality/default.yaml` — no code dependency
7. `scripts/setup.sh` — no code dependency
8. `Makefile` updates — no code dependency

Items 6, 7, 8 can be written at any point.

---

## 8. Non-obvious Design Notes

**Why `~/.dexter/state/` creation is in the Rust binary, not `setup.sh`:**
The state directory location is configurable (via `config.core.state_dir`).
Only the binary knows the resolved path at runtime. `setup.sh` only knows the
default. Creating the directory in the binary ensures it always exists before
first write, regardless of what path is configured.

**Why `atty` for TTY detection, not `std::io::IsTerminal`:**
`std::io::IsTerminal` is stable from Rust 1.70+. We have 1.92 — it's available.
Actually, use `std::io::IsTerminal` instead of `atty` — it's part of stdlib and
saves a dependency. Override: use `use std::io::IsTerminal; stdout().is_terminal()`.
Update: remove `atty` from dependencies, use stdlib.

**Why defaults in `impl Default` not `#[serde(default = "fn")]`:**
`impl Default` makes defaults testable in isolation. You can write a unit test
that asserts `DexterConfig::default().models.fast == "qwen3:8b"` without touching
TOML parsing at all. `#[serde(default = "fn")]` ties the default to the deserializer.

**Why `config.core.socket_path` from config, not `constants::SOCKET_PATH` directly:**
Phase 15 hardening may want to test with a non-default socket path (e.g.,
`/tmp/dexter-test.sock` during integration tests). If the binary hardcodes the
socket path from constants and ignores the config field, that test path is impossible.
Using the config value (which defaults to the constant) keeps the door open.

---

*Phase 2 plan authored: 2026-03-06*
*Depends on: Phase 1 complete (all 9 criteria), IMPLEMENTATION_PLAN.md v1.1*
*Next phase on completion: Phase 3 — IPC Contract Finalization*
