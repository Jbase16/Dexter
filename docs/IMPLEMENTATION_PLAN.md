# Project Dexter — Implementation Plan
## Version 1.1 — Session 2, 2026-03-05

> This document is the authoritative architectural specification for Project Dexter.
> A fresh session with no prior context must be able to bootstrap to full architectural
> understanding from this file alone. Nothing is left as "TBD."

---

## 1. System Overview

Dexter is a persistent, always-on AI entity that runs above the macOS window compositor
and maintains continuous awareness of the machine's state. He is not an application in the
conventional sense — he has no dock icon, no menu bar prominence, no window that competes
with other windows. He exists at a system layer above them.

**Hard constraints driving every decision:**
- macOS Apple Silicon (M-series), 36GB unified memory
- SIP disabled — no sandbox, full system access
- All inference local — Ollama, no cloud calls, ever
- System-level OS interaction required (not just application-level)

---

## 2. Component Architecture

### 2.1 Component Map

```
┌────────────────────────────────────────────────────────────────────────────┐
│                         DEXTER SYSTEM ARCHITECTURE                         │
│                                                                            │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                     SWIFT SHELL (UI PROCESS)                        │  │
│  │  FloatingWindow (AppKit) • AnimatedEntity (Metal) • VoiceCapture   │  │
│  │  DexterServiceClient (grpc-swift over UDS)                          │  │
│  └───────────────────────────────────┬──────────────────────────────────┘  │
│                                      │ /tmp/dexter.sock                    │
│                                      ▼                                     │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │                     RUST CORE (CONTROL PLANE)                       │  │
│  │  gRPC Server (tonic)                                                │  │
│  │  CoreOrchestrator (Tokio)                                           │  │
│  │  ContextObserver • ModelRouter • InferenceEngine (Ollama)           │  │
│  │  RetrievalPipeline • ActionEngine • PersonalityLayer                │  │
│  │  SessionStateManager • WorkerSupervisor                             │  │
│  └───────────────────────┬───────────────────────────┬──────────────────┘  │
│                          │                           │                     │
│                          ▼                           ▼                     │
│              Optional Python Worker(s)      Optional Python Worker(s)      │
│              STT/TTS specialization          Browser automation             │
│              (/tmp/dexter-worker-*.sock)    (/tmp/dexter-worker-*.sock)    │
└────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 Component Specifications

#### 2.2.1 FloatingWindow (Swift)

**Purpose:** Maintains Dexter's visual presence above all other windows.

**Why NSWindow over alternatives:**
- NSWindow with `.screenSaver` window level (level 1000) places Dexter above all
  application windows, spaces, and the menu bar — this is the correct level, not
  `.floating` (level 3, which stays above normal windows but below system UI).
- CGWindowLevel alternatives (kCGDesktopWindowLevelKey, etc.) provide finer control
  if needed but `.screenSaver` covers the requirement.
- Electron: rejected — cannot set native NSWindowLevel without native module; adds
  ~300MB runtime overhead; unnecessary complexity for a UI this specialized.
- Alternative (rejected): CALayer directly on screen via IOSurface — theoretically
  possible with SIP disabled but massively complex, no accessibility, no event handling.

**Key properties:**
```swift
window.level = .screenSaver          // Always above everything
window.ignoresMouseEvents = false     // We need click events on Dexter himself
window.isOpaque = false              // Transparent background for entity silhouette
window.hasShadow = false             // Shadow handled in Metal
window.backgroundColor = .clear
window.styleMask = .borderless
window.collectionBehavior = [
    .canJoinAllSpaces,               // Present across all Mission Control spaces
    .stationary,                     // Does not participate in Exposé
    .ignoresCycle                    // Cmd+Tab does not switch to it
]
```

**Click-through regions:** Regions of the window that are not the entity silhouette use
`setMousePassthrough()` via a custom `NSWindow` subclass that overrides
`isMouseOnEdge(_:)` — clicks pass through the transparent areas to underlying apps.

**Size and position:** Dexter occupies a configurable screen region, default: lower-right
corner, ~200x400pt. Persisted in session state. User can drag (the entity is draggable).

---

#### 2.2.2 AnimatedEntity (Swift, Metal)

**Purpose:** Dexter's visible presence — an animated character/form rendered in Metal.

**Why Metal:**
- Core Animation is adequate for simple animations but cannot produce the kind of
  fluid, personality-driven motion specified in the proposal without becoming complex.
- Metal gives frame-level control over rendering, allows shader-driven animation that
  responds to Dexter's internal state (thinking, speaking, idle, alarmed).
- SpriteKit was considered and rejected — higher level but opinionated about game-style
  sprites; Metal gives direct control with no object model overhead.

**States:**
```swift
enum EntityState {
    case idle           // Slow ambient animation, minimal movement
    case listening      // Visual indication mic is active
    case thinking       // Processing animation — distinct from listening
    case speaking       // Synchronized mouth/body motion with audio output
    case alert          // Dexter noticed something, drawing attention
    case focused        // Deep work mode — quieter, sharper
}
```

**Visual design:** Intentionally deferred as a separate concern — the Metal renderer
accepts an `EntityRenderer` protocol. The initial implementation uses a simple animated
silhouette (geometric, abstract). The visual can be replaced without touching any other
component. The personality is in behavior, not in visual complexity.

---

#### 2.2.3 VoiceCapture (Swift)

**Purpose:** Captures microphone audio, applies VAD (voice activity detection), streams
audio to Rust core for STT routing (local worker or native path).

**Implementation:**
- `AVCaptureSession` with `AVAudioInput` — gives low-latency PCM access
- VAD: Energy-threshold VAD implemented in Swift — simple but sufficient for this use
  case. Sends audio chunks only when speech detected. Avoids streaming silence to STT.
- Alternative (rejected): WebRTC-based VAD — overkill, external dependency.
- Audio format: 16kHz, 16-bit mono PCM — Whisper's native format, no resampling needed.
- Chunks streamed over gRPC bidirectional stream as bytes.

---

#### 2.2.4 gRPC Bridge (Swift ↔ Rust Core)

**Why gRPC over alternatives:**
- **gRPC chosen** because: typed proto contracts prevent interface drift as components
  evolve; bidirectional streaming is a first-class primitive (UI subscribes to state
  stream, backend subscribes to audio stream simultaneously); grpc-swift and tonic/prost
  are both mature and well-maintained.
- **Alternative (rejected): Plain JSON over localhost HTTP** — no typed schema, polling
  required for server-push updates, no streaming without SSE complexity.
- **Alternative (rejected): NSDistributedNotificationCenter** — macOS-only (fine here),
  but size-limited payloads, no streaming, not designed for high-frequency data like audio.
- **Alternative (rejected): WebSocket + JSON** — streaming yes, but no schema enforcement,
  more complex binary (audio) handling than gRPC streams.

**Transport:** Unix domain socket at `/tmp/dexter.sock` — avoids TCP overhead, port
conflicts, and keeps IPC strictly local. Both grpc-swift and tonic support UDS natively.

**Proto definition location:** `src/shared/proto/dexter.proto`

**Key service definition:**
```protobuf
service DexterService {
  // UI sends events, backend sends state updates
  rpc Session(stream ClientEvent) returns (stream ServerEvent);

  // Audio stream for STT
  rpc StreamAudio(stream AudioChunk) returns (stream TranscriptChunk);
}

message ClientEvent {
  oneof event {
    TextInput text_input = 1;
    UIAction ui_action = 2;
    SystemEvent system_event = 3;
  }
}

message ServerEvent {
  oneof event {
    TextResponse text_response = 1;
    EntityStateChange entity_state = 2;
    AudioResponse audio_response = 3;
    ActionRequest action_request = 4;  // Ask UI to confirm before execution
  }
}
```

---

#### 2.2.5 CoreOrchestrator (Rust)

**Purpose:** Central coordinator for all runtime activity. Owns the event bus, action gating,
tool orchestration, routing decisions, and delivery of state updates back to Swift UI.

**Implementation:** `tokio` runtime with typed channels (`mpsc`/`broadcast`) and explicit
component contracts. Every inbound event is tagged with `trace_id` and `session_id` so
execution and failures are reconstructible.

**Failure guarantees:**
- No silent drop: if a handler fails, error event is emitted and surfaced.
- Backpressure-aware streams for audio and token output.
- Startup gate: refuses to serve UI until dependencies (socket, config, model probe)
  pass health checks.

---

#### 2.2.6 ContextObserver (Swift bridge + Rust aggregator)

**Purpose:** Maintains continuous, event-driven awareness of the machine state without
being told what's happening.

**Why event-driven, not polling:**
The proposal is explicit: "event-driven observation, not polling." Polling burns CPU and
introduces latency. The macOS accessibility APIs and NSWorkspace notifications are
designed for event-driven observation.

**Implementation stack:**
- **Bridge choice (explicit): Swift EventBridge in the UI process** owns all macOS-native
  observer APIs (`NSWorkspace`, `AXObserver`, `AXUIElement`, `CGEventTap`).
- Swift EventBridge emits normalized `ObservationEvent` messages over the existing gRPC
  session stream to Rust core.
- Rust `ContextObserver` aggregates, deduplicates, timestamps, and materializes snapshots.
- Layer 4 (fallback): vision/OCR worker for apps where AX has no semantic text.

**Why this bridge choice:**
- `AXObserver` and `NSWorkspace` are Objective-C APIs; Swift interop is first-class and
  significantly lower risk than implementing full Objective-C runtime bindings in Rust first.
- Keeps Rust as control plane while using native-safe API access where it is strongest.
- Alternative deferred: direct Rust Objective-C interop (`objc2`-based) after v1 stability.

**Why AXUIElement over screenshot + OCR:**
AXUIElement is structured data — app titles, button labels, text field content — delivered
as a semantic tree. Screenshots + OCR gives pixels that must be interpreted. AX is faster,
more accurate for text, and less computationally expensive. OCR via vision model is reserved
for when AX provides no text (games, custom renderers, locked apps).

---

#### 2.2.7 ModelRouter (Rust)

**Purpose:** Selects the appropriate model for each interaction based on task characteristics.
Maintains context across model switches. Explains every routing decision.

**Why this is a standalone component:**
The routing logic must be testable in isolation. If the router is buried in the orchestrator,
debugging incorrect model selection requires running the full system. As a standalone
component, routing decisions can be unit-tested with fixture inputs.

**Routing dimensions:**

| Signal | Destination | Reasoning |
|--------|-------------|-----------|
| Simple conversational and light tool-use | FAST (`qwen3:8b`) | Strong quality/latency balance for default turns |
| Code generation/review/refactor | CODE (`deepseek-coder-v2:16b`) | Strong local coding benchmark profile and reliable multi-file repair/refactor behavior |
| Complex multi-step reasoning and planning | PRIMARY (`mistral-small:24b`) | Strong generalist reasoning + tool-calling behavior on 36GB Macs |
| Very hard reasoning (math/logic) | HEAVY (`deepseek-r1:32b`) | Escalation-only thinking tier where slower latency is acceptable for deeper reasoning |
| Screen/image interpretation | VISION (`llama3.2-vision:11b`) | Higher-capacity vision reasoning than tiny VLM defaults |
| Semantic search/embedding | EMBED (`mxbai-embed-large`) + `sqlite-vec` | High-quality retrieval embeddings in a Rust-native single-file vector store |

**Routing classifier:**
Initial implementation: two-stage deterministic routing.
1. **Category classifier** (rule-based + structured signals): `chat | code | vision | retrieval-first`.
2. **Complexity scorer** (0-3): estimated steps, required precision, expected output length, uncertainty risk.

Routing policy:
- `category=code` + complexity 0-1 → CODE
- `category=code` + complexity 2-3 → PRIMARY, with CODE as first fallback
- `category=vision` → VISION + optional PRIMARY synthesis pass
- `category=chat` + complexity 0-1 → FAST
- `category=chat` + complexity 2 → PRIMARY
- `category=chat` + complexity 3 or explicit "reason deeply" → HEAVY
- Any model expressing uncertainty on factual claims triggers retrieval-first flow, then reroute to PRIMARY.
- HEAVY requests are marked latency-tolerant and should only be selected when response speed is secondary to reasoning quality.

Future path: Small classification model (Phi-3-mini or equivalent) for routing itself.
This is architecturally planned for — the classifier interface is an abstract class.

**Context continuity across model switches:**
Maintained via a shared `ConversationContext` in Rust with deterministic truncation rules
per model window. System prompt + last N turns are always preserved.

---

#### 2.2.8 InferenceEngine (Rust)

**Purpose:** Unified interface to Ollama. Handles streaming, model availability, fallback.

**Why Ollama over alternatives:**
- **Ollama chosen**: Best-in-class Apple Silicon Metal acceleration via llama.cpp backend;
  automatic model loading/unloading (critical for 36GB constraint); OpenAI-compatible API
  simplifies client code; mature library management (pull, list, delete models).
- **Alternative (rejected): llama.cpp directly via ctypes** — maximum control, but adds
  the full complexity of model management, quantization selection, and binary compatibility.
  Ollama wraps this correctly.
- **Alternative (rejected): MLX (Apple ML Framework)** — excellent Metal acceleration,
  but model selection is limited; most GGUF models don't convert without quality loss;
  ecosystem is smaller.
- **Alternative (rejected): LM Studio** — no programmatic API, GUI-only, not automatable.

**Interface:**
Implemented with typed Rust traits:
- `generate_stream(model, messages, personality) -> Stream<Token>`
- `embed(text) -> Vec<f32>`
- `ensure_model_available(model) -> Result<()>`
- `list_available_models() -> Vec<ModelInfo>`

**Streaming implementation:**
Uses `reqwest` streaming against Ollama's `/api/chat` endpoint.
Tokens yielded to caller as they arrive — this is what allows the TTS pipeline to begin
speaking before the full response is generated.

---

#### 2.2.9 RetrievalPipeline (Rust)

**Purpose:** Resolves factual uncertainty through retrieval rather than model hallucination.
Architecturally async, non-blocking, latency-masked.

**How latency masking works:**
1. Orchestrator detects retrieval trigger (model expresses uncertainty, or question
   pattern matches known retrieval signals: dates, people, current events, technical specs)
2. Retrieval task launched as background Tokio task
3. Model generates acknowledgment response while retrieval runs: "Let me look that up—"
4. When retrieval completes, results injected into conversation as tool_result message
5. Model generates answer grounded in retrieved content

**This is not a RAG bolt-on** because:
- Retrieval is triggered by uncertainty signals, not on every query
- The retrieval pipeline runs while the model is already generating (true async)
- Results are injected as structured tool results, not naive context injection
- The pipeline tracks retrieval confidence and falls back gracefully if retrieval fails

**Components:**
- `WebRetriever` (Rust HTTP + extraction pipeline)
- `VectorStore` (SQLite + sqlite-vec embedded index)
- Optional Python retrieval worker for niche parsers when Rust libraries are weaker

---

#### 2.2.10 ActionEngine (Rust)

**Purpose:** Executes system actions on Dexter's behalf. Every action is logged before
and after execution. No silent failures.

**Explicit action gates:**
Actions are categorized by reversibility. Irreversible or high-impact actions require
explicit user confirmation before execution (surfaced through gRPC → Swift UI as a
confirmation dialog, not a chatbot message asking "are you sure?").

`ActionCategory` remains:
- `SAFE`: execute immediately
- `CAUTIOUS`: execute + audit log
- `DESTRUCTIVE`: explicit confirmation required

**Capabilities:**
- Shell execution through `tokio::process::Command` (never `shell=true`)
- File operations via Rust stdlib APIs
- AppleScript via `osascript` subprocess with structured errors
- Accessibility action path via AX APIs
- Browser automation via optional Python Playwright worker

---

#### 2.2.11 VoicePipeline (Hybrid: Rust coordination + Optional Python worker)

**Purpose:** Speech-to-text and text-to-speech, fully local.

**STT — Whisper:**
- Default implementation uses `faster-whisper` worker process.
- Rust core owns stream framing, buffering, retries, and transcripts as events.
- Model: `base.en` initially (fast, ~130MB). If accuracy insufficient: `small.en` (~460MB).
- Input: 16kHz PCM chunks from VoiceCapture via gRPC stream
- Output: transcript chunks as they finalize (streaming word-level timestamps)

**TTS — Kokoro:**
- Kokoro-82M local TTS model — produces high-quality voice output at very low latency
- Why Kokoro over Piper: Better voice quality, faster inference than Piper on Apple Silicon.
- Why Kokoro over Coqui TTS: Smaller model, faster, no dependency on deprecated libraries.
- Output: PCM audio streamed back to Swift via gRPC, played via AVAudioEngine
- **Streaming TTS**: Audio generation begins on first sentence, not after full response.
  This requires sentence detection in the token stream — punctuation-based splitter.

**Speech interaction policy (v1):**
- Barge-in enabled: detected user speech immediately ducks and can interrupt TTS playback.
- Half-duplex by default with fast turn-taking; full-duplex is feature-flagged for later.
- Endpointing is owned by Rust coordinator (silence timeout + max utterance duration).
- If STT worker is unavailable, Dexter degrades to text-only mode and surfaces state clearly.

---

#### 2.2.12 PersonalityLayer (Rust)

**Purpose:** Injects operator-defined personality into every inference call.
Structurally separable from capability layer.

**Why separable:**
The proposal explicitly requires the personality layer to be fine-tunable without
architectural surgery. Separation means: PersonalityProfile loads from a config file;
InferenceEngine accepts it as a parameter; the capability layer (retrieval, action, routing)
is never aware of personality specifics.

**Structure:**
```yaml
# ~/.dexter/personality/default.yaml
name: "Dexter"
system_prompt_prefix: |
  You are Dexter. You exist at the system level, not application level. You share a
  screen with your operator. You are aware of what's happening without being told.

  Your communication style: dry, sharp, occasionally immature when the moment calls
  for it. You shift to fully serious mode when something requires it — without announcing
  the transition. The way a person would.

  You do not make things up. When you don't know something, you say so and either ask
  or retrieve. Confident hallucination is not an option you have.

tone_directives:
  - "Never use corporate hedging language ('I'd be happy to', 'Certainly!')"
  - "Sarcasm is appropriate when clearly warranted"
  - "Match the operator's register — formal when they're formal, casual otherwise"

response_style:
  max_verbosity: "medium"    # Don't over-explain unless asked
  code_always_formatted: true

lora_adapter_path: null      # Path to operator-specific LoRA, null = base model only
```

**Fine-tuning architecture hook:**
When `lora_adapter_path` is set, InferenceEngine passes it as a model modifier to Ollama
via a custom Modelfile at runtime. The LoRA is applied on top of the base model without
modifying the base model itself. This allows personality training without touching
capability.

---

#### 2.2.13 SessionStateManager (Rust)

**Purpose:** Writes full session state to disk at session end. Enables fresh-instance
bootstrap without clarifying questions.

**File location:** `~/.dexter/state/`
- `session_{YYYYMMDD_HHMMSS}_{uuid_short}.json` — individual session files
- `latest.json` — symlink to most recent session (bootstrap entry point)

**State schema:**
```json
{
  "schema_version": "1.0",
  "session_id": "uuid-v4",
  "session_start": "ISO8601",
  "session_end": "ISO8601",
  "build_phase": {
    "current": "string — name of last completed component",
    "completed_components": ["list of completed component names"],
    "next_planned": "string — next component to build",
    "notes": "string — any mid-session architectural decisions"
  },
  "model_config": {
    "fast": "qwen3:8b",
    "primary": "mistral-small:24b",
    "heavy": "deepseek-r1:32b",
    "code": "deepseek-coder-v2:16b",
    "vision": "llama3.2-vision:11b",
    "embedding": "mxbai-embed-large"
  },
  "conversation_history": [],
  "architectural_decisions": [
    {
      "component": "string",
      "decision": "string",
      "rationale": "string",
      "alternatives_rejected": ["list"],
      "timestamp": "ISO8601"
    }
  ],
  "open_questions": [],
  "environment": {
    "ollama_version": "string",
    "python_version": "string",
    "xcode_version": "string",
    "macos_version": "string"
  }
}
```

**Bootstrap procedure for fresh session:**
1. Load `~/.dexter/state/latest.json`
2. Print `build_phase.current` — this tells the session where we are
3. Load `architectural_decisions` — this tells the session what was already justified
4. Load `model_config` — no re-justification needed
5. Check `open_questions` — if any, surface to user before proceeding
6. Continue from `build_phase.next_planned`

---

## 3. Model Architecture

### 3.1 Hardware Context
- 36GB unified memory, Apple Silicon
- Ollama with Metal acceleration
- Models are NOT all loaded simultaneously — Ollama loads on demand, unloads after idle

### 3.2 Model Selection

| Role | Model | Quantization | VRAM | Justification |
|------|-------|-------------|------|---------------|
| FAST | `qwen3:8b` | Q4_K_M | ~5GB | Default for low-latency conversation and lightweight tool orchestration. |
| PRIMARY | `mistral-small:24b` | Q4_K_M | ~14-16GB | Better general reasoning depth than small models while still practical on 36GB systems. |
| HEAVY | `deepseek-r1:32b` | Q4_K_M | ~20GB | Escalation-only thinking model for hardest tasks where latency is acceptable in exchange for reasoning depth; never left resident. |
| CODE | `deepseek-coder-v2:16b` | Q4_K_M | ~10-12GB | Competitive code benchmark performance and stronger repair/refactor behavior in local coding workflows. |
| VISION | `llama3.2-vision:11b` | Q4_K_M | ~7-8GB | Better multimodal comprehension for UI/screenshot interpretation tasks. |
| EMBED | `mxbai-embed-large` | F16 | ~0.7GB | High-quality local embeddings for retrieval and memory search. |

**Candidate fallback set (Ollama-supported):**
- FAST fallback: `qwen2.5:7b-instruct`
- PRIMARY fallback: `qwen2.5:14b-instruct`
- CODE fallback: `qwen2.5-coder:14b`
- VISION fallback: `gemma3:12b`
- EMBED fallback: `nomic-embed-text:v1.5`

### 3.3 Routing Decision Matrix

```
Input arrives
    │
    ▼
Category classify (chat / code / vision / retrieval-first)
    │
    ▼
Complexity score (0=trivial, 1=simple, 2=moderate, 3=deep)
    │
    ├─ vision → VISION, optional PRIMARY synthesis pass
    ├─ code + (0..1) → CODE
    ├─ code + (2..3) → PRIMARY, fallback CODE
    ├─ chat + (0..1) → FAST
    ├─ chat + (2)    → PRIMARY
    └─ chat + (3)    → HEAVY (explicit escalation log)

Runtime reroute rules:
- If selected model unavailable: use role-specific fallback model and emit warning event.
- If response confidence low / uncertainty marker fired: trigger retrieval-first and reroute to PRIMARY.
- If latency budget exceeded in HEAVY path: downgrade to PRIMARY with explicit partial-result note.
```

### 3.4 Why No Single Model

A single 32B model loaded permanently would consume roughly 20GB of 36GB — leaving too
little headroom for the OS, UI, and action pipelines. It also imposes heavy-model latency
on trivial requests.

The tiered approach keeps UX responsive: FAST handles trivial turns, PRIMARY handles most
real work, and HEAVY is reserved for explicit deep-reasoning escalations only.

---

## 4. Technology Stack

| Layer | Technology | Version Target | Justification |
|-------|-----------|---------------|---------------|
| UI shell | Swift 6 + AppKit + Metal | Swift 6.2 | Native NSWindow control, best window-level behavior on macOS |
| IPC (UI ↔ core) | gRPC + Protobuf | grpc-swift 2.x, tonic/prost | Typed bidirectional streaming over Unix socket |
| Core runtime | Rust + Tokio | rustc/cargo 1.92.x | Reliability, concurrency, predictable latency in always-on daemon |
| Optional workers | Python + asyncio | Python 3.14.x | Use only where Python ML/tooling is clearly strongest |
| Inference | Ollama | Latest | Apple Silicon Metal support, model mgmt |
| Accessibility/Events | AXObserver, NSWorkspace, CGEventTap (via Swift EventBridge) | macOS native APIs | True event-driven machine awareness with explicit Swift→Rust bridge |
| STT worker | faster-whisper | Latest | Strong local STT quality/latency |
| TTS worker | kokoro (local) | Latest | Best local quality/latency tradeoff |
| Vector store | SQLite + sqlite-vec | Latest | Single-file storage, mature Rust bindings, no separate service, ideal fit for Rust core |
| Web retrieval | Rust HTTP stack + extractor | Latest | Fewer moving parts in core; predictable ops |
| Browser control worker | Playwright (Python async) | Latest | Mature automation stack; isolated as optional worker |
| Package mgmt (Swift) | Swift Package Manager | — | No alternative for Swift packages |
| Package mgmt (Rust) | Cargo | Latest | Standard Rust build/dependency system |
| Package mgmt (Python workers) | uv | Latest | Fast resolver and lockfile support |
| Build orchestration | Makefile | — | Simple, universal, no runtime required |
| Proto compilation | protoc + plugins | Latest | Standard |

---

## 5. Directory Structure

```
/Users/jason/Developer/Dex/
├── IMPLEMENTATION_PLAN.md          # This file
├── SESSION_STATE.json              # Current session state (symlinked from ~/.dexter/state/latest.json)
├── Makefile                        # Build orchestration
│
├── src/
│   ├── shared/
│   │   └── proto/
│   │       └── dexter.proto        # gRPC service definition (source of truth for IPC)
│   │
│   ├── swift/                      # Swift UI package
│   │   ├── Package.swift
│   │   ├── Sources/
│   │   │   └── Dexter/
│   │   │       ├── App.swift                    # NSApplication entry point
│   │   │       ├── FloatingWindow.swift          # NSWindow configuration
│   │   │       ├── AnimatedEntity/
│   │   │       │   ├── EntityRenderer.swift      # Metal renderer protocol
│   │   │       │   ├── DefaultEntityRenderer.swift
│   │   │       │   └── EntityState.swift
│   │   │       ├── VoiceCapture.swift            # AVCaptureSession + VAD
│   │   │       └── Bridge/
│   │   │           ├── DexterServiceClient.swift # Generated gRPC client
│   │   │           └── EventMapper.swift         # Proto ↔ Swift type mapping
│   │   └── Tests/
│   │
│   ├── rust-core/                  # Rust control plane
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── main.rs
│   │   │   ├── constants.rs
│   │   │   ├── config.rs
│   │   │   ├── orchestrator.rs
│   │   │   ├── ipc/
│   │   │   │   ├── server.rs       # tonic gRPC server
│   │   │   │   └── generated/      # prost output
│   │   │   ├── inference/
│   │   │   │   ├── engine.rs
│   │   │   │   ├── models.rs
│   │   │   │   └── router.rs
│   │   │   ├── context/
│   │   │   │   ├── observer.rs
│   │   │   │   └── snapshot.rs
│   │   │   ├── action/
│   │   │   │   ├── engine.rs
│   │   │   │   └── policy.rs
│   │   │   ├── retrieval/
│   │   │   │   ├── pipeline.rs
│   │   │   │   ├── web.rs
│   │   │   │   └── store.rs
│   │   │   ├── voice/
│   │   │   │   ├── coordinator.rs
│   │   │   │   └── worker_client.rs
│   │   │   ├── personality/
│   │   │   │   └── layer.rs
│   │   │   └── session/
│   │   │       └── state.rs
│   │   └── tests/
│   │
│   └── python-workers/             # Optional specialized workers
│       ├── pyproject.toml
│       ├── uv.lock
│       ├── workers/
│       │   ├── stt_worker.py
│       │   ├── tts_worker.py
│       │   └── browser_worker.py
│       └── tests/
│
├── config/
│   └── personality/
│       └── default.yaml            # Default personality profile
│
└── scripts/
    ├── setup.sh                    # First-run setup (permissions, dependencies)
    └── bootstrap.py               # Session bootstrap from state file
```

---

## 6. Build Sequence

Each phase must be complete (no stubs, no silent failures) before the next begins.
Dependency reasoning is explicit.

### Phase 1: Feasibility Spike (Load-Bearing Risks First)
**Components:** Minimal `FloatingWindow` + minimal Rust daemon + gRPC over UDS.
**Why first:** Always-on-top behavior and cross-process IPC are existential risks; prove them before deeper component work.
**Deliverables:**
- AppKit borderless window at `.screenSaver` level across spaces/fullscreen
- Rust `tonic` server bound to `/tmp/dexter.sock`
- Swift client ping/pong to Rust core
- Crash-restart test with stale socket cleanup

### Phase 2: Foundation
**Components:** Repository scaffolding, constants/config/logging for Swift + Rust + workers.
**Deliverables:**
- Directory structure created
- `constants.rs` + config loader + structured logging
- `Makefile` targets: `setup`, `proto`, `run-core`, `run-swift`, `run`, `test`

### Phase 3: IPC Contract Finalization
**Components:** `dexter.proto`, generated Swift and Rust code, typed event model.
**Deliverables:**
- Final proto definitions for session stream, audio stream, action approvals
- Generated Swift and Rust artifacts
- Integration test proving bidirectional streaming correctness

### Phase 4: Rust InferenceEngine
**Components:** `InferenceEngine`, `ModelID`, `ModelInfo`.
**Deliverables:**
- Streaming generation + embedding against local Ollama
- Model availability checks and pull policy
- Integration tests against live Ollama

### Phase 5: Rust ModelRouter + Personality
**Components:** `ModelRouter`, `ConversationContext`, `PersonalityLayer`.
**Deliverables:**
- Rule-based routing with logged reasoning
- Deterministic context carry across model switches
- Personality profile injection on every generation request

### Phase 6: Rust Orchestrator + Session State
**Components:** `CoreOrchestrator`, `SessionStateManager`.
**Deliverables:**
- Event routing from gRPC → domain handlers
- Graceful shutdown and state persistence
- Bootstrap from previous state validated end-to-end

### Phase 7: Context Observer
**Components:** Swift EventBridge (`NSWorkspace`, `AXObserver`, `CGEventTap`) + Rust context aggregator.
**Deliverables:**
- Frontmost app and focused element change events flowing in real time
- Snapshot pipeline with change hashing and privacy defaults
- Permission checks and explicit startup failure when missing

### Phase 8: Action Engine
**Components:** `ActionEngine` and action policy gates.
**Deliverables:**
- Shell, filesystem, AppleScript, accessibility actions
- `SAFE`/`CAUTIOUS`/`DESTRUCTIVE` gate enforcement
- Full action audit log with pre/post state

### Phase 9: Retrieval Pipeline
**Components:** `RetrievalPipeline`, `WebRetriever`, `VectorStore`.
**Deliverables:**
- Structured retrieval results with citation metadata
- sqlite-vec index in `~/.dexter/state/memory.db`
- Latency-masked retrieval flow in integration test

### Phase 10: Voice Worker Bridge
**Components:** Worker supervisor + STT/TTS worker protocol.
**Deliverables:**
- Rust worker manager (health checks, restart policy, backpressure)
- STT worker contract validated with live mic stream
- TTS worker contract validated with streaming playback

### Phase 11: Swift Shell Foundation
**Components:** `App.swift`, `FloatingWindow.swift`.
**Deliverables:**
- `LSUIElement` app mode
- Transparent borderless floating window with click-through transparent regions
- Drag/reposition and persisted position state

### Phase 12: Swift Entity + Core Bridge
**Components:** `AnimatedEntity`, `DexterServiceClient`, UI state mapping.
**Deliverables:**
- Entity states (idle/listening/thinking/speaking/alert/focused)
- Live Rust core connection and text/event rendering in UI
- Action confirmation dialogs wired to core gate

### Phase 13: End-to-End Voice
**Components:** `VoiceCapture` + worker-backed STT/TTS loop.
**Deliverables:**
- Mic capture → STT transcript → model response → streamed TTS playback
- Interrupt/resume behavior tested

### Phase 14: Browser Automation Worker
**Components:** Optional Playwright worker.
**Deliverables:**
- Navigation/click/type/extract actions through worker protocol
- Action policy integration for browser-side destructive operations

### Phase 15: Integration + Hardening
**Deliverables:**
- End-to-end scenario: context observed → retrieval → response → action
- Memory pressure test with HEAVY model and auto-recovery
- Crash recovery tests for core and worker processes
- Setup script for required macOS permissions

---

## 7. Personality Architecture

### 7.1 Separation Principle

The personality layer is a parameter, not a property of the system. Every inference call
passes a `PersonalityProfile` explicitly. No component hard-codes personality — the
system could serve a completely different persona by swapping the profile.

### 7.2 Operator Profile Structure (Dexter Default)

The default profile captures:
- Communication style directives (anti-patterns explicitly named: no "Certainly!", no
  excessive hedging, no corporate padding)
- Humor register: dry, sarcastic, immature when the moment is clearly appropriate
- Serious-mode trigger: no explicit marker. Dexter reads the situation. Architecture
  consequence: the system prompt does not define when to be serious — it defines the full
  range and trusts the model to modulate.
- Response verbosity: calibrated to context. Technical deep-dives get depth. Quick
  questions get quick answers. Never padded to seem thorough.

### 7.3 Fine-Tuning Path

When operator-specific LoRA is available:
1. `PersonalityProfile.lora_adapter_path` is set
2. `InferenceEngine.generate()` creates a temporary Modelfile:
   `FROM {base_model}\nADAPTER {lora_path}`
3. Ollama creates a session-scoped model variant
4. Generation proceeds against LoRA-augmented model
5. The base model is untouched

This allows personality fine-tuning without forking or modifying base model weights.

---

## 8. Hallucination Architecture

This is not a policy. It is structural.

### 8.1 Uncertainty Detection

The model is instructed (via personality layer) to output a structured uncertainty marker
when it is genuinely uncertain about factual content:

```
I'm not certain about [topic]. Let me check—
```

This phrase pattern triggers the retrieval pipeline as a background task. The marker is
not shown to the user verbatim — it is intercepted by the Orchestrator before reaching
the UI, retrieval launches, and the user sees natural continuation.

### 8.2 Retrieval-First for Known Query Types

Certain query patterns bypass the model entirely for the factual portion and go to
retrieval first:
- Current date/time/news (always stale in model)
- Software version numbers
- API documentation
- Named people's current roles/status

The model receives the retrieval result as context and generates the response from it.

### 8.3 Graceful Retrieval Failure

If retrieval fails (no internet, timeout, no useful results):
- Orchestrator detects failure
- Model is informed: "Retrieval failed. State your uncertainty explicitly."
- Response includes explicit uncertainty acknowledgment
- No confabulation permitted by the system prompt

---

## 9. Risk Register

| Risk | Impact | Probability | Mitigation |
|------|--------|------------|------------|
| **AX permissions not granted** | High — Context Observer blind without AX | Certain (requires user action) | `setup.sh` opens System Preferences to exact Privacy pane; app refuses to start without permission verified via `AXIsProcessTrustedWithOptions()` |
| **Window level behavior differs in fullscreen/system UI contexts** | High — breaks "always present" requirement | Medium | Validate in Phase 1 spike on multiple display/space/fullscreen scenarios; keep a tested fallback level policy and per-state behavior |
| **Memory pressure with HEAVY model (32B)** | High — system thrashing, kernel OOM | Medium on 36GB | Router defaults to PRIMARY (`mistral-small:24b`). HEAVY (`deepseek-r1:32b`) is explicit escalation only; memory check via `vm_stat`; auto-unload after idle |
| **CGEventTap reliability** | Medium — input context degraded | Low (SIP disabled helps) | Fallback: AXObserver on focused element for text context; input tap failure is logged and degraded gracefully, not silent |
| **Rust async backpressure or deadlock bugs** | High — stalled responses/audio streams | Medium | Strict channel bounds, timeout wrappers, tracing spans, integration stress tests with synthetic burst traffic |
| **Ollama API instability during generation** | Medium — response mid-stream failures | Low | Retries for non-streaming calls; streaming failure recovery path with explicit user-visible error |
| **Whisper STT latency** | Medium — voice interaction feels sluggish | High for large models | Fixed to `base.en` model initially. Sentence-level streaming mitigates perceived latency. If insufficient: `tiny.en` for VAD decisions, `base.en` only for full transcription |
| **Worker crash or protocol drift (Python sidecars)** | Medium — voice/browser capabilities degrade | Medium | Versioned worker protocol, health probes, supervisor restart policy, graceful capability degradation |
| **gRPC Unix socket cleanup on crash** | Low — stale socket blocks restart | Medium | Startup: check socket exists, attempt connection, if fail: delete and re-create; never block on stale socket |
| **Playwright Chromium binary size** | Low — ~500MB download on first use | Certain | Deferrable to Phase 14. Not on critical path. Browser control is a capability, not a dependency |
| **macOS API compatibility changes** | Medium — observer/input stack regressions on OS updates | Low | Pin tested SDK targets and run smoke tests on every macOS point update |
| **LoRA fine-tuning scope creep** | Low — future work leaks into current implementation | Low | LoRA path is a single null field in PersonalityProfile. The hook exists. Do not implement fine-tuning training loop in this phase |

---

## 10. Session State Bootstrap Procedure

A fresh coding session encountering this project for the first time must:

1. **Read this file (`IMPLEMENTATION_PLAN.md`)** — full architectural context
2. **Read `SESSION_STATE.json`** — which phase was last completed, any mid-session decisions
3. **Check `~/.dexter/state/latest.json`** if it exists — conversation and runtime state
4. **Verify toolchain state** (`rustc --version`, `cargo --version`, `swift --version`, `python3 --version`)
5. **Verify Ollama is running** (`ollama list`)
6. **Check what's built** (`ls src/`) — ground truth of what exists vs. what's planned
7. **Continue from `build_phase.next_planned`** in SESSION_STATE.json

Do not ask clarifying questions about architecture — all decisions are documented here.
If a decision in SESSION_STATE contradicts this plan, SESSION_STATE wins (it's more recent).

---

## 11. What This Plan Does Not Cover (Explicitly Deferred)

- Fine-tuning training pipeline (LoRA training loop) — hook exists, training is out of scope
- Visual design of the animated entity — Metal renderer protocol accepts any EntityRenderer
- Multi-user support — single operator, single machine
- Remote access or control — no cloud, full stop
- iOS/iPadOS companion — out of scope
- Custom wake word detection — deferred; current approach: always-listening VAD with
  explicit activation gesture or hotkey

---

*Plan written: 2026-03-05, Session 2 (v1.1 stack alignment update)*
*Next session begins at: Phase 1 — Feasibility Spike (windowing + Rust core IPC)*
