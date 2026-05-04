# Dexter — AGENTS.md

This file is doctrine for any agent (Codex, Claude, automated tooling) working
on this repo. Read it before reading code. Authoritative spec lives in
`docs/DEXTER_PROJECT_PROPOSAL.md` and `docs/DEXTER_INSTRUCTIONS.md` — this
file compresses what's hard to derive from those + from the code.

## Identity

Dexter is an always-on macOS AI entity that lives at `NSWindowLevel.screenSaver`
and shares a screen with its operator. It is not a chatbot. It listens on a
hotkey for voice, observes app focus and clipboard changes, and takes
real-world actions (shell, AppleScript, browser automation) on the operator's
behalf. Treat it as a **security-sensitive automation system with full system
access**, not a toy assistant.

## Stack (settled — do not propose to redesign)

- **Two processes**, one machine, one user:
  - Swift app (`src/swift/`): UI only — `NSPanel`, Metal-rendered animated
    entity, voice capture / playback. Targets macOS 26.3, Swift 6.2.
  - Rust core (`src/rust-core/`): orchestrator, inference, model routing,
    context observation, action execution, retrieval, voice coordination,
    proactive engine. Edition 2021, tokio runtime.
- **IPC**: gRPC over Unix domain socket at `/tmp/dexter.sock`. Proto in
  `src/shared/proto/dexter.proto`. Both sides regenerate from this; the
  `Makefile` `proto` target drives it.
- **Inference**: local Ollama on a USB SSD (`/Volumes/BitHappens`). Four
  model tiers — FAST / PRIMARY / HEAVY / CODE — plus EMBED. Routing logic
  in `src/rust-core/src/inference/router.rs`; selection is auditable and
  unit-tested.
- **Voice**: persistent Python workers under `src/python-workers/workers/`.
  `tts_worker.py` (kokoro-82M), STT (`faster-whisper base.en`),
  `browser_worker.py` (Playwright). Workers speak length-prefixed JSON
  over Unix sockets.
- **Personality**: `config/personality/default.yaml`. Drives a large fraction
  of model behavior — action emission rules, anti-patterns, domain triggers.
  **YAML semantics are part of the system**; mismatches between YAML claims
  and the orchestrator's parsers are real bugs.

## Operating constraints (do not flag as issues)

- **No `.git` directory.** The user has chosen not to initialize. Do not
  propose CI / PRs / branches / hooks that assume git. Do not run `git`
  commands.
- **macOS-only, Apple Silicon, SIP disabled.** Cross-platform portability is
  not in scope.
- **Action serialization quirk**: the action type emitted by the model is
  `apple_script` (serde snake_case), not `applescript`. Intentional.
- **Always-warm VRAM budget**: FAST + PRIMARY + EMBED ≈ 24 GB of 36 GB.
  HEAVY/CODE swap on demand via `unload_after=true` and
  `pending_primary_rewarm`. Do not propose "load all models always."
- **Storage**: Ollama models live on `/Volumes/BitHappens`. `/Volumes/ByteMe`
  also exists but is **not used** by Dexter.
- **No JS/TS/Node anywhere.** No `package.json`, no `pnpm`, no Tauri/Electron.
  Swift renders the UI.
- **Persistent worker pre-warming** (STT, FAST/PRIMARY model warmups before
  "Ready." TTS) is intentional — see Phase 36 H1. Do not flag the long
  warmup as a startup-perf issue without reading the rationale.
- **Daemon-lifetime workers, not per-session** (Phase 38c): `CoreService`
  owns one `SharedDaemonState` (TTS worker, browser worker, warmup atomic
  flags, startup-greeting-sent flag). New gRPC sessions get *clones* of
  the shared state, NOT fresh workers. If you see code creating a new
  `VoiceCoordinator` or `BrowserCoordinator` per session, that's a
  regression — Phase 38c removed all those code paths. The `Arc`-wrapped
  internals make sharing safe; `VoiceCoordinator`/`BrowserCoordinator`
  derive `Clone` for this reason. Per-session state lives in
  `CoreOrchestrator` (conversation context, cancel token, in-flight
  actions, current entity state); cross-session resources live in
  `SharedDaemonState`.

## Threat model

Dexter is **single-user, on-device, no untrusted code expected on the box**.
Anything running as the operator's UID has the same access Dexter has — it
can call AppleScript directly, read `~/Library/Messages/chat.db`, etc. So
the gRPC socket at `/tmp/dexter.sock` and the shell-context socket at
`/tmp/dexter-shell.sock` treat **same-user local connections as trusted
peers**, not adversaries. Multi-user systems are out of scope.

The boundary that DOES matter — and where the policy gate, action audit,
self-send intercept, off-host detection, and category-classification
machinery all live — is between **the language model and the rest of the
system**. The model is the untrusted instruction source. Every place where
model text becomes a side effect (shell argv, AppleScript source, browser
action, filesystem path, message recipient) is a privilege boundary that
needs explicit Rust-side validation. Findings in those areas are real bugs
regardless of the same-user-trust assumption above.

If you are reviewing socket or IPC code: tightening permissions to 0600,
using `~/.dexter/run/` instead of `/tmp/`, or verifying peer UID where
macOS allows it are all fine defense-in-depth fixes. But "another local
process could connect to the socket" is not, by itself, a high-severity
finding under this threat model — flag it as a hardening opportunity, not
an exploit.

## Doctrine

### Security posture

The model output is **untrusted instruction**. Every place where model text
becomes a side effect (shell argv, AppleScript source, browser action,
filesystem path) is a privilege boundary. Be paranoid there.

Specifically:
- **AppleScript / shell argv built from runtime data** must escape `\`, `"`,
  newlines, and any control characters. See `build_self_send_script` in
  `src/rust-core/src/orchestrator.rs` for the canonical pattern.
- **Recipient resolution for messaging** must come from operator-validated
  config (`operator_self_handle`) or a Contacts lookup the Rust core
  performs and verifies — **never** from a phone number the model
  generated. Toll-free / 800-prefix numbers in model output are
  hallucinations until proven otherwise. (Lesson: Phase 37.9 / T8.)
- **Path arguments** must run through `expand_home()` / `expand_home_path()`
  before reaching the OS. Never pass `~/foo` as-is to a syscall.
- **Destructive actions** (`rm`, `kill`, `pkill`, `killall`, etc.) must hit
  the policy gate in `src/rust-core/src/action/policy.rs` and require
  explicit operator approval via the HUD warning pattern.
- **Off-host detection**: requests targeting "my linux box" / "the VM" /
  "ssh into ..." must NOT execute on the local Mac. See
  `is_off_host_request` in `orchestrator.rs`.
- **No secrets in logs.** `tracing` fields with operator content are fine
  for short query previews; full transcripts, credentials, tokens are not.

### Architecture rules

- **Rust core owns privileged execution, IPC, policy, persistence, and
  state machine.** Swift UI does not make policy decisions and does not
  execute actions.
- **Personality YAML is a contract with the model**, not a config file.
  Changes to it can change model behavior in ways the Rust code does not
  observe directly. Treat YAML changes with the same scrutiny as code.
- **Model routing must remain auditable**: a routing decision logs the
  domain, complexity, and chosen model. Do not collapse routing into
  opaque heuristics.
- **Long-running tasks need cancellation, timeout, and structured error
  reporting.** Generation, downloads (yt-dlp/curl/wget/ffmpeg/ffprobe at
  300s), AppleScript/shell — all of these must be cancellable.
- **Cancellation invariants**: `Arc<AtomicBool>` cancel tokens are stored
  AND replaced on barge-in. `JoinHandle::abort()` drops the reqwest
  stream → closes the Ollama HTTP connection → ends server-side
  generation. See Phase 33.

### Error handling & logging

- **Typed errors via `thiserror`** in production paths. Avoid `anyhow` in
  library code; it's fine in `main.rs` / `bin` glue.
- **No `unwrap()` / `expect()` in production paths.** Tests are exempt.
  If you need an "infallible" assertion, use a typed error and `match`.
- **No broad catches.** A `let _ = result;` that silently swallows an
  error is a bug. If we genuinely don't care about the failure, log it
  at `debug!` or `trace!` with a reason.
- **Logging is `tracing` with structured fields**, not `println!` or
  `eprintln!`. Reuse existing field names: `session`, `trace_id`,
  `agentic_depth`, `model`, `domain`, `complexity`, `load_ms`. Do not
  invent parallel field names.
- **`debug!` for hot-path detail, `info!` for state transitions, `warn!`
  for things the operator should notice in `/tmp/dexter.log`, `error!`
  for failures that produce a degraded user experience.**

### Code style

- **Match existing patterns over imposing new ones.** If the surrounding
  module uses `pub(crate)` helpers, don't introduce module-private free
  functions unless the visibility actually changes. If the module uses
  `Arc<Mutex<T>>`, switching to `RwLock` requires a stated reason.
- **Constants with non-obvious values must have a comment recording the
  reasoning** (and ideally the experimental data — see
  `PRIMARY_KEEPALIVE_PING_INTERVAL_SECS` in `constants.rs` for the model).
- **Tests are pinned to behavior, not implementation.** A passing test
  on a refactor is the goal; a passing test that asserts the new
  implementation's structure is fragile.

## Verification

Before claiming a change is complete, run:

```bash
# Rust core (fast — offline, no Ollama needed)
cd src/rust-core && cargo test --bin dexter-core

# Rust core release build (catches release-only issues)
cd src/rust-core && cargo build --release

# Swift build (proto regen first if proto changed)
cd /Users/jason/Developer/Dex && make proto
cd src/swift && swift build

# Live smoke (requires Ollama + workers running; make run uses release core)
bash scripts/live-smoke.sh
```

Current baseline: **479 Rust tests passing** as of Phase 38c. A change that
drops the count without a documented reason is a regression. New behavior
should land with a targeted test in the same commit.

If a check cannot be run, say exactly why — don't claim "verified" without
evidence.

## dexter-cli (Phase 38 dev tool)

`src/rust-core/src/bin/dexter-cli.rs` — non-interactive gRPC client. Sends
`ClientEvent::TextInput` events to `/tmp/dexter.sock` (the same socket the
Swift HUD uses). Useful for:

- **Scripted regression tests** — drives the daemon from Bash without an
  operator at the keyboard.
- **One-shot dev-loop verification** — test a routing change without
  starting the Swift app: `dexter-cli "explain how X works"` and watch the
  log.
- **Phase 38b harness** — when structured action types land, the CLI is the
  natural place to send synthetic ActionRequest events for testing.

Build + run:
```bash
# Terminal 1: start the release core + Swift UI
cd /Users/jason/Developer/Dex && make run

# Terminal 2: send typed input to that running daemon
cd src/rust-core && cargo build --release --bin dexter-cli
./target/release/dexter-cli "what's 2 plus 2"
./target/release/dexter-cli --quiet --auto-deny "rm -rf /tmp/foo"
printf "q1\nq2\n" | ./target/release/dexter-cli
```

`dexter-cli` is only a client. It sends input to whatever daemon owns
`/tmp/dexter.sock`; rebuild/restart the core with `make run` before using the
CLI to validate Rust-core changes.

Defaults to `from_voice=false` (HUD typed-mode behavior, no TTS). Use
`--from-voice` to exercise the TTS pipeline. Auto-denies destructive action
requests by default; pass `--auto-approve` only when the test explicitly
needs the destructive path executed.

What the CLI deliberately does NOT cover: actual audio playback (no
speakers), HUD visual rendering (markdown beautification, animated entity),
voice-input pipeline (STT — needs real microphone). Everything else is
identical to HUD typed input — same gRPC stream, same orchestrator code.

## File pointers (orientation)

Where to look first depending on what you're investigating:

- **Orchestration / state machine** → `src/rust-core/src/orchestrator.rs`
  (god-object central; ~6000 lines; contains `CoreOrchestrator`, the
  generation loop, action handling, intercepts, agentic continuation).
- **Model routing** → `src/rust-core/src/inference/router.rs` (domain
  classifier, complexity scorer, tier selection, per-tier rationale).
- **Action execution** → `src/rust-core/src/action/{engine,policy,executor}.rs`
  (script execution, destructive-command policy, path expansion).
- **Context observation** → `src/rust-core/src/context_observer.rs`
  (app focus, clipboard, focused element scrubbing for terminal bundles).
- **Proactive engine** → `src/rust-core/src/proactive/engine.rs` (rate-
  limit gates, prompt construction, low-value response filtering).
- **Voice pipeline** → `src/rust-core/src/voice/` (worker_client,
  coordinator, sentence chunker, protocol).
- **Configuration** → `src/rust-core/src/config.rs` (deserialized from
  `~/.dexter/config.toml`).
- **Constants with reasoning notes** → `src/rust-core/src/constants.rs`.
- **Personality** → `config/personality/default.yaml` (model-facing
  rules; heavy comments).
- **Proto** → `src/shared/proto/dexter.proto` (single source of truth
  for IPC shape).
- **Swift UI entry** → `src/swift/Sources/Dexter/`.
- **Phase history** → `docs/PHASE_*_SPEC.md` files (numerical order is
  chronological; not all phases ship — read the SPEC's "outcome" section).

## Known sharp edges (lessons paid for in past phases)

Things that look like bugs but are deliberate, or that bit us before:

- **`apple_script` (snake_case) NOT `applescript`** — Phase 31 fix; the
  personality YAML must use the snake_case form to match serde rename.
- **Persistent STT worker pre-warm** — Phase 23. The first STT request
  used to take 5–10s; now we pre-warm at startup. The `TRANSCRIPT_DONE`
  sentinel marks worker-side completion.
- **`gemma4:26b` aliased to both PRIMARY and VISION** — Phase 37. Vision
  queries must NOT unload the warm PRIMARY. See
  `ModelId::unload_after_use(&ModelConfig)`.
- **HEAVY swap orchestration** — Phase 37.5 B5. HEAVY-routed turns explicitly
  unload PRIMARY before HEAVY load, then re-warm PRIMARY in parallel
  while operator consumes HEAVY output. `pending_primary_rewarm` flag.
- **Generation cancellation requires `JoinHandle::abort()`** — Phase 33.
  A cancel-token check between `await stream.next()` calls is not enough;
  reqwest stream blocks for 160s+ before first token if Ollama is cold.
- **Terminal-workflow short-circuit** — Phase 36 H3. iMessage send actions
  bypass the standard "explain the result" continuation and emit "Sent."
  on success. Recipient errors still continue normally.
- **Off-host detection intercept** — Phase 37.5 B8. "On my linux box" / "ssh
  into ..." emits the command as text instead of executing locally.
- **iMessage self-send intercept** — Phase 37.9 / T8. `is_self_reference_request`
  + `operator_self_handle` config + `build_self_send_script` deterministic
  template. Runs at all agentic depths because Step-2 sends fire at depth 1
  after a Contacts lookup. Toll-free 855 numbers in self-send context are
  hallucinations.
- **Proactive prompt is `[SILENT]`-by-default** — Phase 37.9. Inverted from
  the older "opt-out" framing because small models defaulted to clock
  readouts. `should_suppress_proactive` aggregates `is_silent_response`
  + `is_low_value_response` (length-gated; substantive responses survive).
- **`PRIMARY_KEEPALIVE_PING_INTERVAL_SECS = 60`** — Phase 37.8. Earlier
  values (180s, 90s) cold-loaded under macOS page-reclamation pressure
  even with Ollama's `keep_alive: "30m"`. The 60s constant has the
  experimental log embedded in a doc comment; do not change without
  re-running the experiment.

## Memory & cross-session context

A persistent agent memory file lives outside this repo at
`~/.claude/projects/-Users-jason-Developer-Dex/memory/MEMORY.md`. It is
the source of truth for phase history, current model stack, and recent
session decisions. If you have access to it, read it. If not, this file
plus the SPEC docs in `docs/` are the next-best summary.

## Style for findings / reports

When this file is loaded as part of a code-review or audit task:

- Group by severity: P0 (exploit / data loss / safety), P1 (correctness
  bug), P2 (maintainability / reliability / test gap), P3 (polish).
- Each finding: file path + line, exact issue, why it matters, concrete
  fix, suggested test.
- Cite real symbols. If you can't quote the code in question, drop the
  finding rather than guess.
- One finding per record. Don't bundle.
- Be terse on rationale; expansive only on proposed code.
