/// Named constants for the Dexter core daemon.
///
/// All magic values live here. Nothing in `main.rs`, `server.rs`, or any future
/// module may contain bare string or integer literals that represent configurable
/// or named values — they import from this module instead.
///
/// Phase 2 rationale: scattering magic values across files means every reader has
/// to grep to find the canonical definition, and every change requires finding all
/// copies. A single authoritative source eliminates both problems.

/// Unix domain socket path for gRPC IPC between the Swift shell and the Rust core.
///
/// The Makefile's `SOCKET_PATH` variable must match this value.
/// `config.core.socket_path` defaults to this value at runtime but can be
/// overridden via `~/.dexter/config.toml` — use the config value, not this
/// constant directly, anywhere that reads the socket path at runtime.
pub const SOCKET_PATH: &str = "/tmp/dexter.sock";

/// How long `wait-for-core` polls the socket before declaring failure (seconds).
///
/// 30 seconds accommodates a cold `cargo build` on Apple Silicon on first run.
/// The Makefile's `SOCKET_TIMEOUT_SECS` variable must match this value.
///
/// This constant is the authoritative source for the timeout value; the Makefile
/// variable is a mirror. Rust code does not currently consume it directly — the
/// Makefile's `wait-for-core` target uses it via shell expansion, not via Rust.
#[allow(dead_code)]
pub const SOCKET_TIMEOUT_SECS: u64 = 30;

/// Semantic version of the Rust core binary, injected from `Cargo.toml` at compile time.
///
/// Using `env!` rather than a string literal means the binary's self-reported version
/// is always in sync with the package manifest — no drift possible.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// State directory path, relative to the user's home directory.
///
/// Resolved at runtime via `dirs::home_dir()` — this is the relative segment only.
/// Full path: `{home_dir}/{DEXTER_STATE_DIR}` (e.g. `/Users/operator/.dexter/state`).
/// Created on every startup in `main.rs` before the server binds.
pub const DEXTER_STATE_DIR: &str = ".dexter/state";

/// Config file name, relative to `{home_dir}/.dexter/`.
///
/// Full path: `{home_dir}/.dexter/{DEXTER_CONFIG_FILENAME}`.
pub const DEXTER_CONFIG_FILENAME: &str = "config.toml";

/// Default personality profile path, relative to the project root (cwd at runtime).
///
/// Read by Phase 5's PersonalityLayer. Written in Phase 2 so Phase 5 has no
/// bootstrapping ceremony — the file exists from day one of the project.
pub const PERSONALITY_CONFIG_PATH: &str = "config/personality/default.yaml";

// ── Ollama API ────────────────────────────────────────────────────────────────
//
// These constants are consumed by `inference::engine` (Phase 4, added in this phase).
// The #[allow(dead_code)] annotations are removed once engine.rs imports them.

/// Default Ollama base URL. Overridable via `[inference].ollama_base_url` in config.toml.
///
/// Localhost HTTP only — TLS is never needed for a local Ollama instance, and
/// reqwest is configured with `default-features = false` to omit TLS linkage.
#[allow(dead_code)] pub const OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Bidirectional chat endpoint. Supports both streaming (`stream: true`)
/// and non-streaming (`stream: false`) responses. Also used for model unloading
/// via `keep_alive: 0`.
#[allow(dead_code)] pub const OLLAMA_CHAT_PATH: &str = "/api/chat";

/// Embedding endpoint (Ollama ≥0.1.26).
/// Response: `{"embeddings": [[f32, ...]]}` — plural outer array, always length 1
/// for single-input requests. Do not use the legacy `/api/embeddings` (singular).
#[allow(dead_code)] pub const OLLAMA_EMBED_PATH: &str = "/api/embed";

/// Model inventory endpoint. Returns all downloaded models with metadata.
/// This answers "is the model on disk?" — not "is it loaded into VRAM?"
/// Use `/api/ps` (not implemented here) for the loaded-model query.
#[allow(dead_code)] pub const OLLAMA_TAGS_PATH: &str = "/api/tags";

/// Model download endpoint. Accepts a model name and streams NDJSON progress events.
/// Only called when `InferenceConfig.auto_pull_missing_models = true`.
#[allow(dead_code)] pub const OLLAMA_PULL_PATH: &str = "/api/pull";

/// Process-state endpoint. Returns the list of models currently loaded in VRAM/RAM,
/// including `size_vram` (bytes actually resident on GPU) vs `size` (total bytes the
/// model wants). When `size_vram < size` the model is partially spilled to CPU —
/// first-token latency will balloon from seconds to minutes. Used by the HEAVY
/// dispatch diagnostics (Phase 37.6) to distinguish "failed to load" from
/// "loaded partially" from "loaded but slow".
#[allow(dead_code)] pub const OLLAMA_PS_PATH: &str = "/api/ps";

/// Bounded mpsc channel capacity for inference event streams.
///
/// 32 slots: enough to buffer a burst of tokens between the inference task and the
/// gRPC sender task without stalling the generator, while keeping memory bounded.
/// Phase 6 may tune this based on observed back-pressure patterns.
#[allow(dead_code)] pub const INFERENCE_CHANNEL_CAPACITY: usize = 32;

// ── KV Cache Prefill (Phase 24) ────────────────────────────────────────────────

/// Ollama `keep_alive` duration for the FAST model tier (qwen3:8b).
///
/// "999m" ≈ 16.65 hours — effectively permanent for a session. The FAST model
/// must stay resident in VRAM so that KV cache prefilling (Solutions 1+2) can
/// maintain a warm prefix. Without pinning, Ollama's default TTL evicts the
/// model between interactions, destroying the cached KV entries.
///
/// VRAM budget: qwen3:8b ≈ 5GB. With embed (~0.7GB) and OS+processes (~8GB),
/// leaves ~22GB for PRIMARY (gemma4:26b MoE at ~18GB Q4_K_M, 3.8B active per token).
pub const FAST_MODEL_KEEP_ALIVE: &str = "999m";

/// Ollama `keep_alive` duration for the PRIMARY model tier (gemma4:26b).
///
/// Phase 37: "30m" — PRIMARY is now a 26B MoE (gemma4:26b, 18GB Q4_K_M, 3.8B active).
/// Pinning FAST (~5GB) + PRIMARY (~18GB) + EMBED (~0.7GB) ≈ 24GB of the 36GB unified
/// memory budget, leaving ~12GB for OS, Swift UI, Python workers, and on-demand
/// HEAVY/CODE swaps. 30 minutes of idle-retain keeps PRIMARY warm across typical
/// session breaks while still letting Ollama reclaim VRAM if the operator walks
/// away for half an hour.
///
/// Rationale for warming PRIMARY at startup:
/// Without warmup, the first PRIMARY-routed request (any iMessage/explain query)
/// pays a 30–120s cold-load penalty. The operator perceives this as a hard hang
/// — GENERATION_WALL_TIMEOUT_SECS fires before the first token arrives. Warming
/// PRIMARY in the background at session start trades ~15s of idle VRAM pressure
/// for a consistent sub-5s first-token latency on PRIMARY-routed queries.
pub const PRIMARY_MODEL_KEEP_ALIVE: &str = "30m";

/// Interval (seconds) between background PRIMARY "keep warm" pings.
///
/// Bug report: gemma4:26b's Ollama `keep_alive: "30m"` is set correctly on every
/// request, yet successive PRIMARY-routed turns still showed `load_duration ≈ 22s`
/// on the second query. Root cause: `keep_alive` is Ollama's **eviction** timer,
/// but Ollama does not `mlock` the mmap'd GGUF pages — so macOS's unified-memory
/// page reclamation can evict hot model weights under pressure (Swift UI, browser
/// workers, clipboard churn) even though Ollama still considers the model "loaded".
/// Next request does a warm-start from disk rather than a cold-load, but disk
/// re-read on USB-SSD (BitHappens) is the 20+ second penalty we see.
///
/// Fix: a cheap background request (`num_predict: 1`) every 60 seconds re-touches
/// the weight pages, pulling them back into resident memory and resetting macOS's
/// LRU clock. 60 s is comfortably under PRIMARY_MODEL_KEEP_ALIVE (1800 s) so
/// Ollama never sees an eviction window.
///
/// Tuning history (evidence-based, not guessed):
/// - First cut: 180 s. Live test on 2026-04-21 observed two cold-loads inside a
///   3m21s idle gap (keepalive ping at load_ms≈21600, subsequent turn at ≈22500).
/// - Second cut: 90 s. Live test on 2026-04-22 still showed two cold-loads.
/// - Third cut: 60 s. Assumed 25s margin against ≈85s post-heavy-gen floor.
/// - Fourth cut: 45 s. Live test on 2026-04-26 (Phase 38c live-smoke) showed
///   60 s still failed under sustained memory pressure (Claude.app + Swift +
///   daemon all competing for unified memory).
/// - **Current: 30 s.** Phase 38c lifted the per-session-warmup veil and exposed
///   that even 45 s wasn't enough — live-smoke session-035 saw FIVE consecutive
///   cold-loads on the keepalive task (load_ms ≈ 21000-22000 each, at 45s
///   intervals from 00:02:14 to 00:05:14) before the weights finally stayed
///   resident. Eviction is happening in <45s under realistic load. 30 s gives
///   ~15s margin against an observed ≈45s reclamation floor. Cost still trivial:
///   `num_predict: 1` is ~150 ms GPU time, so 30 s cadence = ~0.5 % duty cycle.
///
/// If 30 s still fails: the real fix is `mlock`-ing Ollama's weight pages
/// (requires patching Ollama) or moving to a supervisor that keeps a fixed slice
/// of VRAM carved out for PRIMARY. Going below 30 s starts to feel pathological;
/// at that point structural mitigation is more honest than chasing the cadence.
///
/// Cost: one tiny Ollama request every 30 seconds — negligible compared to the
/// 20+ second cold-load penalty it prevents. The ping uses the inference engine's
/// chat API with `num_predict: 1`, so it exercises the full load path; a pure
/// `/api/show` call would inform Ollama but not touch the weight pages.
pub const PRIMARY_KEEPALIVE_PING_INTERVAL_SECS: u64 = 30;

/// Minimum seconds between KV cache prefill requests (debounce).
///
/// AXElementChanged fires on every keystroke in a text editor. Without debounce,
/// the prefill would flood Ollama with requests. 5 seconds yields at most
/// 12 prefills/minute — each is a tiny request (1 output token), but the
/// debounce prevents unnecessary Ollama queue contention.
pub const PREFILL_DEBOUNCE_SECS: u64 = 5;

// ── Session state ──────────────────────────────────────────────────────────────

/// Schema version field written into every session state JSON file.
///
/// Increment this string whenever a breaking change is made to the `SessionState`
/// schema so that old files can be detected and handled gracefully on load.
pub const SESSION_STATE_SCHEMA_VERSION: &str = "1.0";

/// Prefix for session state filenames.
///
/// Full filename format: `{SESSION_FILENAME_PREFIX}{YYYYMMDD_HHMMSS}_{uuid8}.json`
/// e.g. `session_20260307_142201_a3f9b2c1.json`
pub const SESSION_FILENAME_PREFIX: &str = "session_";

/// Name of the symlink in the state directory that always points to the most recent
/// session file. Loaded by `SessionStateManager::load_latest()` and by `main.rs`
/// at startup for session bootstrap logging.
pub const SESSION_LATEST_SYMLINK: &str = "latest.json";

// ── Context Observer (Phase 7) ────────────────────────────────────────────────

/// Maximum characters of AXUIElement value included in a context snapshot.
///
/// Prevents large document contents (open files, emails, documents) from
/// bloating the JSON payload sent over the gRPC session stream. 200 chars
/// is enough to identify what the operator is looking at without transmitting
/// entire document contents.
pub const AX_VALUE_PREVIEW_MAX_CHARS: usize = 200;

/// Milliseconds to debounce AXFocusedUIElementChanged callbacks.
///
/// AX fires for every intermediate focus state during keyboard navigation
/// (arrow keys, Tab, click-drag). 150ms yields stable "user settled on element X"
/// signals rather than a flood of transient states.
///
/// Defined in Rust as the authoritative source. `EventBridge.swift` mirrors this
/// value in its own `contextDebouncMs` constant — keeping the value here prevents
/// the two sides from drifting independently.
#[allow(dead_code)] // authoritative source; consumed by EventBridge.swift, not Rust
pub const CONTEXT_DEBOUNCE_MS: u64 = 150;

// ── Action Engine (Phase 8) ───────────────────────────────────────────────────

/// Audit log filename written to `state_dir`. Append-only JSONL — one entry per line.
///
/// Never truncated or rotated in this phase; each line is one action record.
pub const AUDIT_LOG_FILENAME: &str = "audit.jsonl";

/// Maximum number of autonomous action steps Dexter will chain before stopping.
///
/// Each agentic step is: model generates → action dispatched → result injected →
/// continuation generation. After AGENTIC_MAX_DEPTH steps, the chain stops and
/// Dexter says he couldn't complete the task, preventing runaway infinite loops
/// from a confused model repeatedly retrying a broken action.
pub const AGENTIC_MAX_DEPTH: u8 = 6;

/// Maximum number of user+assistant exchange pairs retained in ConversationContext.
///
/// Round 3 / T1.1: originally 20. That budget (~4000 tokens of history) made the
/// system prompt dominate every request — qwen3:8b spent 500–900ms re-processing
/// stale dialog before emitting a token, and the cached KV prefix was frequently
/// invalidated because "20 turns ago" almost always changes turn-to-turn.
///
/// Round 3 / behavioral fix: T1.1 set this to 4, which was too aggressive — multi-step
/// agentic workflows (send message → "to whom?" → name → "what should I say?" → content)
/// need at least 5–6 turns of context, and iMessage/Contacts queries inject tool-result
/// turns that count against the budget.
///
/// Phase 36: 8 → 16. User testing surfaced a "memory cliff" at 8 turns — questions
/// like "what was the first thing I asked?" after 7-8 back-and-forth exchanges
/// returned confabulated answers because the initial turn had been evicted. Since
/// the FAST model is pinned and the KV cache benefit is dominated by system-prompt
/// stability (not tail stability), doubling the window has negligible latency cost
/// while closing the continuity gap. 16 pairs ≈ 3200 tokens of history: still well
/// under the qwen3:8b 32k context window, and retrieval still handles older facts.
pub const CONVERSATION_MAX_TURNS: usize = 16;

/// Phase 36: proactive-observation suppression window after a user interaction.
///
/// When the operator has spoken or typed within this many seconds, `ProactiveEngine`
/// blocks app-focus-driven ambient observations. Rationale: during active dialogue
/// the operator is engaged WITH Dexter, not just co-working; a time/weather comment
/// dropped between the operator's turn and Dexter's response reads as non-sequitur
/// noise. After 60s of silence, ambient observations resume normally.
///
/// Unrelated to `proactive_interval_secs` (the minimum gap BETWEEN observations) —
/// this is an interaction-activity gate that applies even when the observation
/// would otherwise pass the interval check. 60s matches the natural pause-threshold
/// between a completed turn and "I guess I'm done for now" ambient state.
pub const PROACTIVE_USER_ACTIVE_WINDOW_SECS: u64 = 60;

/// Vision-continuation window (seconds): how long after a successful vision turn
/// follow-up questions should re-route to the Vision tier with a fresh screen
/// capture, rather than falling through to Chat/FAST.
///
/// Background: vision routing in Dexter is single-shot — the image attaches only
/// when the router classifies the user's text as `Category::Vision`. Once a user
/// asks "take a look at my screen", the next turn ("how big is that?", "what
/// color is it?", "show me another") classifies as Chat and routes to FAST
/// (qwen3:8b), which is text-only. The model has no choice but to hallucinate
/// visual details from conversation context.
///
/// Fix: when a recent turn was a successful Vision route AND the new utterance
/// contains an anaphoric/visual-reference marker, we override the route back to
/// Vision so `capture_screen()` fires again and the image attaches to gemma4:26b.
/// Re-capturing (instead of caching bytes) means follow-ups about a *different*
/// image still work — the operator scrolls to a new image, says "what about this
/// one?", and the live screen state goes to the model.
///
/// Tuning: 300s (5 min) covers a normal multi-turn conversation about an image
/// while erring short enough that "what's it look like?" 20 minutes later — when
/// the conversation has clearly moved on — falls through to default routing.
/// Smaller windows risk losing legitimate follow-ups; larger windows risk
/// false-positive vision routes when the operator is no longer in image-mode.
pub const VISION_CONTINUATION_WINDOW_SECS: u64 = 300;

/// Wall-clock timeout for the entire generation pipeline (seconds).
///
/// If the full token stream has not completed within this window, the generation
/// task signals cancellation, sends an is_final text marker so Swift exits streaming
/// mode, and delivers a GenerationResult with cancelled=true. This handles the case
/// where qwen3:8b think-mode produces many silent frames that reset the per-chunk
/// inactivity timer but never produce visible output — the operator sees THINKING
/// indefinitely without this guard.
///
/// 90 seconds: generous for a FAST-tier response; heavy responses that are legitimately
/// producing tokens are guarded by `GENERATION_HARD_TIMEOUT_SECS` instead — this one
/// fires early ONLY when no visible output has appeared yet (classic stuck-in-think).
pub const GENERATION_WALL_TIMEOUT_SECS: u64 = 90;

/// Absolute hard ceiling on a single generation (seconds).
///
/// Unlike `GENERATION_WALL_TIMEOUT_SECS` — which fires only when the model has produced
/// NO visible output — this ceiling applies to every generation regardless of token flow.
/// Purpose: bound runaway outputs (models that loop, hallucinate long lists, keep
/// emitting tokens without ever reaching EOS) and pathological agentic turns that
/// stream slowly enough to keep the inactivity timer alive while exceeding operator
/// patience.
///
/// 180 seconds: roughly 3× the stuck-think threshold. A legitimate FAST/PRIMARY answer
/// finishes well under 30s on an M-series Mac; HEAVY deepseek-r1 can reason for ~60s.
/// Anything past 180s is either broken or not useful to a human-in-the-loop interaction.
pub const GENERATION_HARD_TIMEOUT_SECS: u64 = 180;

/// Default wall-clock timeout for action subprocess execution (seconds).
///
/// Prevents a runaway shell command or hung AppleScript from stalling the
/// orchestrator indefinitely. 30 seconds is generous for any interactive
/// action Dexter should take on behalf of the operator.
pub const ACTION_DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Extended timeout for known long-running download tools (yt-dlp, curl, wget, ffmpeg).
/// Video downloads routinely take 60–300s depending on file size and network speed.
pub const ACTION_DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// Timeout for AppleScript actions (`osascript`) specifically.
///
/// The default 30s is insufficient for batch reverse-phone lookups in Contacts.app.
/// An O(n × m) nested loop over hundreds of contacts × multiple handles routinely
/// exceeds 30s with AppleScript's per-call overhead (~5–10ms per Contacts API call).
/// 90s gives the script room to complete while still bounding runaway scripts.
pub const ACTION_APPLESCRIPT_TIMEOUT_SECS: u64 = 90;

/// Opening delimiter for action blocks embedded in model responses.
///
/// The personality system prompt instructs the model to wrap action JSON
/// in these tags. Post-generation parsing scans for this literal string.
pub const ACTION_BLOCK_OPEN: &str = "<dexter:action>";

/// Closing delimiter for action blocks. Must appear after ACTION_BLOCK_OPEN.
pub const ACTION_BLOCK_CLOSE: &str = "</dexter:action>";

/// Maximum characters of subprocess stdout stored in the audit log.
///
/// Full stdout is not stored — prevents large outputs (e.g., `find /`) from
/// bloating the JSONL file. 500 chars is enough to confirm what a command did.
pub const AUDIT_OUTPUT_PREVIEW_CHARS: usize = 500;

// ── Retrieval Pipeline (Phase 9) ──────────────────────────────────────────────

/// SQLite database filename in the state directory for Dexter's semantic memory.
///
/// Full path: `{home_dir}/{DEXTER_STATE_DIR}/{MEMORY_DB_FILENAME}`.
/// Created on first insert; never truncated or rotated.
pub const MEMORY_DB_FILENAME: &str = "memory.db";

/// Embedding dimension for mxbai-embed-large (the EMBED model tier).
///
/// Used to validate BLOB sizes on read. 1024 f32 values × 4 bytes = 4096-byte BLOBs.
/// Must be updated if the embedding model changes to a different output width.
pub const RETRIEVAL_EMBED_DIM: usize = 1024;

/// Maximum MemoryEntry results returned per semantic similarity search.
///
/// 3 hits keeps the injected context compact. At retrieval phase 9 scale (hundreds
/// of entries) this is the entire top of the ranked list with margin to spare.
pub const RETRIEVAL_MAX_MEMORY_HITS: usize = 3;

/// HTTP timeout for web content fetch (seconds).
///
/// 10 seconds is generous for a DuckDuckGo instant-answer API call or a single
/// HTML page fetch. Network failure beyond this returns a non-fatal Err and
/// the pipeline falls back to memory-only context.
pub const RETRIEVAL_WEB_TIMEOUT_SECS: u64 = 10;

/// HTTP timeout for the wttr.in weather fast-path (seconds).
///
/// Phase 37.8: weather queries (e.g. "what's the weather in Tokyo?") are routed
/// to RetrievalFirst by the classifier but DuckDuckGo's instant-answer API has
/// no live weather vertical, so they used to dead-end with the model emitting
/// "I don't have live weather data". `wttr.in` returns a one-line
/// `"{location}: {emoji} +{temp}°{unit}"` payload in plain text — typical RTT
/// 200–600 ms. The 4-second cap keeps a slow wttr response from stalling
/// retrieval longer than DDG would have. On timeout/failure the pipeline falls
/// back to the regular DDG path (which usually returns nothing for weather but
/// at least preserves prior behavior).
pub const RETRIEVAL_WTTR_TIMEOUT_SECS: u64 = 4;

/// Maximum characters extracted from a fetched web page before truncation.
///
/// Prevents a full Wikipedia article (hundreds of kB) from flooding the context
/// window. 4,000 chars is ~600 tokens — enough to answer most factual queries
/// without displacing the conversation history.
pub const RETRIEVAL_MAX_CONTENT_CHARS: usize = 4_000;

/// Text sent to the operator while retrieval runs as a non-final TextResponse.
///
/// Appears immediately before retrieval begins, masking the embed + search + web
/// fetch latency. The UI renders this as a prefix to the substantive response.
pub const RETRIEVAL_ACKNOWLEDGMENT: &str = "Looking that up...\n\n";

/// Substring the model uses to express factual uncertainty.
///
/// Post-generation scan: if `response_text.contains(UNCERTAINTY_MARKER)`, trigger
/// retrieval and generate a follow-up grounded response. The personality system
/// prompt instructs the model to use this exact phrase when uncertain about facts
/// so that the retrieval gate fires consistently.
pub const UNCERTAINTY_MARKER: &str = "I'm not certain about";

// ── Voice Worker Bridge (Phase 10) ────────────────────────────────────────────

/// Path to the STT worker script, relative to daemon working directory (project root).
pub const VOICE_STT_WORKER_PATH: &str = "src/python-workers/workers/stt_worker.py";

/// Path to the TTS worker script, relative to daemon working directory (project root).
pub const VOICE_TTS_WORKER_PATH: &str = "src/python-workers/workers/tts_worker.py";

/// Python executable used to launch voice workers.
/// Points to the uv-managed venv created by `uv sync` in src/python-workers/.
/// Path is relative to daemon working directory (project root).
pub const VOICE_PYTHON_EXE: &str = "src/python-workers/.venv/bin/python3";

/// How often Rust sends HEALTH_PING to an idle worker (seconds).
pub const VOICE_WORKER_HEALTH_INTERVAL_SECS: u64 = 5;

/// How long Rust waits for HEALTH_PONG before declaring the worker dead (seconds).
pub const VOICE_WORKER_HEALTH_TIMEOUT_SECS: u64 = 3;

/// Timeout for the initial worker handshake read.
///
/// Separate from VOICE_WORKER_HEALTH_TIMEOUT_SECS because workers that load
/// heavyweight models write their handshake AFTER model initialization
/// (e.g. TTS loads kokoro ~3s, STT loads faster-whisper ~5s on cold start).
/// 30 seconds covers cold loads; after handshake the tighter health timeout applies.
pub const VOICE_WORKER_STARTUP_TIMEOUT_SECS: u64 = 30;

/// Maximum consecutive restart attempts before entering permanent degraded mode.
pub const VOICE_WORKER_RESTART_MAX_ATTEMPTS: u32 = 3;

/// Initial restart backoff (seconds). Doubles after each failed attempt.
pub const VOICE_WORKER_RESTART_BACKOFF_SECS: u64 = 1;

/// Minimum characters in accumulated text before a punctuation boundary triggers TTS.
/// Guards against splitting on "Mr. Smith" or "e.g. a case".
pub const TTS_SENTENCE_MIN_CHARS: usize = 10;

/// Phase 38 / Codex finding [14]: per-frame read timeout for TTS worker frames.
///
/// Wraps each `client.read_frame()` in the TTS read loops so a stalled or
/// deadlocked kokoro synth thread doesn't park the orchestrator forever. A
/// reasonable kokoro synthesis emits PCM frames every 50–200 ms during
/// generation — 30 s gives massive headroom while still bounding the worst
/// case. Pre-Phase-38 a hung kokoro could block generation completion, action
/// dispatch, and state transitions indefinitely.
///
/// On timeout the read loop breaks and the TTS task exits cleanly. The Python
/// worker's try/except/finally (Phase 38 / Codex [14] Python half) ALSO emits
/// MSG_TTS_DONE on synthesis exception so this Rust timeout is defense-in-depth
/// for the deadlock case (where Python doesn't even get to the except clause).
pub const TTS_FRAME_READ_TIMEOUT_SECS: u64 = 30;

/// faster-whisper model name loaded by stt_worker.py.
#[allow(dead_code)] // informational — value lives in Python; Rust constant is the source of truth
pub const STT_WHISPER_MODEL: &str = "base.en";

/// kokoro voice name used by tts_worker.py.
#[allow(dead_code)] // informational — value lives in Python; Rust constant is the source of truth
pub const TTS_KOKORO_VOICE: &str = "af_heart";

/// IPC wire protocol version. Must match Python workers/protocol.py PROTOCOL_VERSION.
pub const VOICE_PROTOCOL_VERSION: u32 = 1;

// ── Browser Worker (Phase 14) ─────────────────────────────────────────────────

/// Path to the browser worker script, relative to daemon working directory (project root).
pub const BROWSER_WORKER_PATH: &str = "src/python-workers/workers/browser_worker.py";

/// Timeout for a single browser command (navigate/click/type/extract/screenshot) in seconds.
///
/// Navigation can be slow on large pages. 30s matches ACTION_DEFAULT_TIMEOUT_SECS.
pub const BROWSER_WORKER_RESULT_TIMEOUT_SECS: u64 = 30;

/// How often Rust health-checks the browser worker (seconds).
///
/// Browser worker is idle most of the time — a 60s check interval is sufficient
/// to detect crashes without generating unnecessary subprocess traffic.
pub const BROWSER_WORKER_HEALTH_INTERVAL_SECS: u64 = 60;

// ── System Memory (Phase 15) ─────────────────────────────────────────────────

/// Shell command used to sample macOS virtual memory statistics.
/// Output format: "Pages free: N." etc. Page size on the first line.
pub const VM_STAT_CMD: &str = "vm_stat";

/// Maximum context window (tokens) for HEAVY-tier (`deepseek-r1:32b`) requests.
///
/// Phase 37.6 / Cluster-E (B15): Ollama's default `num_ctx` for deepseek-r1 is
/// the model's max training context (131,072 tokens). At that size the KV cache
/// alone is ~32 GiB, and Ollama happily allocates it — pushing total memory
/// required to ~60 GiB against the M4 Max's 28 GiB of GPU-visible unified memory.
/// Ollama then splits the model 21/65 layers GPU / 44/65 layers CPU (56% CPU
/// spill), making per-token generation an order of magnitude slower than
/// advertised. First-token latency balloons from a few seconds to well over
/// 30 s and the inactivity timer trips before anything arrives on the stream.
///
/// Capping `num_ctx` to 8,192 shrinks the KV cache by 16× (~2 GiB instead of
/// ~32 GiB). The model's 18.48 GiB of weights + 2 GiB KV + ~0.5 GiB compute
/// graph fits comfortably in VRAM with zero CPU spill, and first-token
/// latency drops back to ~3–5 s.
///
/// 8,192 is sufficient for Dexter's HEAVY workload: offensive-security / red-team
/// reasoning prompts are single-turn, system-prompt + question + recent history,
/// rarely exceeding ~3,000 tokens. Bumping past 8k re-introduces CPU spill
/// proportionally. Tune up (16,384) only if a concrete prompt is observed to
/// truncate.
///
/// Applied to any tier where `ModelId::needs_context_cap()` is true — currently
/// Heavy (deepseek-r1:32b, 131k native) and Code (deepseek-coder-v2:16b, 163k
/// native). FAST and PRIMARY retain Ollama's model-trained default. Phase 37.7:
/// renamed from HEAVY_NUM_CTX when the predicate for applying the cap was
/// decoupled from `unload_after` (which covers only Heavy).
pub const LARGE_MODEL_NUM_CTX: u32 = 8_192;

/// Minimum available (free + inactive) headroom in GiB before a HEAVY model
/// inference request triggers a warning log entry.
///
/// deepseek-r1:32b requires ~20GB. Ollama evicts the current resident model
/// before loading HEAVY — this threshold guards against situations where
/// free + inactive < the model's footprint, indicating likely swap pressure.
/// This is a warning threshold only — it never blocks inference.
pub const MEMORY_HEAVY_WARN_THRESHOLD_GB: f64 = 20.0;

// ── Memory (Phase 21) ────────────────────────────────────────────────────────

/// VectorStore `source` value for automatically-embedded conversation turns.
pub const MEMORY_SOURCE_CONVERSATION: &str = "memory";

/// VectorStore `source` value for operator-specified explicit facts ("remember X").
pub const MEMORY_SOURCE_OPERATOR: &str = "operator";

/// Minimum cosine similarity score for a recalled memory entry to be injected
/// into inference context. Below this threshold: the entry is ignored.
///
/// 0.65 is chosen to pass obviously relevant entries while excluding loosely
/// related entries that would add noise without useful grounding.
pub const MEMORY_RECALL_THRESHOLD: f32 = 0.65;

/// Maximum number of memory entries injected into a single inference request.
pub const MEMORY_RECALL_TOP_N: usize = 3;

/// Maximum characters of text submitted to the embedding model per request.
///
/// `mxbai-embed-large` ships on Ollama with a 512-token context window (the
/// model's trained positional range). Inputs beyond that are rejected by
/// Ollama with `400 "the input length exceeds the context length"`, causing
/// the turn to be silently dropped from long-term memory.
///
/// 1800 chars ≈ 450–500 tokens on English prose (3.5–4 chars/token), giving
/// safe headroom below the 512-token ceiling even on code-heavy text where
/// the char:token ratio skews lower. Truncation is head-preserving — for a
/// `User: Q\nAssistant: A` turn the question (which carries the most
/// distinctive retrieval handle) is always retained; only the tail of a long
/// answer is dropped. The full answer is still stored in session history and
/// in the VectorStore's `content` column — only the *embedding vector* is
/// computed from the truncated prefix.
pub const MEMORY_EMBED_MAX_CHARS: usize = 1_800;

/// Minimum cosine similarity for a local knowledge entry (operator fact or cached
/// web page) to satisfy a retrieval query without falling through to a network fetch.
///
/// Set above MEMORY_RECALL_THRESHOLD (0.65) because this is a binary skip-web
/// decision — the local entry must be semantically close enough to be trusted as a
/// direct answer, not merely contextually relevant. 0.82 ≈ 34° angular distance;
/// tight enough to confirm topical match, loose enough to tolerate phrasing variation
/// (e.g., "Python version" vs "Python release number").
pub const LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD: f32 = 0.82;

// ── Vision Screen Capture (Phase 20) ─────────────────────────────────────────

/// Path prefix for per-invocation screen capture temp files.
///
/// `capture_screen()` appends a UUID suffix to produce a unique path per call:
/// `/tmp/dexter_screen_<uuid>.png`. The prefix lives here as a named constant
/// so the directory and filename base can be changed in one place; the UUID
/// suffix is generated at call time via `uuid::Uuid::new_v4().as_simple()`.
///
/// Per-invocation paths eliminate the race condition that a fixed path would
/// introduce if two vision queries were processed concurrently (the second
/// write would clobber the first before it was read and encoded).
pub const SCREEN_CAPTURE_PATH_PREFIX: &str = "/tmp/dexter_screen";

/// Maximum wall-clock seconds allowed for a `screencapture` subprocess to complete.
///
/// screencapture on Apple Silicon typically completes in <1s. 5 seconds is generous
/// headroom for any momentary system load spike. On timeout, the vision query
/// falls back to text-only generation (no image attachment) rather than blocking.
pub const SCREEN_CAPTURE_TIMEOUT_SECS: u64 = 5;

// ── Clipboard Context (Phase 28) ─────────────────────────────────────────────

/// Maximum characters of clipboard text stored in ContextObserver and forwarded
/// over gRPC. Clipboard content is operator-initiated (explicit copy action).
/// 4,000 chars ≈ 600 tokens — meaningful code/text content without overwhelming
/// the context window or displacing conversation history.
///
/// Mirrors RETRIEVAL_MAX_CONTENT_CHARS: both represent third-party text injected
/// into the inference context; the same ceiling applies.
///
/// Defined in Rust as the authoritative source. EventBridge.swift mirrors this
/// value in its own `clipboardMaxChars` constant — keeping the value here prevents
/// the two sides from drifting independently.
pub const CLIPBOARD_MAX_CHARS: usize = 4_000;


/// Clipboard content is only auto-injected into inference context when the
/// clipboard was updated within this many seconds. Beyond this window, clipboard
/// is only injected when the user explicitly references it (keywords like "copy",
/// "clipboard", "paste", "what I copied").
///
/// 30 seconds: natural copy-then-ask workflow ("I copied a URL, what is this?")
/// typically completes within 30s. Stale clipboard (e.g. cookies from hours ago)
/// should NOT be injected into every query — it confuses small models into
/// treating the clipboard content as the answer.
pub const CLIPBOARD_RECENCY_SECS: i64 = 30;

/// NSPasteboard.changeCount polling interval in milliseconds.
///
/// 1,000ms = 1 second: imperceptible for the copy → speak workflow while keeping
/// polling overhead trivial (one integer comparison per second). The delay between
/// copying content and the operator speaking their question is naturally > 1s.
///
/// Authoritative source here; consumed by EventBridge.swift as `clipboardPollIntervalMs`.
#[allow(dead_code)] // authoritative source; consumed by EventBridge.swift, not Rust
pub const CLIPBOARD_POLL_INTERVAL_MS: u64 = 1_000;

// ── Shell Context (Phase 30) ──────────────────────────────────────────────────

/// Unix domain socket path for the shell integration hook.
///
/// The zsh hook (config/shell_integration.zsh) connects here after each command
/// completes. Each notification is a separate connect → write JSON → EOF → close
/// connection. Override with the DEXTER_SHELL_SOCKET env var for testing.
pub const SHELL_SOCKET_PATH: &str = "/tmp/dexter-shell.sock";

/// Minimum shell command length (chars) to store in context.
///
/// Filters out bare Enter presses (empty string) and single-character typos/aliases
/// (e.g. `l`, `s`). 2 chars admits `cd`, `ls`, `vi`, etc. — useful short commands.
pub const SHELL_CMD_MIN_CHARS: usize = 2;

/// Maximum shell command length (chars). Commands longer than this are truncated.
///
/// Guards against `cat bigfile | grep pattern` pipelines where the file's content
/// appears in the command string. 500 chars is enough for the longest realistic
/// command while keeping the injected [Shell: ...] message compact.
pub const SHELL_CMD_MAX_CHARS: usize = 500;

/// Maximum CWD length (chars). Paths longer than this are truncated.
///
/// Unusually deep paths (e.g. `node_modules` trees) can exceed 200 chars, but
/// the model only needs the tail of the path to understand context.
pub const SHELL_CWD_MAX_CHARS: usize = 200;

/// Shell context is injected into inference only when the stored command is fresher
/// than this many seconds. A command run 6+ minutes ago is unlikely to be relevant
/// to the current conversation; omitting it prevents misleading grounding.
pub const SHELL_CONTEXT_MAX_AGE_SECS: u64 = 300; // 5 minutes
