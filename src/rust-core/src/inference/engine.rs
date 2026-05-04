/// Ollama HTTP inference engine for the Dexter core.
///
/// Wraps the Ollama REST API in a typed Rust interface. All Ollama-specific HTTP and
/// JSON handling is contained here — callers never see reqwest, serde_json, or raw URL
/// strings. The public API surface is:
///
///   - `generate_stream()`        — streaming chat generation (NDJSON, yields TokenChunks)
///   - `embed()`                  — single-input dense embedding vector
///   - `list_available_models()`  — inventory of all models on disk
///   - `ensure_model_available()` — check-or-pull policy gate
///   - `unload_model()`           — evict a model from VRAM via keep_alive: 0
///   - `pull_model()`             — download a model, streaming progress
///
/// ## Streaming timeout strategy
///
/// `generate_stream` uses streaming-specific timeout boundaries, NOT a total-request
/// timeout. `reqwest::ClientBuilder::timeout` / `RequestBuilder::timeout` applies a
/// wall-clock deadline to the entire request+response pair and would kill a legitimate
/// 3–4 minute HEAVY-tier response. Dexter instead caps the pre-stream response-header
/// wait, then wraps each individual `.next()` await call with
/// `tokio::time::timeout(inactivity_window, stream.next())`. The per-chunk window resets
/// on every received byte chunk and only fires if Ollama stops sending entirely — exactly
/// the case we want to detect (hung connection, crashed process, OOM kill).
///
/// Non-streaming endpoints (embed, list, unload, pull status check) use
/// `RequestBuilder::timeout(request_timeout)` because they have a bounded response size
/// and a genuine wall-clock SLA.
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::InferenceConfig;
use crate::constants::{
    OLLAMA_CHAT_PATH, OLLAMA_EMBED_PATH, OLLAMA_PS_PATH, OLLAMA_PULL_PATH, OLLAMA_TAGS_PATH,
};

use super::error::InferenceError;
use super::models::ModelInfo;

// ── Public request/response types ─────────────────────────────────────────────

/// Semantic origin of a message — separate from its wire `role`.
///
/// Phase 37.7: introduced to fix a budget-accounting bug in `ConversationContext`.
/// Ollama's chat API only accepts `"system" | "user" | "assistant"` as roles, and
/// messages without native tool-calling support (qwen3:8b, llama3, etc.) silently
/// drop custom roles. So tool-result and retrieval injections MUST serialize as
/// `role: "user"` to be visible to the model. But that means `turn_count()`
/// counting `role == "user"` silently includes synthetic injections, and the
/// `max_turns` budget evicts real history to make room for them.
///
/// `MessageOrigin` records the true semantic source. It's local metadata —
/// `#[serde(skip)]` keeps it out of the Ollama wire format. `turn_count()` counts
/// only `Origin::User`, so the budget reflects real operator turns regardless of
/// how many synthetic injections have accumulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageOrigin {
    /// Personality prompt at index 0 (role == "system").
    System,
    /// Genuine operator turn (role == "user"). Counts toward `max_turns`.
    #[default]
    User,
    /// Model response (role == "assistant").
    Assistant,
    /// Action-executor output injected as role="user". Does NOT count toward budget.
    ToolResult,
    /// Retrieval pipeline injection (Phase 9) as role="user". Does NOT count.
    Retrieval,
}

/// A single message in a chat turn.
///
/// Maps 1:1 to Ollama's `{"role": "...", "content": "..."}` message object for
/// wire serialization. The `origin` field is local-only metadata — excluded from
/// both serialize and deserialize via `#[serde(skip)]` — and carries the true
/// semantic source for budget accounting and trimming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    /// Base64-encoded image payloads for multimodal (vision) inference.
    ///
    /// Sent to Ollama's `/api/chat` as `"images": ["<base64>"]` per the multimodal
    /// API contract. `None` (the default) omits the field entirely from serialization
    /// via `skip_serializing_if` — normal text-only messages are unaffected.
    ///
    /// Images are EPHEMERAL: attached per-request by the orchestrator's vision path
    /// and never stored in `ConversationContext`. Session state never contains images.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub images: Option<Vec<String>>,

    /// Semantic origin — local metadata, not serialized to Ollama.
    /// See `MessageOrigin` for why this exists separate from `role`.
    #[serde(skip)]
    #[serde(default)]
    pub origin: MessageOrigin,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
            images: None,
            origin: MessageOrigin::System,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            images: None,
            origin: MessageOrigin::User,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            images: None,
            origin: MessageOrigin::Assistant,
        }
    }

    /// Construct a user message with a single base64-encoded image attachment.
    ///
    /// Used by the Vision routing path in the orchestrator: the screen capture is
    /// base64-encoded and attached to the primary user turn before generation.
    /// The returned message serializes to:
    /// `{"role":"user","content":"...","images":["<base64>"]}`
    #[allow(dead_code)] // Phase 20 — used in vision tests; callable by future vision pipelines
    pub fn user_with_image(content: impl Into<String>, image_b64: String) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            images: Some(vec![image_b64]),
            origin: MessageOrigin::User,
        }
    }

    /// Construct a synthetic tool-result message.
    ///
    /// Serializes as role="user" so non-tool-capable models see it, but carries
    /// `MessageOrigin::ToolResult` so the conversation-budget accounting can
    /// distinguish it from real operator input. See `MessageOrigin` docs for
    /// the full rationale.
    pub fn tool_result(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            images: None,
            origin: MessageOrigin::ToolResult,
        }
    }

    /// Construct a synthetic retrieval-injection message. Same wire shape as
    /// a user message (role="user"), different semantic origin.
    #[allow(dead_code)] // Phase 9 — retrieval pipeline call site
    pub fn retrieval(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            images: None,
            origin: MessageOrigin::Retrieval,
        }
    }
}

/// Input for a streaming chat generation request.
#[derive(Debug, Clone)]
pub struct GenerationRequest {
    /// Ollama model tag, e.g. `"qwen3:8b"`. Resolved from `ModelId::ollama_name()` by
    /// the caller — the engine does not know about `ModelId`.
    pub model_name: String,

    /// Full conversation history. The engine forwards this verbatim to Ollama;
    /// context truncation and summarization are the orchestrator's responsibility.
    pub messages: Vec<Message>,

    /// Sampling temperature. `None` uses Ollama's default (typically 0.8).
    /// Range: 0.0 (deterministic) – 2.0 (very random).
    pub temperature: Option<f32>,

    /// If true, send `keep_alive: 0` with the request to evict the model from VRAM
    /// immediately after generation completes. Used by the `Heavy` tier policy.
    pub unload_after: bool,

    /// Phase 24: explicit `keep_alive` override. When `Some`, this value is sent
    /// regardless of `unload_after`. Used by the KV cache prefill to pin the
    /// FAST model in VRAM with `FAST_MODEL_KEEP_ALIVE` ("999m").
    ///
    /// Priority: `unload_after` (keep_alive:"0") > `keep_alive_override` > default (None).
    pub keep_alive_override: Option<&'static str>,

    /// Phase 24: maximum tokens to generate. `None` = Ollama default.
    /// For KV cache prefill: `Some(1)` — warm the KV cache, discard the output.
    pub num_predict: Option<u32>,

    /// Per-request override for the stream inactivity timeout. `None` uses
    /// `InferenceConfig.stream_inactivity_timeout_secs` (default: 30s).
    ///
    /// Use `Some(300)` for the startup warmup: Ollama is silent while loading a
    /// cold model from external storage (can take several minutes for large GGUF
    /// files). The 30s default fires during that loading window as a false stall,
    /// causing the warmup to fail before any tokens are generated.
    pub inactivity_timeout_override_secs: Option<u64>,

    /// Phase 37.6 / Cluster-E (B15): per-request context-window override.
    /// `None` lets Ollama use the model-trained default; `Some(n)` caps the
    /// KV cache to `n` tokens. Set via `constants::LARGE_MODEL_NUM_CTX` for
    /// tiers whose native context is too large to fit alongside the always-warm
    /// stack — currently Heavy (deepseek-r1:32b, 131k native) and Code
    /// (deepseek-coder-v2:16b, 163k native). See `ModelId::needs_context_cap`.
    pub num_ctx_override: Option<u32>,
}

/// A single streamed token chunk from `generate_stream`.
///
/// The caller collects these and either streams them over gRPC (via the `session` handler)
/// or accumulates them into a complete response.
#[derive(Debug, Clone)]
pub struct TokenChunk {
    /// The token text for this chunk. May be a single character, a word, or a
    /// sentence fragment — Ollama's granularity varies by model.
    pub content: String,

    /// True on the final chunk. The final chunk carries `eval_count` but no `content`.
    /// Callers should use this flag to detect stream completion rather than checking
    /// for empty `content` (which can occur mid-stream for some models).
    pub done: bool,

    /// Token count for the generated portion of the response. Only meaningful when
    /// `done == true`; zero on intermediate chunks. Consumed in Phase 7 telemetry.
    #[allow(dead_code)] // Phase 7 — telemetry / cost accounting
    pub eval_count: u64,

    /// Model-load duration in milliseconds, populated only on `done == true`.
    ///
    /// A value > ~5000 ms on a model the orchestrator believes is warm (e.g.
    /// `primary_model_warm=true`) indicates OS-level page reclamation of the
    /// mmap'd GGUF file — Ollama still has the model "loaded" per `keep_alive`,
    /// but the weight pages were paged out and had to be faulted back from
    /// disk. This is the signal the PRIMARY keepalive ping task is designed
    /// to prevent; surfacing `load_duration_ms` to the orchestrator lets us
    /// emit a `warn!` when the prevention fails instead of discovering it by
    /// operator-perceived latency.
    ///
    /// `None` on intermediate chunks; `Some(ms)` on the final chunk.
    pub load_duration_ms: Option<u64>,
}

/// Phase 38 / Codex finding [7]: a streaming generation handle that exposes
/// both the receiver and the producer task so cancellable callers can abort
/// the producer directly when the generation is cancelled.
///
/// The producer task owns `response.bytes_stream()` (the reqwest HTTP stream).
/// Aborting `producer` drops that stream → closes the HTTP connection →
/// stops Ollama from generating any further server-side. Without this, a
/// barge-in mid-cold-load left the producer parked at `byte_stream.next().await`
/// for up to `inactivity` seconds before discovering the consumer had dropped
/// the receiver — meaning Ollama kept generating a response no operator would
/// ever see.
///
/// Non-cancellable callers (warm-up requests, keepalive pings, fast-path
/// helpers) should use `.into_rx_detached()` to drop the producer handle and
/// recover the pre-Phase-38 detached behavior. Dropping a `JoinHandle`
/// detaches the task without aborting it; the task runs to completion
/// independently.
pub struct GenerationStream {
    /// Drain this to receive `TokenChunk`s.
    pub rx: tokio::sync::mpsc::Receiver<Result<TokenChunk, InferenceError>>,
    /// JoinHandle for the producer task. Cancellable callers should keep this
    /// (or its `.abort_handle()`) so they can cancel the upstream HTTP stream.
    pub producer: tokio::task::JoinHandle<()>,
}

impl GenerationStream {
    /// Discard the producer JoinHandle (let the spawned task run detached, the
    /// pre-Phase-38 behavior). Use for non-cancellable call sites where the
    /// generation is bounded and short — warm-up pings, `num_predict: Some(1)`
    /// helpers, fast-path retrievers — and there's no caller-side cancel path.
    ///
    /// Dropping a `JoinHandle` detaches the task; it does NOT abort.
    pub fn into_rx_detached(
        self,
    ) -> tokio::sync::mpsc::Receiver<Result<TokenChunk, InferenceError>> {
        let _ = self.producer; // explicit drop = detach
        self.rx
    }
}

/// Input for a single-text embedding request.
#[allow(dead_code)] // Phase 9 — Retrieval Pipeline
#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    /// Ollama model tag for the embedding model, e.g. `"mxbai-embed-large"`.
    pub model_name: String,

    /// The text to embed. For retrieval use cases, prefix search queries with
    /// `"Represent this sentence for searching relevant passages: "` per
    /// the mxbai-embed-large documentation.
    pub input: String,
}

// ── Internal Ollama wire types (not exposed outside this module) ───────────────
//
// These mirror the Ollama API JSON shapes. They are separate from the public
// types above so that API changes only affect these structs, not callers.
// All fields use `snake_case` which matches Ollama's JSON directly.

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
    /// keep_alive = 0 evicts the model from VRAM immediately after the request.
    /// Sent as a string per Ollama docs ("0" or "5m", not an integer).
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<&'static str>,
    /// Disable qwen3's hybrid thinking mode (Phase 23).
    ///
    /// qwen3 models default to generating a hidden <think>...</think> reasoning
    /// chain before any visible output. Ollama filters these tokens from the
    /// stream, so from the caller's perspective the model is completely silent
    /// for the entire thinking phase — typically 30 s–5 min on a CPU-only run.
    /// `"think": false` disables this, producing direct responses immediately.
    ///
    /// Harmless for non-qwen3 models: Ollama ignores unknown top-level fields.
    think: bool,
}

#[derive(Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// Phase 24: maximum tokens to generate. `None` = Ollama default (unlimited).
    /// For KV cache prefill requests, set to `1` — process the full prompt into
    /// the KV cache, generate exactly 1 output token, then stop. The output is
    /// discarded; the purpose is to warm the cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
    /// Phase 37.6 / Cluster-E (B15): context-window size in tokens. `None` lets
    /// Ollama pick the model-trained default. For tiers flagged by
    /// `ModelId::needs_context_cap` (currently Heavy and Code) we cap this to
    /// `LARGE_MODEL_NUM_CTX` (8,192) — e.g. deepseek-r1:32b's 131k default KV
    /// cache consumes ~32 GiB and forces 56% CPU spill on a 28 GiB GPU budget,
    /// making generation unusably slow. See `constants::LARGE_MODEL_NUM_CTX`
    /// for the full rationale.
    #[serde(skip_serializing_if = "Option::is_none")]
    num_ctx: Option<u32>,
    /// Diagnostic (post-Phase 38c): explicit per-request RNG seed.
    ///
    /// We observed byte-identical model output across distinct user turns under
    /// the FAST tier (qwen3:8b) — same 46-token response repeated verbatim 5
    /// times in a row despite changing user messages and growing conversation
    /// history. Temperature was at Ollama default (0.8), no `seed` field was
    /// being sent.
    ///
    /// Hypothesis: Ollama may use a deterministic default seed when the field
    /// is omitted, making sampling reproducible-given-prompt rather than
    /// stochastic. By injecting a fresh seed per request (derived from the
    /// system clock's lower 32 bits of nanoseconds), we force Ollama down a
    /// different sample path on each call. If output stays byte-identical even
    /// with varying seeds, the bug is upstream of sampling — likely the small
    /// model collapsing into a degenerate attractor under bloated context, in
    /// which case the Adaptive Context Compiler is the real fix.
    ///
    /// Either way, the seed is logged at debug! so failing turns can be
    /// reproduced post-hoc by re-running with the same seed.
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u32>,
}

/// Intermediate streaming chunk from /api/chat.
/// `message` is None on the final (done=true) chunk.
///
/// Timing fields (`load_duration`, `prompt_eval_duration`, `eval_duration`) are
/// sent by Ollama ONLY on the final (`done: true`) chunk. All values are in
/// nanoseconds. These are logged at the engine layer for Cluster-E diagnostics
/// (HEAVY-tier swap behavior under memory pressure — Phase 37.6).
#[derive(Deserialize, Debug)]
struct OllamaChatChunk {
    #[serde(default)]
    message: Option<OllamaChunkMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    eval_count: Option<u64>,
    /// Nanoseconds Ollama spent loading the model weights for this request.
    /// `0` when the model was already warm; large when cold-loaded from disk.
    #[serde(default)]
    load_duration: Option<u64>,
    /// Nanoseconds spent evaluating the prompt (prefill).
    #[serde(default)]
    prompt_eval_duration: Option<u64>,
    /// Nanoseconds spent generating the response tokens.
    #[serde(default)]
    eval_duration: Option<u64>,
}

#[derive(Deserialize, Debug)]
struct OllamaChunkMessage {
    #[serde(default)]
    content: String,
}

#[derive(Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
    /// Keep the embed model resident for 10 minutes between requests (Phase 23).
    /// Without this Ollama evicts it after the default 5-minute TTL, forcing a
    /// ~30–45 s cold-reload on the next voice query. "10m" keeps it warm through
    /// a normal conversation gap without consuming GPU/RAM indefinitely.
    keep_alive: &'static str,
}

/// Response from /api/embed (Ollama ≥0.1.26).
/// Outer array is always length 1 for single-input requests.
#[derive(Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

/// Response from /api/tags — the model inventory.
#[derive(Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaTagsModel>,
}

#[derive(Deserialize)]
struct OllamaTagsModel {
    name: String,
    size: u64,
    digest: String,
    details: OllamaModelDetails,
}

#[derive(Deserialize, Default)]
struct OllamaModelDetails {
    #[serde(default)]
    parameter_size: String,
    #[serde(default)]
    quantization_level: String,
    // `Option<Vec<String>>` rather than `Vec<String>`: some older models in Ollama's
    // registry return `"families": null` (not absent — explicit JSON null). serde's
    // `#[serde(default)]` handles *missing* keys but not explicit null; `Option<>` is
    // the correct wrapper for nullable JSON arrays.
    #[serde(default)]
    families: Option<Vec<String>>,
}

// ── /api/ps types (Phase 37.6 — HEAVY-swap diagnostics) ───────────────────────

/// One entry from Ollama's `/api/ps` response — a model currently loaded into
/// VRAM or system RAM. Only the fields we actually use for diagnostics are
/// deserialized; everything else Ollama returns is ignored.
///
/// The critical comparison is `size_vram` vs `size`:
///   - `size_vram == size` → fully GPU-resident; first-token in seconds
///   - `size_vram <  size` → partial CPU spill; first-token in tens of seconds
///   - model absent from /api/ps → not loaded (or load failed silently)
#[derive(Deserialize, Debug, Clone)]
pub struct OllamaPsEntry {
    pub name: String,
    /// Total bytes the model needs to run at full speed (GPU + CPU combined).
    pub size: u64,
    /// Bytes actually resident in GPU memory (or unified memory on Apple Silicon).
    pub size_vram: u64,
    /// ISO-8601 timestamp when Ollama will auto-evict the model. Useful for
    /// confirming keep_alive windows. Deserialized as a raw string to avoid
    /// pulling in a datetime crate for a diagnostic log field.
    #[serde(default)]
    #[allow(dead_code)] // Phase 37.6 — available for future keep_alive verification
    pub expires_at: String,
}

#[derive(Deserialize)]
struct OllamaPsResponse {
    #[serde(default)]
    models: Vec<OllamaPsEntry>,
}

/// Response line from /api/pull progress stream.
#[allow(dead_code)] // Phase 9 — pull_model() uses this for progress reporting
#[derive(Deserialize)]
struct OllamaPullProgress {
    status: String,
    #[serde(default)]
    completed: Option<u64>,
    #[serde(default)]
    total: Option<u64>,
}

// ── InferenceEngine ───────────────────────────────────────────────────────────

/// HTTP client for the Ollama inference API.
///
/// The engine is cheaply cloneable — `reqwest::Client` is an `Arc` internally.
/// The `InferenceConfig` is stored as-is; the engine does not hold a reference to
/// the full `DexterConfig` to keep the dependency footprint minimal.
///
/// Construct once at startup (see `main.rs`) and pass as `Arc<InferenceEngine>` to
/// any component that needs inference. The engine itself has no mutable state.
#[derive(Clone)]
pub struct InferenceEngine {
    client: Client,
    config: InferenceConfig,
    base_url: String,
}

impl InferenceEngine {
    /// Construct a new engine from the operator's inference configuration.
    ///
    /// This does NOT contact Ollama — it only builds the HTTP client. Call
    /// `list_available_models()` to verify connectivity, or rely on the startup
    /// health-check in `main.rs`.
    pub fn new(config: InferenceConfig) -> Result<Self, InferenceError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
            // No global .timeout() — streaming responses need inactivity detection,
            // not a wall-clock timeout. Per-request timeouts are set at call sites
            // for the non-streaming methods only.
            .build()
            .map_err(|e| InferenceError::OllamaUnavailable {
                url: config.ollama_base_url.clone(),
                source: format!("failed to build HTTP client: {e}"),
            })?;

        let base_url = config.ollama_base_url.trim_end_matches('/').to_string();

        Ok(Self {
            client,
            config,
            base_url,
        })
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Stream a chat generation from Ollama, yielding `TokenChunk`s via an async channel.
    ///
    /// Returns a `Receiver<Result<TokenChunk, InferenceError>>`; the producer task
    /// is detached. Use this entry point for non-cancellable generations — warm-up
    /// pings, fast-path retrievers, keepalive checks, anything where the request
    /// is bounded and short. Cancellable generations should use
    /// `generate_stream_cancellable` (Phase 38 / Codex [7]) so the producer
    /// JoinHandle is exposed and can be aborted to close the upstream HTTP
    /// connection immediately on barge-in.
    pub async fn generate_stream(
        &self,
        req: GenerationRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<TokenChunk, InferenceError>>, InferenceError>
    {
        Ok(self
            .generate_stream_cancellable(req)
            .await?
            .into_rx_detached())
    }

    /// Phase 38 / Codex finding [7]: cancellable streaming generation.
    ///
    /// Returns a `GenerationStream` containing both the receiver and the producer
    /// JoinHandle. The caller drains `rx`; the engine drives a separately-owned
    /// task referenced by `producer`. Pre-Phase-38, all generations went through
    /// the detached path: aborting the consumer dropped `rx` which eventually
    /// closed the channel, but the producer could be parked at
    /// `byte_stream.next().await` for up to `inactivity` seconds before
    /// discovering the closed channel — meaning a barge-in mid-cold-load kept
    /// Ollama generating server-side until the inactivity timeout fired.
    /// Calling `.abort_handle().abort()` on `producer` drops the byte stream and
    /// closes the HTTP connection immediately.
    ///
    /// Using an mpsc channel rather than returning a `Stream` avoids generic
    /// return-type complexity in `server.rs` and allows the engine to be `Clone`
    /// + object-safe. The channel capacity is `INFERENCE_CHANNEL_CAPACITY` (32
    /// slots) — enough to buffer a burst of tokens between the inference task
    /// and the gRPC sender without stalling the generator.
    ///
    /// The producer task terminates when:
    ///   - Ollama sends `done: true` (normal completion)
    ///   - The inactivity timeout fires (yields `Err(StreamInterrupted)`)
    ///   - A deserialization error occurs (yields `Err(SerializationError)`)
    ///   - The receiver is dropped (channel send fails → task exits silently)
    ///   - `producer.abort()` is called (Phase 38 / Codex [7])
    pub async fn generate_stream_cancellable(
        &self,
        req: GenerationRequest,
    ) -> Result<GenerationStream, InferenceError> {
        let url = format!("{}{}", self.base_url, OLLAMA_CHAT_PATH);

        let keep_alive = if req.unload_after {
            Some("0")
        } else {
            req.keep_alive_override
        };

        // Diagnostic: derive a per-request seed from the system clock so that
        // Ollama cannot fall back to a deterministic default. See OllamaOptions.seed
        // doc comment for the full rationale. Lower 32 bits of nanoseconds since
        // UNIX_EPOCH gives microsecond-level entropy — well-distributed for our
        // purpose ("a different sample path per call"), not cryptographic.
        let request_seed: u32 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or(0);

        let body = OllamaChatRequest {
            model: &req.model_name,
            messages: &req.messages,
            stream: true,
            options: Some(OllamaOptions {
                temperature: req.temperature,
                num_predict: req.num_predict,
                num_ctx: req.num_ctx_override,
                seed: Some(request_seed),
            }),
            keep_alive,
            think: false, // disable qwen3 hidden reasoning chain
        };

        debug!(
            model   = %req.model_name,
            msgs    = req.messages.len(),
            unload  = req.unload_after,
            seed    = request_seed,
            "Starting streaming generation"
        );

        // Send the request and get the raw response headers. We still avoid a
        // RequestBuilder global timeout because that would cap the full stream,
        // but the initial `.send().await` happens before we have a byte stream to
        // apply per-chunk inactivity detection to. If Ollama wedges while loading
        // a model and never returns headers, this guard is the only thing that can
        // return control to the orchestrator.
        let header_timeout = Duration::from_secs(
            req.inactivity_timeout_override_secs.unwrap_or(
                self.config
                    .request_timeout_secs
                    .max(self.config.stream_inactivity_timeout_secs),
            ),
        );
        let response =
            match timeout(header_timeout, self.client.post(&url).json(&body).send()).await {
                Ok(send_result) => send_result.map_err(|e| {
                    if e.is_connect() {
                        InferenceError::OllamaUnavailable {
                            url: url.clone(),
                            source: e.to_string(),
                        }
                    } else {
                        InferenceError::from(e)
                    }
                })?,
                Err(_elapsed) => {
                    return Err(InferenceError::StreamInterrupted(format!(
                        "no response headers from Ollama for {}s before stream started (model: {})",
                        header_timeout.as_secs(),
                        req.model_name,
                    )));
                }
            };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(InferenceError::ApiError { status, message });
        }

        // Channel capacity mirrors INFERENCE_CHANNEL_CAPACITY but is inlined here
        // to avoid an import cycle (constants → config → engine is fine; the
        // constant value is 32 and unlikely to change).
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<TokenChunk, InferenceError>>(32);

        let inactivity = Duration::from_secs(
            req.inactivity_timeout_override_secs
                .unwrap_or(self.config.stream_inactivity_timeout_secs),
        );
        let model_name = req.model_name.clone();

        let producer = tokio::spawn(async move {
            let mut byte_stream = response.bytes_stream();
            // Line-framing buffer: accumulate bytes until `\n`, then decode each line.
            let mut line_buf = String::new();

            loop {
                // Inactivity detection: each `.next()` await gets its own timeout window.
                // The window resets on every received chunk — a model producing tokens
                // every 500ms for 10 minutes will never fire this. It fires only when
                // Ollama goes completely silent (hung connection, OOM kill, process crash).
                match timeout(inactivity, byte_stream.next()).await {
                    Err(_elapsed) => {
                        let _ = tx
                            .send(Err(InferenceError::StreamInterrupted(format!(
                                "no data from Ollama for {}s (model: {})",
                                inactivity.as_secs(),
                                model_name,
                            ))))
                            .await;
                        break;
                    }

                    // Stream ended cleanly — Ollama closed the connection.
                    Ok(None) => break,

                    // Network-level error mid-stream.
                    Ok(Some(Err(e))) => {
                        let _ = tx
                            .send(Err(InferenceError::ApiError {
                                status: e.status().map(|s| s.as_u16()).unwrap_or(0),
                                message: e.to_string(),
                            }))
                            .await;
                        break;
                    }

                    // Received a bytes chunk — append to the line buffer and process
                    // any complete lines (delimited by `\n`).
                    Ok(Some(Ok(bytes))) => {
                        // `bytes` is arbitrary-length and may contain 0, 1, or many
                        // complete NDJSON lines. We extend the buffer and split, keeping
                        // any trailing incomplete line for the next chunk.
                        match std::str::from_utf8(&bytes) {
                            Err(e) => {
                                let _ = tx
                                    .send(Err(InferenceError::SerializationError(format!(
                                        "UTF-8 decode error in stream: {e}"
                                    ))))
                                    .await;
                                break;
                            }
                            Ok(text) => {
                                line_buf.push_str(text);

                                // Drain all complete lines.
                                while let Some(newline_pos) = line_buf.find('\n') {
                                    let line: String = line_buf.drain(..=newline_pos).collect();
                                    let line = line.trim();
                                    if line.is_empty() {
                                        continue;
                                    }

                                    match serde_json::from_str::<OllamaChatChunk>(line) {
                                        Err(e) => {
                                            let _ = tx
                                                .send(Err(InferenceError::SerializationError(
                                                    format!(
                                                        "NDJSON parse error: {e} — line: {line}"
                                                    ),
                                                )))
                                                .await;
                                            return; // exit the spawned task
                                        }
                                        Ok(chunk) => {
                                            let content = chunk
                                                .message
                                                .map(|m| m.content)
                                                .unwrap_or_default();
                                            let eval_count = chunk.eval_count.unwrap_or(0);
                                            // Phase 37.6 / Cluster-E diagnostics: Ollama sends
                                            // timing fields only on the final chunk. `load_duration`
                                            // is the signal that proves "warm" vs "cold-loaded
                                            // from disk" — a value >1s on a supposedly warm model
                                            // means keep_alive didn't hold. `eval_duration` +
                                            // `eval_count` gives generation throughput.
                                            if chunk.done {
                                                let ld_ms =
                                                    chunk.load_duration.unwrap_or(0) / 1_000_000;
                                                let ped_ms =
                                                    chunk.prompt_eval_duration.unwrap_or(0)
                                                        / 1_000_000;
                                                let ed_ms =
                                                    chunk.eval_duration.unwrap_or(0) / 1_000_000;
                                                let tps = if ed_ms > 0 {
                                                    (eval_count as f64) / (ed_ms as f64 / 1000.0)
                                                } else {
                                                    0.0
                                                };
                                                info!(
                                                    model             = %model_name,
                                                    load_ms           = ld_ms,
                                                    prompt_eval_ms    = ped_ms,
                                                    eval_ms           = ed_ms,
                                                    eval_tokens       = eval_count,
                                                    tokens_per_sec    = format!("{tps:.1}"),
                                                    "Generation complete — Ollama timing report"
                                                );
                                            }
                                            // Surface load_duration_ms only on the final
                                            // chunk — matches the source-of-truth Ollama
                                            // timing report above. Intermediate chunks
                                            // always carry `None`.
                                            let load_duration_ms = if chunk.done {
                                                chunk.load_duration.map(|ns| ns / 1_000_000)
                                            } else {
                                                None
                                            };
                                            let token = TokenChunk {
                                                content,
                                                done: chunk.done,
                                                eval_count,
                                                load_duration_ms,
                                            };
                                            // If the receiver was dropped, the caller
                                            // cancelled — exit silently.
                                            if tx.send(Ok(token)).await.is_err() {
                                                return;
                                            }
                                            if chunk.done {
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(GenerationStream { rx, producer })
    }

    /// Embed a single text input and return the embedding vector.
    ///
    /// Uses `/api/embed` (Ollama ≥0.1.26). The response is `{"embeddings": [[f32, ...]]}`.
    /// The outer array is always length 1 for single-input requests.
    ///
    /// Uses `request_timeout_secs` — this is a non-streaming, bounded-size response.
    #[allow(dead_code)] // Phase 9 — Retrieval Pipeline
    pub async fn embed(&self, req: EmbeddingRequest) -> Result<Vec<f32>, InferenceError> {
        let url = format!("{}{}", self.base_url, OLLAMA_EMBED_PATH);
        let body = OllamaEmbedRequest {
            model: &req.model_name,
            input: &req.input,
            keep_alive: "10m",
        };

        debug!(model = %req.model_name, chars = req.input.len(), "Embedding request");

        let response = self
            .client
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(self.config.request_timeout_secs))
            .send()
            .await
            .map_err(InferenceError::from)?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(InferenceError::ApiError { status, message });
        }

        let embed_resp: OllamaEmbedResponse = response
            .json()
            .await
            .map_err(|e| InferenceError::SerializationError(e.to_string()))?;

        embed_resp.embeddings.into_iter().next().ok_or_else(|| {
            InferenceError::SerializationError(
                "Ollama /api/embed returned empty embeddings array".to_string(),
            )
        })
    }

    /// Return the inventory of all models currently on disk.
    ///
    /// Calls `/api/tags`. Models returned are on disk but not necessarily in VRAM.
    /// Uses `request_timeout_secs`.
    pub async fn list_available_models(&self) -> Result<Vec<ModelInfo>, InferenceError> {
        let url = format!("{}{}", self.base_url, OLLAMA_TAGS_PATH);

        let response = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(self.config.request_timeout_secs))
            .send()
            .await
            .map_err(|e| {
                // Surface connect errors as OllamaUnavailable rather than the generic
                // ApiError — connect errors mean the daemon is down, which is
                // operationally different from a 500.
                if e.is_connect() || e.is_timeout() {
                    InferenceError::OllamaUnavailable {
                        url: url.clone(),
                        source: e.to_string(),
                    }
                } else {
                    InferenceError::from(e)
                }
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(InferenceError::ApiError { status, message });
        }

        let tags: OllamaTagsResponse = response
            .json()
            .await
            .map_err(|e| InferenceError::SerializationError(e.to_string()))?;

        let models = tags
            .models
            .into_iter()
            .map(|m| ModelInfo {
                name: m.name,
                size_bytes: m.size,
                digest: m.digest,
                parameter_size: m.details.parameter_size,
                quantization: m.details.quantization_level,
                families: m.details.families.unwrap_or_default(),
            })
            .collect();

        Ok(models)
    }

    /// Return the list of models currently loaded in Ollama's process memory.
    ///
    /// Calls `/api/ps`. Unlike `list_available_models` (which reports models on
    /// disk), this reports models that are actually *resident* — either in VRAM
    /// (`size_vram > 0`) or partially spilled to CPU (`size_vram < size`).
    ///
    /// Used by the HEAVY-tier dispatch path (Phase 37.6) to diagnose load
    /// failures and partial-offload scenarios that otherwise surface as silent
    /// 40-second first-token stalls. Failures are non-fatal: the caller should
    /// log the error and continue, not abort dispatch.
    #[allow(dead_code)] // Phase 37.6 — wired from orchestrator HEAVY dispatch
    pub async fn ps(&self) -> Result<Vec<OllamaPsEntry>, InferenceError> {
        let url = format!("{}{}", self.base_url, OLLAMA_PS_PATH);

        let response = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(self.config.request_timeout_secs))
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() || e.is_timeout() {
                    InferenceError::OllamaUnavailable {
                        url: url.clone(),
                        source: e.to_string(),
                    }
                } else {
                    InferenceError::from(e)
                }
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(InferenceError::ApiError { status, message });
        }

        let ps: OllamaPsResponse = response
            .json()
            .await
            .map_err(|e| InferenceError::SerializationError(e.to_string()))?;

        Ok(ps.models)
    }

    /// Check whether a model is available on disk. If not, either pull it or return an error
    /// depending on `InferenceConfig.auto_pull_missing_models`.
    ///
    /// Called before routing a generation request to avoid opaque Ollama 404 responses.
    #[allow(dead_code)] // Phase 9 — ensure model is on disk before routing inference requests
    pub async fn ensure_model_available(&self, model_name: &str) -> Result<(), InferenceError> {
        let models = self.list_available_models().await?;
        let found = models
            .iter()
            .any(|m| m.name == model_name || m.name.starts_with(&format!("{model_name}:")));

        if found {
            debug!(model = %model_name, "Model is available");
            return Ok(());
        }

        if self.config.auto_pull_missing_models {
            info!(model = %model_name, "Model not found — pulling (auto_pull_missing_models=true)");
            self.pull_model(model_name).await
        } else {
            Err(InferenceError::ModelNotFound(model_name.to_string()))
        }
    }

    /// Evict a model from VRAM by sending a generation request with `keep_alive: 0`.
    ///
    /// Ollama does not have a dedicated "unload" endpoint. The standard approach is to
    /// send a minimal `/api/chat` request with `keep_alive: 0`, which tells Ollama to
    /// expire the model from VRAM immediately after handling the request.
    ///
    /// Used by the Heavy-tier policy: after each Heavy generation, the orchestrator
    /// calls `unload_model` to reclaim VRAM for the Primary or Code model.
    #[allow(dead_code)] // Phase 6+ — Heavy-tier VRAM reclamation after generation
    pub async fn unload_model(&self, model_name: &str) -> Result<(), InferenceError> {
        let url = format!("{}{}", self.base_url, OLLAMA_CHAT_PATH);
        let body = OllamaChatRequest {
            model: model_name,
            messages: &[],
            stream: false,
            options: None,
            keep_alive: Some("0"),
            think: false,
        };

        debug!(model = %model_name, "Unloading model from VRAM");

        let response = self
            .client
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(self.config.request_timeout_secs))
            .send()
            .await
            .map_err(InferenceError::from)?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(InferenceError::ApiError { status, message });
        }

        debug!(model = %model_name, "Model unloaded from VRAM");
        Ok(())
    }

    /// Pull (download) a model from the Ollama registry, streaming progress to the log.
    ///
    /// Blocks until the pull completes. Uses `request_timeout_secs` for each individual
    /// progress-chunk receive but not for the overall pull duration — a 20GB model pull
    /// can legitimately take many minutes. Uses inactivity detection: if the stream goes
    /// silent for `stream_inactivity_timeout_secs`, the pull is considered failed.
    #[allow(dead_code)] // Phase 9 — auto-pull missing models before first inference call
    pub async fn pull_model(&self, model_name: &str) -> Result<(), InferenceError> {
        let url = format!("{}{}", self.base_url, OLLAMA_PULL_PATH);

        #[derive(Serialize)]
        struct PullRequest<'a> {
            model: &'a str,
            stream: bool,
        }

        info!(model = %model_name, "Pulling model from Ollama registry");

        let response = self
            .client
            .post(&url)
            .json(&PullRequest {
                model: model_name,
                stream: true,
            })
            .send()
            .await
            .map_err(InferenceError::from)?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(InferenceError::ApiError { status, message });
        }

        let inactivity = Duration::from_secs(self.config.stream_inactivity_timeout_secs);
        let mut byte_stream = response.bytes_stream();
        let mut line_buf = String::new();

        loop {
            match timeout(inactivity, byte_stream.next()).await {
                Err(_) => {
                    return Err(InferenceError::StreamInterrupted(format!(
                        "pull stream silent for {}s (model: {})",
                        inactivity.as_secs(),
                        model_name
                    )));
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(InferenceError::from(e)),
                Ok(Some(Ok(bytes))) => match std::str::from_utf8(&bytes) {
                    Err(e) => {
                        return Err(InferenceError::SerializationError(format!(
                            "UTF-8 in pull stream: {e}"
                        )))
                    }
                    Ok(text) => {
                        line_buf.push_str(text);
                        while let Some(pos) = line_buf.find('\n') {
                            let line: String = line_buf.drain(..=pos).collect();
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }

                            if let Ok(progress) = serde_json::from_str::<OllamaPullProgress>(line) {
                                match (progress.completed, progress.total) {
                                    (Some(c), Some(t)) if t > 0 => {
                                        let pct = (c as f64 / t as f64 * 100.0) as u8;
                                        debug!(
                                            model = %model_name,
                                            status = %progress.status,
                                            pct,
                                            "Pull progress"
                                        );
                                    }
                                    _ => {
                                        debug!(
                                            model  = %model_name,
                                            status = %progress.status,
                                            "Pull progress"
                                        );
                                    }
                                }
                                if progress.status == "success" {
                                    info!(model = %model_name, "Pull complete");
                                    return Ok(());
                                }
                            }
                        }
                    }
                },
            }
        }

        // Stream ended without a `"success"` status line — treat as an interrupted pull.
        // This can happen if the connection drops mid-download.
        warn!(model = %model_name, "Pull stream ended without success status");
        Err(InferenceError::StreamInterrupted(format!(
            "pull for '{model_name}' ended without success confirmation"
        )))
    }
}

// ── Integration tests ─────────────────────────────────────────────────────────
//
// These tests require a live Ollama instance and are gated with `#[ignore]` so
// `cargo test` passes in CI. Run with:
//
//   cargo test -p dexter-core -- --ignored
// or:
//   make test-inference      (runs only the inference integration tests)
//
// All tests that require Ollama to be running are marked `#[ignore]`.
// AC-3 (offline new() succeeds) is NOT marked ignore — it must pass always.
//
// Prerequisites (see PHASE_4_PLAN.md §Prerequisites):
//   ollama serve          (Ollama running at http://localhost:11434)
//   ollama pull phi3:mini (only phi3:mini is required; larger models are optional)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InferenceConfig;

    fn test_config() -> InferenceConfig {
        InferenceConfig {
            ollama_base_url: "http://localhost:11434".to_string(),
            // 60s: phi3:mini must be loaded from disk on first request. Cold load on
            // Apple Silicon takes 20–40s. The production default is 30s (non-streaming
            // endpoint SLA), but tests need headroom for a cold model start.
            request_timeout_secs: 60,
            connect_timeout_secs: 3,
            stream_inactivity_timeout_secs: 30,
            auto_pull_missing_models: false,
        }
    }

    // ── Phase 20: Message.images field tests ──────────────────────────────────

    #[test]
    fn message_with_images_serializes_images_array_field() {
        // When images is Some, the JSON output must include an "images" key with
        // the base64 array. The Ollama /api/chat multimodal contract requires this.
        let msg = Message {
            role: "user".to_string(),
            content: "what do you see?".to_string(),
            images: Some(vec!["base64abc".to_string()]),
            origin: MessageOrigin::User,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains("\"images\""),
            "JSON must contain 'images' key when images is Some: {}",
            json
        );
        assert!(
            json.contains("base64abc"),
            "JSON must contain the base64 payload: {}",
            json
        );
    }

    #[test]
    fn message_without_images_skips_images_field_in_json() {
        // When images is None, the "images" key must be absent entirely (not "images":null).
        // skip_serializing_if = "Option::is_none" enforces this. Normal text messages
        // must not send an empty images array to Ollama — it confuses non-vision models.
        let msg = Message::user("hello");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            !json.contains("images"),
            "JSON must NOT contain 'images' key when images is None: {}",
            json
        );
    }

    #[test]
    fn message_user_with_image_constructor_creates_single_image_vec() {
        // user_with_image() must produce a user-role message with exactly one base64
        // string in the images vec. The orchestrator's vision path calls this.
        let b64 = "dGVzdGltYWdl".to_string(); // "testimage" in base64
        let msg = Message::user_with_image("analyze this", b64.clone());
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "analyze this");
        let images = msg.images.expect("images must be Some");
        assert_eq!(images.len(), 1, "must have exactly one image");
        assert_eq!(images[0], b64, "image payload must match input");
    }

    // AC-3: InferenceEngine::new() succeeds offline — no network contact is made.
    // This test MUST pass always (not #[ignore]).
    #[test]
    fn new_succeeds_without_ollama_running() {
        let result = InferenceEngine::new(test_config());
        assert!(
            result.is_ok(),
            "InferenceEngine::new() should not contact Ollama"
        );
    }

    #[tokio::test]
    async fn generate_stream_times_out_waiting_for_response_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            if let Ok((_socket, _peer)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        let mut cfg = test_config();
        cfg.ollama_base_url = format!("http://{addr}");
        cfg.request_timeout_secs = 1;
        cfg.stream_inactivity_timeout_secs = 1;
        let engine = InferenceEngine::new(cfg).unwrap();
        let req = GenerationRequest {
            model_name: "header-timeout-test".to_string(),
            messages: vec![Message::user("hello")],
            temperature: None,
            unload_after: false,
            keep_alive_override: None,
            num_predict: None,
            inactivity_timeout_override_secs: None,
            num_ctx_override: None,
        };

        let result = tokio::time::timeout(Duration::from_secs(3), engine.generate_stream(req))
            .await
            .expect("generate_stream should return via its own header timeout");

        match result {
            Err(InferenceError::StreamInterrupted(message)) => {
                assert!(
                    message.contains("before stream started"),
                    "wrong timeout boundary: {message}"
                );
            }
            other => panic!("expected pre-stream timeout, got {other:?}"),
        }
    }

    // AC-2: phi3:mini is present on disk with a non-zero size.
    // Requires: `ollama pull phi3:mini`
    #[tokio::test]
    #[ignore]
    async fn phi3_mini_is_available() {
        let engine = InferenceEngine::new(test_config()).unwrap();
        let models = engine
            .list_available_models()
            .await
            .expect("list_available_models should succeed when Ollama is running");

        let phi3 = models.iter().find(|m| m.name.starts_with("phi3:mini"));
        assert!(
            phi3.is_some(),
            "phi3:mini must be pulled before running inference tests"
        );
        assert!(
            phi3.unwrap().size_bytes > 0,
            "phi3:mini size_bytes should be non-zero"
        );
    }

    // AC-4: Single-turn generation with phi3:mini streams at least one non-empty token.
    // Requires: Ollama running + phi3:mini pulled.
    #[tokio::test]
    #[ignore]
    async fn generate_stream_yields_tokens() {
        let engine = InferenceEngine::new(test_config()).unwrap();
        let req = GenerationRequest {
            model_name: "phi3:mini".to_string(),
            messages: vec![Message::user("Say 'hello' and nothing else.")],
            temperature: Some(0.0),
            unload_after: false,
            keep_alive_override: None,
            num_predict: None,
            inactivity_timeout_override_secs: None,
            num_ctx_override: None,
        };

        let mut rx = engine
            .generate_stream(req)
            .await
            .expect("generate_stream should not error before streaming starts");

        let mut token_count = 0usize;
        let mut total_text = String::new();
        let mut saw_done = false;

        while let Some(result) = rx.recv().await {
            let chunk = result.expect("stream chunk should not be an error");
            if chunk.done {
                saw_done = true;
                assert!(
                    chunk.eval_count > 0,
                    "eval_count should be >0 on final chunk"
                );
                break;
            }
            if !chunk.content.is_empty() {
                token_count += 1;
                total_text.push_str(&chunk.content);
            }
        }

        assert!(
            token_count > 0,
            "should receive at least one non-empty token"
        );
        assert!(saw_done, "stream should terminate with a done=true chunk");
        assert!(
            !total_text.is_empty(),
            "accumulated text should not be empty"
        );
    }

    // AC-5: ModelNotFound is returned for a model that is not pulled.
    // Requires: Ollama running. The model "nonexistent-model:latest" must not be pulled.
    #[tokio::test]
    #[ignore]
    async fn generate_stream_errors_for_missing_model() {
        let engine = InferenceEngine::new(test_config()).unwrap();
        let req = GenerationRequest {
            model_name: "nonexistent-model-dexter-test:latest".to_string(),
            messages: vec![Message::user("hello")],
            temperature: None,
            unload_after: false,
            keep_alive_override: None,
            num_predict: None,
            inactivity_timeout_override_secs: None,
            num_ctx_override: None,
        };

        // The error may surface at generate_stream (if ensure_model_available is called
        // before streaming) or as the first chunk in the stream (if Ollama returns a 404).
        // Either path must produce a recognisable error — we accept both.
        let stream_result = engine.generate_stream(req).await;
        match stream_result {
            Err(InferenceError::ModelNotFound(_)) => { /* expected: pre-stream check */ }
            Err(InferenceError::ApiError { status: 404, .. }) => { /* expected: Ollama 404 */ }
            Err(other) => panic!("unexpected error variant: {other:?}"),
            Ok(mut rx) => {
                // The error arrives as the first chunk.
                let first = rx.recv().await.expect("should receive an error chunk");
                assert!(
                    first.is_err(),
                    "first chunk should be an error for missing model"
                );
            }
        }
    }

    // AC-6: embed() returns a non-empty vector for phi3:mini.
    // Requires: Ollama running + phi3:mini pulled.
    // Note: phi3:mini is not an embedding-specific model but Ollama accepts /api/embed
    // for any model as a capability test. For production, mxbai-embed-large is used.
    #[tokio::test]
    #[ignore]
    async fn embed_returns_non_empty_vector() {
        let engine = InferenceEngine::new(test_config()).unwrap();
        let req = EmbeddingRequest {
            model_name: "phi3:mini".to_string(),
            input: "This is a test embedding.".to_string(),
        };

        let embedding = engine
            .embed(req)
            .await
            .expect("embed should succeed with phi3:mini");

        assert!(
            !embedding.is_empty(),
            "embedding vector should be non-empty"
        );
        // Sanity check: embedding values should not all be zero.
        assert!(
            embedding.iter().any(|&v| v != 0.0),
            "embedding should have non-zero values"
        );
    }

    // AC-7: list_available_models returns a non-empty list when Ollama is running.
    // Requires: Ollama running (at least one model pulled).
    #[tokio::test]
    #[ignore]
    async fn list_available_models_returns_models() {
        let engine = InferenceEngine::new(test_config()).unwrap();
        let models = engine
            .list_available_models()
            .await
            .expect("list_available_models should succeed when Ollama is running");

        assert!(
            !models.is_empty(),
            "should have at least one model if Ollama is running"
        );

        // Every model should have a non-empty name and non-zero size.
        for model in &models {
            assert!(!model.name.is_empty(), "model name should not be empty");
            assert!(model.size_bytes > 0, "model size_bytes should be >0");
        }
    }
}
