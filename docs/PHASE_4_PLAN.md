# Phase 4 — Rust InferenceEngine

## Goal

Build the `InferenceEngine` — a typed Rust struct that owns all communication with
the Ollama HTTP API. Phase 4 provides the streaming generation primitive, embedding,
model lifecycle management (list, pull, unload), and startup health check. It defines
the API surface that Phase 5 (ModelRouter + PersonalityLayer) and Phase 6 (Orchestrator)
will consume.

No routing logic, no personality, no conversation context, no orchestrator wiring.
Those are Phase 5 and Phase 6.

---

## Where We Are

Phase 3 complete: bidirectional gRPC session over UDS is proven. The session handler
in `server.rs` currently has a reader task that logs incoming `ClientEvent`s and echoes
`TextResponse("session ready")` on CONNECTED. It is a stub — no inference involved.

Phase 4 produces the component that Phase 6 will wire into that session handler.

---

## Scope

### In Phase 4:
- `InferenceEngine` struct: `reqwest::Client` over Ollama's HTTP API
- `generate_stream()` — POST `/api/chat`, streaming NDJSON → `Stream<Item = Result<TokenChunk, InferenceError>>`
- `embed()` — POST `/api/embed` → `Vec<f32>`
- `list_available_models()` — GET `/api/tags` → `Vec<ModelInfo>`
- `ensure_model_available()` — checks `/api/tags` (downloaded ≠ loaded; "is the model on disk?")
- `unload_model()` — POST `/api/chat` with `keep_alive: 0`
- `pull_model()` — POST `/api/pull` with streaming progress (explicit opt-in only)
- `InferenceConfig` TOML section in `DexterConfig`
- `InferenceError` enum with `From<InferenceError> for tonic::Status` conversion
- `ModelId` enum (Fast/Primary/Heavy/Code/Vision/Embed) and `ModelInfo` value type
- `GenerationRequest`/`GenerationResponse`/`TokenChunk`/`EmbeddingRequest` typed API surface
- Startup health check: `list_available_models()` at startup → logs `ollama_reachable=true/false`; degraded-mode start (not fatal) if Ollama is down
- Integration tests against live Ollama — gated behind `#[ignore]` so `make test` stays offline-clean
- `make test-inference` Makefile target

### NOT in Phase 4:
- Model routing logic (Phase 5 `ModelRouter`)
- Personality system prompt injection (Phase 5 `PersonalityLayer`)
- Conversation context / history management (Phase 5/6)
- Wiring inference into the gRPC session handler (Phase 6 Orchestrator)
- STT/TTS Python workers (Phase 10)
- Any proto changes
- Any Swift changes

---

## 1. New Files

### `src/rust-core/src/inference/mod.rs`

Module root. Declares `engine`, `models`, `error` submodules and re-exports the
public API surface consumed by Phase 5/6:

```rust
pub use engine::{InferenceEngine, GenerationRequest, TokenChunk, EmbeddingRequest};
pub use models::{ModelId, ModelInfo};
pub use error::InferenceError;
```

### `src/rust-core/src/inference/error.rs`

`InferenceError` enum with structured variants. Implements `std::error::Error`,
`Display`, and `From<InferenceError> for tonic::Status` (Phase 6 needs this to
propagate inference errors as gRPC status codes).

```rust
pub enum InferenceError {
    OllamaUnavailable { url: String, source: String },
    ModelNotFound(String),
    StreamInterrupted(String),
    RequestTimeout,
    ApiError { status: u16, message: String },
    SerializationError(String),
}
```

Status code mapping:
- `OllamaUnavailable` → `Status::unavailable`
- `ModelNotFound` → `Status::not_found`
- `StreamInterrupted` → `Status::aborted`
- `RequestTimeout` → `Status::deadline_exceeded`
- `ApiError` → `Status::internal`
- `SerializationError` → `Status::internal`

### `src/rust-core/src/inference/models.rs`

`ModelId` enum (six variants matching the config tiers):

```rust
pub enum ModelId { Fast, Primary, Heavy, Code, Vision, Embed }
```

`ModelInfo` value type (from Ollama `/api/tags` response):

```rust
pub struct ModelInfo {
    pub name:           String,       // "deepseek-coder-v2:16b"
    pub size_bytes:     u64,
    pub digest:         String,
    pub parameter_size: String,       // "16B"
    pub quantization:   String,       // "Q4_K_M"
    pub families:       Vec<String>,  // ["deepseek2"]
}
```

`ModelId::ollama_name(config: &ModelConfig) -> &str` — resolves a `ModelId` to the
configured model name string. Used by `InferenceEngine` to look up the actual Ollama
model name for a given tier.

### `src/rust-core/src/inference/engine.rs`

The core of Phase 4. `InferenceEngine` struct and its full implementation.

**Public types:**

```rust
pub struct InferenceEngine { /* private reqwest::Client + InferenceConfig */ }

pub struct GenerationRequest {
    pub model_name:   String,
    pub messages:     Vec<Message>,
    pub temperature:  Option<f32>,  // None = Ollama default
}

pub struct Message {
    pub role:    String,  // "system" | "user" | "assistant"
    pub content: String,
}

pub struct TokenChunk {
    pub content:          String,
    pub is_final:         bool,
    pub done_reason:      Option<String>,  // "stop" | "length" | "content_filter" on final
    pub prompt_eval_count: Option<u32>,    // populated on final chunk only
    pub eval_count:        Option<u32>,    // populated on final chunk only
}

pub struct EmbeddingRequest {
    pub model_name: String,
    pub input:      String,
}
```

**Private Ollama API structs** (not exported, `serde` derived):

```rust
// POST /api/chat request body
struct OllamaChatRequest {
    model:      String,
    messages:   Vec<OllamaMessage>,
    stream:     bool,
    keep_alive: Option<i64>,  // -1 = keep forever, 0 = unload immediately
    options:    Option<OllamaOptions>,
}

struct OllamaMessage { role: String, content: String }

struct OllamaOptions { temperature: Option<f32> }

// One line of the streaming response
struct OllamaChatChunk {
    model:             String,
    message:           Option<OllamaMessage>,  // None on final chunk — must be Option
    done:              bool,
    done_reason:       Option<String>,
    prompt_eval_count: Option<u32>,
    eval_count:        Option<u32>,
}

// POST /api/embed request and response
struct OllamaEmbedRequest { model: String, input: String }
struct OllamaEmbedResponse { embeddings: Vec<Vec<f32>> }  // outer vec always length 1

// GET /api/tags response
struct OllamaTagsResponse { models: Vec<OllamaModelEntry> }
struct OllamaModelEntry {
    name:    String,
    size:    u64,
    digest:  String,
    details: OllamaModelDetails,
}
struct OllamaModelDetails {
    parameter_size:    Option<String>,
    quantization_level: Option<String>,
    families:          Option<Vec<String>>,
}
```

**Public methods:**

```rust
impl InferenceEngine {
    pub fn new(config: &InferenceConfig) -> Self;

    /// GET /api/tags — returns all downloaded models.
    pub async fn list_available_models(&self) -> Result<Vec<ModelInfo>, InferenceError>;

    /// Returns true if the named model is downloaded (appears in /api/tags).
    /// Distinguishes "not found" (Ok(false)) from "Ollama unreachable" (Err).
    pub async fn ensure_model_available(&self, model_name: &str)
        -> Result<bool, InferenceError>;

    /// POST /api/chat with stream:true.
    /// Returns connection errors eagerly (outer Result); mid-stream errors
    /// surface as Err items in the inner Stream.
    pub async fn generate_stream(&self, request: GenerationRequest)
        -> Result<
            Pin<Box<dyn Stream<Item = Result<TokenChunk, InferenceError>> + Send>>,
            InferenceError,
        >;

    /// POST /api/embed — returns embedding vector for the input.
    pub async fn embed(&self, request: EmbeddingRequest)
        -> Result<Vec<f32>, InferenceError>;

    /// Evicts a model from VRAM by sending keep_alive=0.
    /// Required for HEAVY/VISION mutual exclusion (see model_warming_strategy).
    pub async fn unload_model(&self, model_name: &str) -> Result<(), InferenceError>;

    /// POST /api/pull — downloads a model with streaming progress.
    /// Only called when InferenceConfig.auto_pull_missing_models = true.
    pub async fn pull_model(&self, model_name: &str) -> Result<(), InferenceError>;
}
```

**`generate_stream` internals — NDJSON line accumulator:**

Ollama streams newline-delimited JSON. Each HTTP chunk from `bytes_stream()` can
contain zero, one, or multiple complete JSON lines, or a partial line. The accumulator:

```
let mut buf = String::new();
while let Some(chunk) = byte_stream.next().await {
    buf.push_str(&String::from_utf8_lossy(&chunk?));
    while let Some(newline_pos) = buf.find('\n') {
        let line = buf.drain(..=newline_pos).collect::<String>();
        let line = line.trim();
        if line.is_empty() { continue; }
        let parsed = serde_json::from_str::<OllamaChatChunk>(line)?;
        yield TokenChunk { ... };
    }
}
```

This pattern handles all chunk boundary cases correctly.

---

## 2. Existing Files Modified

### `src/rust-core/constants.rs`

Add Ollama URL/path constants:

```rust
pub const OLLAMA_BASE_URL:              &str = "http://localhost:11434";
pub const OLLAMA_CHAT_PATH:             &str = "/api/chat";
pub const OLLAMA_EMBED_PATH:            &str = "/api/embed";
pub const OLLAMA_TAGS_PATH:             &str = "/api/tags";
pub const OLLAMA_PULL_PATH:             &str = "/api/pull";
pub const INFERENCE_CHANNEL_CAPACITY:   usize = 32;
```

### `src/rust-core/src/config.rs`

Add `InferenceConfig` struct and a `[inference]` TOML section in `DexterConfig`.

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct InferenceConfig {
    #[serde(default = "default_ollama_base_url")]
    pub ollama_base_url: String,

    /// Timeout for non-streaming requests (embed, list, pull, unload).
    /// Applied per-request via RequestBuilder::timeout, NOT via ClientBuilder.
    /// Not applied to generate_stream — streaming uses inactivity_timeout_secs instead.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,

    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,

    /// Maximum seconds of silence before a streaming generation is aborted.
    /// Resets on every received byte chunk — so a model generating tokens every 500ms
    /// for 10 minutes will never fire this. Only fires when Ollama stops sending entirely.
    /// This is the correct timeout primitive for streaming; total-request timeout is not.
    #[serde(default = "default_stream_inactivity_timeout_secs")]
    pub stream_inactivity_timeout_secs: u64,

    /// If true, pull missing models automatically on first use.
    /// Default false: silently pulling 5–20GB on startup is wrong behavior.
    #[serde(default)]
    pub auto_pull_missing_models: bool,
}

fn default_ollama_base_url()                  -> String { OLLAMA_BASE_URL.to_string() }
fn default_request_timeout_secs()             -> u64    { 30  }
fn default_connect_timeout_secs()             -> u64    { 5   }
fn default_stream_inactivity_timeout_secs()   -> u64    { 30  }
```

Add to `DexterConfig`:
```rust
#[serde(default)]
pub inference: InferenceConfig,
```

Remove the `#[allow(dead_code)]` on `ModelConfig` — it is now consumed by Phase 4.

Add unit tests:
- `inference_default_url_matches_constant`: `InferenceConfig::default().ollama_base_url == OLLAMA_BASE_URL`
- `inference_default_timeouts_are_sane`: `request_timeout_secs == 30`, `connect_timeout_secs == 5`, `stream_inactivity_timeout_secs == 30`
- `inference_auto_pull_defaults_false`

### `src/rust-core/Cargo.toml`

Three new production dependencies:

```toml
reqwest    = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls"] }
futures-util = "0.3"
serde_json = "1"
```

See Section 3 for rationale.

### `src/rust-core/src/main.rs`

Add `mod inference;` declaration and startup health check:

```rust
// After ensure_state_dir and before ipc::serve:
let engine = inference::InferenceEngine::new(&cfg.inference);
match engine.list_available_models().await {
    Ok(models) => info!(
        ollama_reachable = true,
        model_count = models.len(),
        "Ollama health check passed"
    ),
    Err(e) => warn!(
        ollama_reachable = false,
        error = %e,
        "Ollama unreachable at startup — inference will fail until Ollama is available"
    ),
}
// engine dropped here intentionally — Phase 6 constructs it inside the orchestrator
```

The engine is constructed and health-checked, then dropped. Not fatal on Ollama
unavailability — Dexter starts and waits; inference calls will fail until Ollama comes up.
Phase 6 constructs the engine inside the orchestrator where it lives for the session lifetime.

### `Makefile`

Add `make test-inference` target:

```makefile
## test-inference: run inference integration tests (requires live Ollama)
test-inference:
	cd $(RUST_CORE_DIR) && cargo test -- --include-ignored
```

---

## 3. New Cargo.toml Dependencies

### `reqwest = "0.12"` with `default-features = false, features = ["json", "stream", "rustls-tls"]`

Ollama HTTP client. Version 0.12 (not 0.13 — 0.13 is not yet stable/published; 0.12 is
the latest stable). Uses hyper 1.x internally, consistent with `hyper-util` already in
dev-dependencies.

`default-features = false` disables the default TLS backend (which links against OpenSSL
or the system security framework). Since all Ollama communication is localhost HTTP
(`http://`, not `https://`), TLS is never used. However, `rustls-tls` is included anyway
to avoid issues if reqwest internally requires a TLS feature to compile at all — it is
not used at runtime for HTTP targets.

`stream` feature: enables `.bytes_stream()` on responses — required for the NDJSON
streaming loop in `generate_stream`.

`json` feature: enables `.json::<T>()` response deserialization and `.json(&body)` request
serialization — used for non-streaming endpoints (`/api/embed`, `/api/tags`).

### `futures-util = "0.3"`

Provides `StreamExt` (specifically `.next().await`) for consuming `reqwest`'s byte stream.

`tokio-stream` (already in Cargo.toml) provides `StreamExt` too, but it is scoped to
Tokio-specific stream types. The `Bytes` stream from `reqwest` implements the general
`futures::Stream` trait, which requires `futures_util::StreamExt` to iterate with
`.next().await`. Both imports can coexist without conflict; they resolve to different
trait impls.

### `serde_json = "1"`

Runtime JSON parsing for NDJSON line processing. `serde` is already present for config
deserialization, but that only handles TOML at startup via the `toml` crate. Here we need
`serde_json::from_str::<T>(&line)` inside the async streaming loop — a separate crate.

`reqwest`'s `.json()` method handles request body serialization internally, but we still
need `serde_json` explicitly for deserializing individual NDJSON lines from the byte stream.

---

## 4. Acceptance Criteria

| # | Criterion | How to verify |
|---|-----------|---------------|
| AC-1 | All 7 prior tests still pass | `make test` |
| AC-2 | `list_available_models()` returns correct metadata for present models | `make test-inference`: asserts `phi3:mini` present with `size_bytes > 0` and non-empty `families`; asserts `list.len() > 0` |
| AC-3 | `ensure_model_available()` returns `true`/`false` correctly | `make test-inference`: `"phi3:mini"` → `Ok(true)`, `"not-a-real-model:99b"` → `Ok(false)` |
| AC-4 | `generate_stream()` yields tokens then a done final chunk | `make test-inference`: `phi3:mini` with prompt `"Say the word 'pong' and nothing else"`, asserts concatenated content contains `"pong"`, final chunk has `is_final=true`, `eval_count > 0` |
| AC-5 | `embed()` returns a non-empty vector | `make test-inference`: `phi3:mini` with `"hello world"`, asserts `vec.len() > 0` |
| AC-6 | Unreachable Ollama → typed error, no panic, structured log | `make test` (offline, no `#[ignore]`): construct engine with port 19999, assert `Err(InferenceError::OllamaUnavailable { .. })` or `RequestTimeout`, no panic |
| AC-7 | `auto_pull_missing_models = false` never triggers pull | `make test-inference`: missing model → `Ok(false)` in under 1s, no pull log |
| AC-8 | `cargo build` zero warnings, zero errors | `cargo build` |
| AC-9 | Startup health check logs correctly in both states | `make run-core` with Ollama up → INFO `ollama_reachable=true`; with Ollama stopped → WARN `ollama_reachable=false`; core stays running in both cases |

Integration tests requiring Ollama (AC-2 through AC-5, AC-7) are marked `#[ignore]`.
They run via `make test-inference` (`cargo test -- --include-ignored`), not `make test`.
AC-6 does not require Ollama and runs in `make test`.

---

## 5. Architectural Decisions

### Token stream return type

Return `Pin<Box<dyn Stream<Item = Result<TokenChunk, InferenceError>> + Send>>` wrapped
in an outer `Result<_, InferenceError>`:

```rust
pub async fn generate_stream(&self, request: GenerationRequest)
    -> Result<Pin<Box<dyn Stream<...> + Send>>, InferenceError>
```

Outer `Result` captures connection failures (Ollama unreachable before stream begins).
Inner `Result<TokenChunk>` per item captures mid-stream failures (Ollama dies during
generation). The boxed form is required because Phase 6's session handler will store the
stream in a struct field — unnameable `impl Trait` return types can't be stored in
structs without boxing. One heap allocation per generation call; negligible.

### `reqwest::Client` ownership

Store `reqwest::Client` directly in `InferenceEngine` (no wrapping `Arc`). The reqwest
`Client` is already `Clone` over an internal `Arc`, so cloning `InferenceEngine` is cheap
and shares the underlying connection pool. Derive `Clone` on `InferenceEngine`. Phase 6
clones the engine into session handler tasks without ceremony.

### NDJSON parsing

Manual line accumulator — no additional dep. Accumulate `bytes_stream()` chunks into a
`String` buffer, split on `\n`, `serde_json::from_str` each non-empty line. If multiple
parsing use cases emerge, `tokio-util::codec::LinesCodec` can replace this in a later phase.

### Mid-stream error handling

Mid-stream errors surface as `Err(InferenceError)` stream items, not panics or a separate
error channel. The caller (Phase 6 orchestrator) decides whether to surface partial output
to the user or discard it. Stream item type is locked as `Result<TokenChunk, InferenceError>`.

### Model unload

`unload_model(model_name)` sends `POST /api/chat` with `{"model": name, "messages": [], "keep_alive": 0}`.
This is the documented Ollama unload pattern. The routing logic that calls this is
Phase 5's concern (mutual exclusion between HEAVY and VISION tiers), but the primitive
lives here.

### Streaming timeout strategy: inactivity detection, not total-request timeout

`reqwest`'s `.timeout()` — whether set on `ClientBuilder` or `RequestBuilder` — is a
wall-clock timer that starts when the request is sent and fires unconditionally after N
seconds regardless of whether bytes are arriving. For streaming generation this is the
wrong primitive: a `deepseek-r1:32b` response on a complex prompt can stream for 3–4
minutes while delivering tokens every few hundred milliseconds the entire time. A 120s
total timeout would kill a legitimate long response. There is also no way to know a safe
upper bound upfront.

The correct primitive is **inactivity detection**: wrap each individual `.next()` call on
the byte stream with `tokio::time::timeout`. The timer resets every time a new chunk
arrives, so continuous streaming is never interrupted regardless of total duration. It only
fires when Ollama stops sending entirely — which indicates a hung connection or a crashed
process, not a slow but healthy generation.

Implementation in `generate_stream`:

```rust
let inactivity = Duration::from_secs(config.stream_inactivity_timeout_secs);
loop {
    match tokio::time::timeout(inactivity, byte_stream.next()).await {
        Err(_elapsed) => {
            yield Err(InferenceError::StreamInterrupted(
                format!("no bytes for {}s — Ollama may have hung", config.stream_inactivity_timeout_secs)
            ));
            break;
        }
        Ok(None) => break,           // clean end-of-stream
        Ok(Some(Err(e))) => {
            yield Err(InferenceError::StreamInterrupted(e.to_string()));
            break;
        }
        Ok(Some(Ok(bytes))) => { /* accumulate into line buffer */ }
    }
}
```

Consequence for `reqwest::Client` construction: the client is built with `connect_timeout`
only — no `.timeout()` on `ClientBuilder`. Non-streaming requests (embed, tags, unload, pull)
apply a per-request timeout via `RequestBuilder::timeout(Duration::from_secs(request_timeout_secs))`
at the call site. This keeps short requests bounded without imposing a wall-clock limit on streams.

`GenerationRequest` needs no `timeout_override_secs` field. The inactivity window is
a global config value — per-tier tuning is Phase 5's concern if it turns out to be needed.

---

## 6. Gotchas

**NDJSON chunk boundaries.** `bytes_stream()` gives arbitrary-sized `Bytes` chunks — they
do not align to line boundaries. One chunk can contain zero, one, or multiple complete JSON
lines, or a partial line. The accumulator must handle all three cases. Do not assume each
chunk is one JSON object.

**Final chunk has no `message` field.** Intermediate: `{"message":{"role":"assistant","content":"token"},"done":false}`.
Final: `{"done":true,"done_reason":"stop","eval_count":N,...}` — no `message`. Must use
`Option<OllamaMessage>` in the serde struct or deserialization fails on the final chunk.

**HTTP 404 for missing model, not a stream error.** Ollama returns `404 {"error":"model 'x' not found"}`
before streaming starts. Check HTTP status before reading the body as an NDJSON stream.
If status ≠ 200, read body as non-streaming JSON and return `InferenceError::ModelNotFound`.

**`/api/embed` not `/api/embeddings`.** Ollama ≥0.1.26 uses `/api/embed` (response:
`{"embeddings": [[...]]}` plural outer array). The older `/api/embeddings` endpoint returns
`{"embedding": [...]}` singular. Do not implement the old endpoint.

**`auto_pull_missing_models` default = `false`.** Silently pulling a 20GB model on startup
on a machine that doesn't have it is wrong. When `true` and a pull fires, log progress at
DEBUG level (not INFO).

**`ensure_model_available` ≠ "model is loaded".** `/api/tags` = downloaded (on disk).
`/api/ps` = loaded into VRAM. This function answers "is it downloaded?" only. Warm/cold
state is Phase 5/6's concern.

**`reqwest` 0.12 vs 0.13.** 0.13 is pre-release. Use 0.12 (latest stable). If Cargo
resolves a conflict with `hyper-util = "0.1"` in dev-dependencies (both use hyper 1.x),
that is expected and fine — they are compatible minor versions.

**Integration tests use `phi3:mini` as the canonical test model.** `qwen3:8b` (FAST),
`mistral-small:24b` (PRIMARY), `llama3.2-vision:11b` (VISION), and `mxbai-embed-large`
(EMBED) are not yet pulled. Available for testing as of session-005: `phi3:mini` (2.2GB,
fast), `deepseek-coder-v2:16b` (8.9GB), `deepseek-r1:32b` (19GB, avoid — slow to load).
All integration tests use `phi3:mini` only. `deepseek-coder-v2:16b` is NOT asserted in
AC-2 — that test is `phi3:mini`-only so it passes on any machine with Ollama and at least
one model pulled.

**`make test-inference` prerequisite: `phi3:mini` must be pulled.** Run `ollama pull phi3:mini`
before running `make test-inference`. The test suite does not pull models — it only tests
against what is already present. On a fresh machine, `make test-inference` will fail
AC-3/AC-4/AC-5 until at least `phi3:mini` is available.

---

## 7. Execution Order

Each step produces a clean `cargo build` before the next begins.

```
1.  constants.rs additions
    → cargo build clean

2.  config.rs: InferenceConfig + unit tests
    → cargo test: prior 7 + 3 new config tests pass

3.  Cargo.toml: reqwest + futures-util + serde_json
    → cargo build clean (reqwest compilation, ~30s)

4.  inference/error.rs
    → cargo build clean

5.  inference/models.rs
    → cargo build clean

6.  inference/engine.rs (in order within file):
    a. Private Ollama serde structs
    b. InferenceEngine::new
    c. list_available_models
    d. ensure_model_available
    e. embed
    f. unload_model
    g. pull_model
    h. generate_stream (NDJSON accumulator last — most complex)
    → cargo build clean

7.  inference/mod.rs
    → cargo build clean, zero warnings

8.  main.rs: mod inference + startup health check
    → cargo build clean, zero warnings
    → confirm #[allow(dead_code)] on ModelConfig is now removable

9.  Integration tests in engine.rs #[cfg(test)]
    → make test:           all 10+ tests pass (7 prior + new config + AC-6 offline test)
    → make test-inference: AC-2 through AC-5, AC-7 pass against live Ollama

10. Makefile: test-inference target
    → make test-inference runs without error
```

---

## 8. Notes

**reqwest client construction.** Build the client once in `InferenceEngine::new` with
`connect_timeout` only — do NOT set `.timeout()` on `ClientBuilder`. Applying a global
read timeout here would break streaming generation (see the streaming timeout decision in
Section 5). Non-streaming calls apply `RequestBuilder::timeout(Duration::from_secs(request_timeout_secs))`
at each individual call site instead.

**Streaming generation: no total-request timeout, inactivity detection only.**
`ClientBuilder::timeout` and `RequestBuilder::timeout` both apply to the entire
request+response cycle including the streaming body. They cannot be used for
`generate_stream` without killing legitimate long responses from heavy-tier models.
The inactivity wrapper (see Section 5) is the only correct mechanism. `request_timeout_secs`
in `InferenceConfig` is used exclusively by non-streaming requests. The two timeout
fields in `InferenceConfig` have distinct, non-overlapping responsibilities.

**Phase 6 replacement pattern.** In Phase 6, `InferenceEngine` moves into `CoreOrchestrator`,
which holds it for the process lifetime. The Phase 4 startup health check in `main.rs`
(construct → health check → drop) is a temporary arrangement — it is deliberately not the
final architecture. The comment in `main.rs` must make this explicit.

**`pull_model` progress logging.** Ollama's pull response is also NDJSON:
`{"status":"pulling manifest"}`, `{"status":"downloading digestN","completed":N,"total":N}`,
`{"status":"success"}`. Parse these and emit `debug!(...)` log lines. Never INFO — a 20GB
pull would produce thousands of progress lines.

**Error context on `OllamaUnavailable`.** Include the attempted URL in the error variant
so log lines are self-describing: `"Ollama unreachable at http://localhost:11434"`. This
matters when `ollama_base_url` is non-default (e.g. remote Ollama instance in a future setup).
