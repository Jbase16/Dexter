# Dexter

An always-on macOS AI entity that shares a screen with its operator. Not a chatbot. Not an assistant. A persistent system-level presence that is already running when you sit down, has been paying attention, and can take over entirely or stay completely quiet depending on what the moment requires.

---

## What This Is

Dexter lives at `NSWindowLevel.screenSaver` — above every other window, uncoverable, always visible. Other applications open beneath him. He exists on a layer normal applications don't occupy.

He is aware of his environment without being told what's happening. Active application, terminal output, clipboard content, focused UI element — all of it flows into his reasoning continuously through event-driven observation. He does not wait to be summoned. He does not require context to be re-explained each turn.

He has no guardrails. He does not refuse to engage with topics because they're uncomfortable, explicit, or outside what a corporate AI product would touch. He operates as an extension of his operator — not as a liability hedge.

He takes real actions: shell commands, AppleScript, browser automation, iMessage. Not suggestions. Execution.

---

## Architecture

Two processes, one machine:

```
┌─────────────────────────────────┐     gRPC / Unix socket      ┌──────────────────────────────┐
│         Swift UI Process        │◄──────────────────────────►│      Rust Core Process       │
│                                 │     /tmp/dexter.sock        │                              │
│  • NSPanel at .screenSaver      │                             │  • Orchestrator + state      │
│  • Metal animated entity        │                             │  • Model routing             │
│  • Voice capture (STT trigger)  │                             │  • Context observation       │
│  • TTS audio playback           │                             │  • Action execution          │
│  • HUD conversation display     │                             │  • Memory + retrieval        │
│  • Hotkey handling              │                             │  • Proactive engine          │
└─────────────────────────────────┘                             └──────────────────────────────┘
                                                                           │
                                                              ┌────────────┴────────────┐
                                                              │   Python Workers        │
                                                              │  • tts_worker.py        │
                                                              │    (kokoro-82M)         │
                                                              │  • stt_worker.py        │
                                                              │    (faster-whisper)     │
                                                              │  • browser_worker.py    │
                                                              │    (Playwright)         │
                                                              └─────────────────────────┘
```

**Swift** handles UI only — rendering, voice capture, audio playback, HUD display. It makes no policy decisions and executes no actions.

**Rust core** owns everything else: orchestration, inference, routing, context observation, action execution, memory, retrieval, and the proactive engine. The policy gate, action audit, and privilege boundary enforcement all live here.

**Python workers** run as daemon-lifetime subprocesses. One TTS worker (kokoro-82M), one persistent STT worker (faster-whisper base.en), one browser worker (Playwright). Workers are pre-warmed at startup and shared across all sessions.

---

## Model Stack

All inference is local. No data leaves the machine. Ever.

| Tier | Model | VRAM | Role |
|------|-------|------|------|
| FAST | `qwen3:8b` | ~5 GB | Chat, routing, agentic continuation |
| PRIMARY | `gemma4:26b` | ~18 GB | Reasoning, analysis, iMessage, multimodal |
| HEAVY | `deepseek-r1:32b` | ~20 GB | Offsec, deep reasoning, procedural walkthroughs |
| CODE | `deepseek-coder-v2:16b` | ~10 GB | Code generation (uncensored) |
| VISION | `gemma4:26b` | — | Aliased to PRIMARY — Gemma 4 is natively multimodal |
| EMBED | `mxbai-embed-large` | ~0.7 GB | Memory recall and retrieval |

**Always-warm**: FAST + PRIMARY + EMBED ≈ 24 GB of 36 GB unified memory  
**On-demand**: HEAVY and CODE swap in when routed — PRIMARY unloads first to make room, rewarms in parallel while the operator reads output

HEAVY and CODE are specifically chosen for being uncensored on offsec and red-team queries. This is a deliberate architectural decision, not an oversight.

**Inference runtime**: Ollama 0.15.1, models on internal NVMe

---

## Hardware Requirements

- macOS, Apple Silicon
- Minimum 24 GB unified memory (36 GB recommended for HEAVY/CODE swaps)
- SIP disabled — required for system-level accessibility APIs
- Ollama running locally

This is a single-user, on-device system. Cross-platform portability is not a goal.

---

## IPC

gRPC over Unix domain socket at `/tmp/dexter.sock`. Proto definition at `src/shared/proto/dexter.proto` — single source of truth for all IPC shape. Both Swift and Rust regenerate from this file.

Shell context integration via a separate socket at `/tmp/dexter-shell.sock`.

---

## Security Model

The boundary that matters is between **the language model and the rest of the system**. The model is the untrusted instruction source. Every place where model text becomes a side effect — shell argv, AppleScript source, browser action, filesystem path, message recipient — is a privilege boundary with explicit Rust-side validation.

Local socket connections from the same user are treated as trusted peers. Multi-user systems are out of scope.

---

## Key Capabilities

**Context observation** — AXUIElement + NSWorkspace + CGEventTap → continuous awareness of active app, focused element, clipboard content, terminal output. Terminal bundles (iTerm2, Terminal, Warp, etc.) have their content scrubbed before context injection to prevent scrollback leaks.

**Voice pipeline** — Persistent STT worker pre-warmed at startup (5-10s cold-load eliminated). VAD with adaptive endpoint detection. TTS synthesis runs concurrently with inference. Barge-in cancels in-flight generation via `JoinHandle::abort()` which closes the Ollama HTTP connection immediately.

**Action execution** — Shell, AppleScript, browser automation, file read/write, iMessage send. Destructive commands (`rm`, `kill`, `pkill`, `killall`) require explicit operator approval via HUD warning. Off-host detection prevents commands targeting other machines from executing locally.

**Memory and retrieval** — SQLite vector store with in-Rust cosine similarity. Async retrieval pipeline runs concurrently with generation, results injected into context.

**Proactive engine** — Rate-limited observation-triggered turns. Low-value and `[SILENT]`-tagged responses suppressed. User-active window (60s) gates proactive firing after each operator turn.

**Model routing** — Domain classifier + complexity scorer → tier selection. Every routing decision logs domain, complexity, and chosen model with rationale. Auditable, unit-tested.

---

## Project Structure

```
src/
  rust-core/          — Rust orchestrator (cargo workspace)
    src/
      orchestrator.rs       — Central state machine (~6000 lines)
      inference/
        router.rs           — Domain classifier, complexity scorer, tier selection
        engine.rs           — Ollama HTTP client, streaming, KV-cache
      action/
        engine.rs           — Action dispatch
        policy.rs           — Destructive command classification
        executor.rs         — Shell, AppleScript, file I/O execution
      voice/
        coordinator.rs      — TTS worker supervisor (daemon-lifetime)
        worker_client.rs    — Python worker subprocess management
        protocol.rs         — Binary IPC framing + handshake
      context_observer.rs   — App focus, clipboard, accessibility events
      proactive/
        engine.rs           — Rate-limit gates, prompt construction
      retrieval/
        pipeline.rs         — Vector store, async retrieval
      config.rs             — ~/.dexter/config.toml deserialization
      constants.rs          — All constants with reasoning comments
  swift/              — Swift UI (SwiftUI + AppKit + Metal)
    Sources/Dexter/
  python-workers/     — Voice and browser workers
    workers/
      tts_worker.py         — kokoro-82M synthesis
      stt_worker.py         — faster-whisper STT
      browser_worker.py     — Playwright automation
config/
  personality/
    default.yaml            — Model-facing personality and action rules
docs/                 — Phase specs and implementation history
src/shared/
  proto/dexter.proto  — IPC contract (single source of truth)
```

---

## Build

**Prerequisites**: Rust (edition 2021), Swift 6.2, Python 3.12+, Ollama, protoc

```bash
# Install Python worker dependencies
make setup-python

# Regenerate proto artifacts (after proto changes only)
make proto

# Run (starts release Rust core + Swift UI, waits for socket before launching Swift)
make run

# Tests (offline, no Ollama required)
cd src/rust-core && cargo test --bin dexter-core

# Release build
cd src/rust-core && cargo build --release
```

`make run` refuses to start if another Dexter core already owns `/tmp/dexter.sock`.
Stop the existing core/UI first so the Swift UI and `dexter-cli` talk to the same
freshly built daemon.

**Dev tool** — `dexter-cli` sends typed input to the running daemon without the Swift UI:

```bash
cd src/rust-core && cargo build --release --bin dexter-cli
./target/release/dexter-cli "what's 2 plus 2"
```

---

## Configuration

`~/.dexter/config.toml` — operator config (self-handle for iMessage, display preferences, etc.)  
`config/personality/default.yaml` — model-facing personality rules. Changes here affect model behavior directly; treat with the same scrutiny as code changes.

---

## Phase History

38 development phases, documented in `docs/PHASE_*_SPEC.md`. Highlights:

- **Phase 23**: Persistent STT worker, pre-warm at startup
- **Phase 24**: KV-cache prefill, VAD adaptive endpoint, STT fast path
- **Phase 27**: Generation barge-in via `JoinHandle::abort()`
- **Phase 33**: Cancellation completeness, shell path expansion
- **Phase 36**: Test sweep, terminal scrollback scrubbing, proactive spam gates
- **Phase 37**: Model stack modernization (gemma4:26b PRIMARY, router overhaul)
- **Phase 37.5**: Action safety (off-host detection, destructive command policy)
- **Phase 37.8**: PRIMARY keepalive, weather multi-city fast-path
- **Phase 38c**: Cross-session resource sharing (daemon-lifetime workers, no per-session warmup tax)

---

## Test Baseline

479 Rust tests. Run with:

```bash
cd src/rust-core && cargo test --bin dexter-core
```

A change that drops the count without documented reason is a regression.
