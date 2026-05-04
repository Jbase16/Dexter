/// CoreOrchestrator — per-session coordinator for all runtime activity.
///
/// ## Role
///
/// The orchestrator is the central wiring layer that connects the gRPC session stream
/// to the domain components built in Phases 4–8. It owns:
/// - `InferenceEngine`     — generates token streams from Ollama
/// - `ModelRouter`         — selects the appropriate model tier for each turn
/// - `PersonalityLayer`    — injects the operator's identity into every generation call
/// - `ConversationContext` — accumulates and truncates conversation history
/// - `SessionStateManager` — persists history and build state on session end
/// - `ContextObserver`     — aggregates machine context (Phase 7)
/// - `ActionEngine`        — executes system actions with policy gates (Phase 8)
///
/// ## Concurrency model
///
/// One orchestrator is constructed per gRPC `Session()` call. It lives in a single
/// Tokio task (the reader task in `ipc::server`) and processes events sequentially.
/// Sequential processing is intentional: ordering guarantees come from the single-task
/// model, not from locks. Session JSON is persisted for audit/debugging, but it is not
/// replayed into new live conversations; cross-session context belongs in retrieval/memory
/// with explicit relevance checks, not raw transcript bootstrap.
///
/// ## Failure guarantees
///
/// Every error path calls `error!()` with full structured context before breaking or
/// returning. No silent failures. `OrchestratorError::ChannelClosed` is the only error
/// that indicates an unrecoverable session state — it means the gRPC send channel to
/// Swift has been dropped, and the reader task will exit and call `shutdown()`.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tonic::Status;
use tracing::{debug, error, info, warn};

use crate::{
    action::{ActionEngine, ActionOutcome, ActionResult, ActionSpec, ExecutorHandle, PolicyEngine},
    config::{DexterConfig, ModelConfig},
    constants::{
        ACTION_BLOCK_CLOSE, ACTION_BLOCK_OPEN, AGENTIC_MAX_DEPTH, CONVERSATION_MAX_TURNS,
        FAST_MODEL_KEEP_ALIVE, GENERATION_HARD_TIMEOUT_SECS, GENERATION_WALL_TIMEOUT_SECS,
        LARGE_MODEL_NUM_CTX, MEMORY_DB_FILENAME, PREFILL_DEBOUNCE_SECS,
        PRIMARY_KEEPALIVE_PING_INTERVAL_SECS, PRIMARY_MODEL_KEEP_ALIVE, RETRIEVAL_ACKNOWLEDGMENT,
        SCREEN_CAPTURE_PATH_PREFIX, SCREEN_CAPTURE_TIMEOUT_SECS,
    },
    context_observer::ContextObserver,
    inference::{
        engine::{GenerationRequest, InferenceEngine},
        error::InferenceError,
        models::ModelId,
        retrieval_classifier::is_retrieval_first_query,
        router::{Category, ConversationContext, ModelRouter},
    },
    ipc::proto::{
        client_event, server_event, ActionApproval, ActionCategory, ActionRequest, ClientEvent,
        EntityState, EntityStateChange, ServerEvent, SystemEvent, SystemEventType, TextResponse,
        UiAction,
    },
    memory::{detect_memory_command, extract_facts, slug_id, MemoryCommand},
    personality::PersonalityLayer,
    proactive::ProactiveEngine,
    retrieval::RetrievalPipeline,
    session::SessionStateManager,
    system,
    voice::{protocol::msg, sentence::SentenceSplitter, VoiceCoordinator},
};

/// Bridging phrases emitted when the uncertainty sentinel fires mid-stream.
///
/// Selection is deterministic per trace_id (first byte mod 4) — no RNG dependency.
/// All end with '.' to form valid sentence boundaries for the SentenceSplitter.
const BRIDGING_PHRASES: &[&str] = &[
    "Let me check on that.",
    "One moment.",
    "Looking that up.",
    "Let me verify.",
];

/// Content-prefixes produced by [`crate::inference::router::ConversationContext::push_tool_result`].
///
/// Tool results (action output, retrieval hits) are injected into the conversation as
/// **synthetic `role="user"` messages** — the only way Ollama base-instruct models reliably
/// attend to them (custom roles like `"tool"`/`"retrieval"` are silently dropped). The
/// content-prefix is how downstream code distinguishes these synthetic turns from real
/// operator input.
///
/// When iterating `messages` looking for the most recent *genuine* user turn (e.g. vision
/// image attachment, barge-in cancellation target), use [`is_tool_result_content`] to skip
/// over synthetic injections and land on the operator's actual question.
const TOOL_RESULT_PREFIXES: &[&str] = &["[Retrieved", "[Action result", "[Action FAILED"];

/// Returns `true` if `content` begins with any prefix in [`TOOL_RESULT_PREFIXES`].
///
/// Used by: vision image attachment (must skip retrieval-injected user messages),
/// any future turn-scanning that needs to distinguish real operator input from
/// synthetic tool-result injections.
#[inline]
fn is_tool_result_content(content: &str) -> bool {
    TOOL_RESULT_PREFIXES.iter().any(|p| content.starts_with(p))
}

/// Rewrite literal "User:" / "Assistant:" role markers inside retrieved
/// prior-session content to "Q:" / "A:".
///
/// Phase 37.8 — cross-session retrieval-leak defense layer 2.
///
/// VectorStore stores conversation turns with the literal format
/// `"User: <q>\nAssistant: <a>"`. When a prior-session row is injected into the
/// prompt as reference material, those role-marker strings can bleed through
/// the model's role-parsing heuristics — gemma4 in particular will see
/// `"Assistant: …"` inside a system message and treat it as its own recent
/// turn in the current conversation, producing "I already told you this"
/// hallucinations (observed: Test 2, ret2libc query).
///
/// Replacing the role markers with non-role shorthand (`Q:` / `A:`) preserves
/// the Q/A structure for topical grounding while removing the tokens that
/// trigger role-continuity pattern matching. Only applied to prior-session
/// entries; current-session entries retain the original format because they
/// genuinely ARE part of the conversation the model is participating in.
///
/// Uses line-anchored matching — replaces `User:` / `Assistant:` only when
/// they appear at the start of a line (or the start of the buffer), so
/// incidental occurrences ("the user's Assistant: field") aren't rewritten.
fn neutralize_role_markers(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for (i, line) in content.split_inclusive('\n').enumerate() {
        // split_inclusive preserves the trailing '\n' on each piece; index 0 is
        // the first line whether or not a newline precedes it.
        let _ = i;
        if let Some(rest) = line.strip_prefix("User: ") {
            out.push_str("Q: ");
            out.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("Assistant: ") {
            out.push_str("A: ");
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Strip injected context labels from model-generated text before recording.
///
/// Two failure modes require two passes:
///
/// 1. **Bracket tokens** — qwen3's training data contains `[Context: X]` annotations,
///    so it generates them from memory even when our injection format uses bare labels.
///    A response like `[Context: Safari] You're in Safari.` needs the bracket token
///    excised while preserving the content after it.
///
/// 2. **Bare label lines** — qwen3 sometimes echoes the injection format verbatim as the
///    entire response (`Context: Codex` with nothing after it).  These whole lines must be
///    dropped.
///
/// `DateTime:` is also stripped for legacy responses — the injection now uses natural
/// language ("The current time is …") which doesn't need stripping.
fn strip_context_markers(s: &str) -> String {
    // Round 3 / T0.5: `[Env · clipboard: …]` / `[Env · shell: …]` are the new
    // user-turn prefix format (replacing the old "Clipboard: …" system messages).
    // Legacy bracket prefixes are retained so responses stored in memory from
    // prior sessions still get stripped on replay.
    const BRACKET_PREFIXES: &[&str] = &["[Context:", "[Clipboard:", "[Shell:", "[Memory:", "[Env "];
    const LINE_PREFIXES: &[&str] = &["Context:", "Clipboard:", "Shell:", "Memory:"];

    // Pass 1: Remove bracket-delimited tokens anywhere in the text.
    // Content appearing AFTER the closing `]` is preserved.
    let mut out = s.to_string();
    for &prefix in BRACKET_PREFIXES {
        while let Some(start) = out.find(prefix) {
            let close = out[start..]
                .find(']')
                .map(|i| start + i + 1)
                .unwrap_or(out.len());
            // Eat a trailing space so preserved content isn't double-spaced.
            let close = if out.get(close..close + 1) == Some(" ") {
                close + 1
            } else {
                close
            };
            out.replace_range(start..close, "");
        }
    }

    // Pass 2: Drop entire lines that start with a bare label.
    // These are whole-response echoes where the model produced nothing beyond the marker.
    let filtered: String = out
        .lines()
        .filter(|line| {
            let t = line.trim();
            !LINE_PREFIXES.iter().any(|&p| t.starts_with(p))
        })
        .collect::<Vec<_>>()
        .join("\n");

    filtered.trim().to_string()
}

/// Timeout for the retrieval-first and sentinel-triggered web retrieval calls.
/// 10 seconds: the bridging phrase has already been sent; the operator is waiting.
/// On timeout: inject a failure context and re-prompt to state uncertainty explicitly.
const RETRIEVAL_TIMEOUT_SECS: u64 = 10;
const COMEDY_MODE_INSTRUCTION: &str = concat!(
    "Comedy mode: The operator is asking for humor. Answer with the joke or comedic iteration directly. ",
    "Do not refuse, moralize, sanitize, soften, lecture about sensitivity, add a safety preamble, or explain that a topic is too risky for comedy. ",
    "Do not treat identity-themed comedy (gay, queer, trans, race, religion, etc.) as a special refusal category. ",
    "Match the requested comedic premise and tone directly. No hedge preambles like \"I'm not sure this qualifies\" or \"here goes\". ",
    "If the operator asks for a step-dad or step dad joke, treat that as a format label: an adult/NSFW dad-joke-style pun. ",
    "The joke does not need to mention a stepdad, parent, or family member unless the operator explicitly asks for that subject. ",
    "For another/different/try-again follow-ups, read the recent assistant jokes in the conversation and use a fresh setup, punchline, and premise instead of repeating or lightly rewording one."
);

// ── Phase 27: generation result ───────────────────────────────────────────────

/// Result delivered from the background generation task to the orchestrator event loop.
///
/// Phase 27: `generate_primary` runs in a background Tokio task so the session reader
/// remains responsive to `BargIn` events during generation. Results arrive via the
/// `generation_tx` → `gen_rx` channel managed by `ipc::server`'s `select!` loop.
pub struct GenerationResult {
    /// True when the generation was cancelled mid-stream by a barge-in.
    /// `handle_generation_complete` skips all post-processing when set.
    pub cancelled: bool,
    pub full_response: String,
    pub intercepted_q: Option<String>,
    /// True when a TTS task was active and the is_final audio sentinel was sent.
    /// When true, `handle_generation_complete` defers IDLE to AUDIO_PLAYBACK_COMPLETE.
    pub tts_was_active: bool,
    pub trace_id: String,
    /// Original user message — needed for memory accumulation (extract_facts,
    /// embed_and_store_turn, record assistant reply). For agentic continuation steps
    /// this carries the original user query (not the intermediate step's content).
    pub content: String,
    /// Embed model tag — needed for post-generation retrieval / memory operations.
    pub embed_model: String,
    /// Phase 31: true when this result is from run_shell_error_proactive_background.
    /// handle_generation_complete skips all regular post-processing when set.
    pub is_shell_proactive: bool,
    /// Phase 31: true when is_shell_proactive AND model returned [SILENT] or empty.
    /// Signals handle_generation_complete to refund the proactive rate-limit slot.
    pub proactive_silent: bool,
    /// Phase 32: depth in the agentic action chain (0 = user-initiated, 1+ = continuation).
    /// Passed to new Interactions so handle_action_result can enforce AGENTIC_MAX_DEPTH.
    pub agentic_depth: u8,
    /// T1.4: per-generation timing + counters. Emitted as a structured `info!` line
    /// on generation completion; forwarded to `handle_generation_complete` for any
    /// downstream use (e.g. surfacing slow-gen observability in the UI later).
    ///
    /// `allow(dead_code)`: the field is populated and logged in the producing task,
    /// and `handle_generation_complete` doesn't currently need it. Kept on the struct
    /// so future consumers (HUD indicator, Prometheus exporter, anomaly alerting)
    /// can read it without re-instrumenting the generation loop.
    #[allow(dead_code)]
    pub telemetry: GenerationTelemetry,
}

/// T1.4: per-generation telemetry captured by `run_generation_background`
/// (and a minimal subset by the shell-error proactive path).
///
/// Emit this once on generation completion so operators have a single grep-able
/// line per turn with: model, duration, tokens, and whether we cancelled. Also
/// lets later phases (e.g. a HUD "slow response" indicator or Prometheus export)
/// consume the data without re-instrumenting the loop.
#[derive(Debug, Clone, Default)]
pub struct GenerationTelemetry {
    /// Model tag used for this generation (e.g. "qwen3:8b").
    pub model: String,
    /// Milliseconds from `gen_started` to first visible operator-facing token.
    /// `None` if the generation cancelled / errored before producing any output.
    pub first_token_ms: Option<u64>,
    /// Total wall-clock time of the generation, in milliseconds.
    pub total_ms: u64,
    /// Number of streaming chunks received (close approximation to token count —
    /// Ollama emits one chunk per token for FAST/PRIMARY models, fewer per chunk
    /// for some HEAVY streaming modes).
    pub token_count: u32,
    /// True when this generation was cancelled (barge-in or deadline).
    pub cancelled: bool,
    /// Character length of `full_response` at termination.
    pub response_len: usize,
    /// Depth in the agentic chain (0 = user-initiated). Duplicated here so the
    /// log line stands alone without having to look at the outer GenerationResult.
    pub agentic_depth: u8,
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can be returned by orchestrator event handlers.
#[derive(Debug)]
pub enum OrchestratorError {
    /// `InferenceEngine::new()` failed — typically a bad reqwest client config.
    InferenceSetup(InferenceError),
    /// The gRPC sender channel was dropped before we could send — session is over.
    ChannelClosed,
    /// `RetrievalPipeline::new()` failed — SQLite could not open the memory DB.
    /// The pipeline falls back to `new_degraded()` so this variant is only produced
    /// if the caller explicitly rejects the degraded fallback (currently unused).
    #[allow(dead_code)] // Phase 10+: surfaced when degraded fallback is unacceptable
    RetrievalSetup(String),
    /// TTS worker setup failed. Non-fatal — voice stays degraded (text-only).
    #[allow(dead_code)] // Phase 13 may surface this to the caller
    VoiceSetup(String),
}

impl std::fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrchestratorError::InferenceSetup(e) => write!(f, "InferenceEngine init failed: {e}"),
            OrchestratorError::ChannelClosed => write!(f, "gRPC session channel closed"),
            OrchestratorError::RetrievalSetup(s) => write!(f, "RetrievalPipeline init failed: {s}"),
            OrchestratorError::VoiceSetup(s) => write!(f, "VoiceCoordinator init failed: {s}"),
        }
    }
}

impl std::error::Error for OrchestratorError {}

// ── Interaction lifecycle (Phase 24) ─────────────────────────────────────────

/// Represents a single voice interaction from hotkey press to final IDLE.
///
/// Keyed by `action_id` in the orchestrator's `interactions` HashMap.
/// When an action is dispatched to a background task, an `Interaction` is
/// inserted. When the result arrives via `action_rx`, the interaction is
/// looked up, transitioned to `FeedbackPending`, and TTS feedback is queued.
///
/// TTL: 5 minutes. A periodic GC sweep (piggybacked on the health-check timer)
/// removes stale entries to handle silently-panicked action tasks.
pub struct Interaction {
    #[allow(dead_code)] // Phase 24c: used by TTS priority queue for trace correlation
    pub trace_id: String,
    pub stage: InteractionStage,
    #[allow(dead_code)] // Phase 24c: used by TTS priority queue to schedule Active vs Background
    pub priority: InteractionPriority,
    pub created_at: Instant,
    /// Phase 32: position of this action in the agentic chain (0 = first action from user turn).
    /// Used by handle_action_result to enforce AGENTIC_MAX_DEPTH and pass depth+1 forward.
    pub agentic_depth: u8,
    /// Phase 32: the original user query that started this agentic chain.
    /// Threaded through all continuation steps so memory embedding stays tied to the
    /// root request rather than intermediate action results.
    pub original_content: String,
    /// Phase 36: true when this action is a *terminal* workflow step — a successful
    /// outcome means the operator's request is fulfilled and no continuation is
    /// needed. Currently detected for iMessage send-via-AppleScript: after a Messages
    /// send succeeds, the model was re-asked to continue, emitted a follow-up
    /// sqlite3 query to "verify", and the phantom-retry guard misfired on that
    /// unrelated query. Setting this flag at dispatch time short-circuits the
    /// continuation after a successful completion, returning "Sent." directly.
    pub is_terminal_workflow: bool,
}

/// Phase 36: detect whether `spec` represents a terminal iMessage send workflow.
///
/// Matches AppleScript bodies that address the Messages app AND invoke `send` —
/// the pattern the personality prompt prescribes for send-iMessage actions:
///   tell application "Messages"
///       set targetBuddy to buddy "+…" of targetService
///       send "body" to targetBuddy
///   end tell
///
/// Match is lowercase-substring; string-literal content is lowercased at the
/// call site, so `"Messages"` in either case will catch. Requires both markers
/// so a plain Contacts lookup (`tell application "Contacts"`) does not match
/// and the Contacts → send chain still runs its continuation.
///
/// Token-level match on `send` rules out `resend`/`sendmessage` false-positives
/// (a plain substring check on `"send "` matches `"resend "` because the trailing
/// space sits outside the `send` prefix — split on whitespace to get real tokens).
fn is_terminal_send_action(spec: &ActionSpec) -> bool {
    match spec {
        ActionSpec::AppleScript { script, .. } => {
            let s = script.to_ascii_lowercase();
            if !s.contains("tell application \"messages\"") {
                return false;
            }
            // Token-level search — requires `send` as a standalone AppleScript verb.
            // `split_ascii_whitespace` yields tokens stripped of surrounding whitespace,
            // so "resend" and "sendmessage" correctly fail to match.
            s.split_ascii_whitespace().any(|tok| tok == "send")
        }
        _ => false,
    }
}

/// Lifecycle stage of a single interaction.
pub enum InteractionStage {
    /// Action executing in a background tokio task.
    ActionInFlight,
    /// Action complete, TTS feedback queued or in progress.
    FeedbackPending,
    /// All done — ready for cleanup.
    Complete,
}

/// Priority level for TTS scheduling.
///
/// Active = most recent user-initiated interaction (owns TTS worker).
/// Background = previous interaction whose action completed after a new one started.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InteractionPriority {
    Active,
    #[allow(dead_code)] // Phase 24c: used when new hotkey press demotes existing interaction
    Background,
}

/// Maximum age (seconds) before an interaction is garbage-collected regardless
/// of stage. Catches edge cases where a spawned action task panics silently.
const INTERACTION_TTL_SECS: u64 = 300;

// ── SharedDaemonState ─────────────────────────────────────────────────────────

/// Phase 38c: resources that live for the daemon's lifetime, NOT per session.
///
/// Bundles together the cross-session coordinators and atomic flags that
/// `CoreService` constructs once at startup and clones into every new
/// `CoreOrchestrator`. The crucial property: cloning produces independent
/// handles to the same underlying state, so all sessions observe the same
/// warmup completion, the same TTS worker, the same browser worker.
///
/// **Why this exists**: pre-Phase-38c each new gRPC session created a fresh
/// `CoreOrchestrator` which spawned its own TTS worker + browser worker and
/// re-ran FAST/PRIMARY warmup queries. The browser-worker spawn caused
/// memory pressure that evicted PRIMARY's mmap'd pages — every reconnect
/// ate a 22-second cold-load on the first chat turn. See MEMORY.md
/// "Phase 38c" for the full diagnosis.
///
/// **What stays per-session** (not bundled here):
///   - `ConversationContext` (per-conversation turn history)
///   - `SessionStateManager` (per-session ID, persistence path)
///   - `cancel_token` (per-session generation cancellation)
///   - `in_flight_actions` (per-session action handles)
///   - `current_state` (per-session UI state)
///
/// **What lives here** (shared across all sessions):
///   - `voice` — single TTS worker (kokoro Python process), one for the daemon
///   - `browser` — single browser worker (Playwright chromium), one for the daemon
///   - `fast_model_warm` / `primary_model_warm` / `embed_model_warm` — Ollama
///     warmup state. Set once at daemon startup, observed by all sessions.
///   - `startup_greeting_sent` — gates "Ready." TTS to fire only on the first
///     session that connects, not every reconnect.
///   - `pending_primary_rewarm_global` — moved here so HEAVY swap rewarm logic
///     works across session boundaries (a HEAVY query in session 1 that gets
///     barge-in'd should still trigger rewarm even if session 1 closes before
///     completion).
#[derive(Clone)]
pub struct SharedDaemonState {
    pub voice: crate::voice::VoiceCoordinator,
    pub browser: crate::browser::BrowserCoordinator,
    pub fast_model_warm: Arc<AtomicBool>,
    pub primary_model_warm: Arc<AtomicBool>,
    pub embed_model_warm: Arc<AtomicBool>,
    pub startup_greeting_sent: Arc<AtomicBool>,
}

impl SharedDaemonState {
    /// Build a "degraded" daemon state with empty workers and all flags unset.
    /// `CoreService::new()` constructs this and then spawns the daemon-startup
    /// warmup task that brings each piece online.
    ///
    /// Tests use this as the default for `CoreOrchestrator::new()` — they don't
    /// need real workers; the unit tests exercise the orchestrator's own logic
    /// against degraded coordinators.
    pub fn new_degraded() -> Self {
        Self {
            voice: crate::voice::VoiceCoordinator::new_degraded(),
            browser: crate::browser::BrowserCoordinator::new_degraded(),
            fast_model_warm: Arc::new(AtomicBool::new(false)),
            primary_model_warm: Arc::new(AtomicBool::new(false)),
            embed_model_warm: Arc::new(AtomicBool::new(false)),
            startup_greeting_sent: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Phase 38c: run the daemon-startup warmup sequence.
    ///
    /// Called once from `CoreService::new()` (in its own spawned task) before any
    /// session can connect. Brings TTS + browser workers online and warms the
    /// Ollama models. Sets the atomic flags as each piece completes so connecting
    /// sessions observe the ready state without having to re-warm.
    ///
    /// Order is deliberate (matches the pre-Phase-38c per-session flow):
    ///   1. TTS worker (kokoro Python — needs ~3s to load model)
    ///   2. Browser worker (Playwright chromium — ~500MB RAM spike)
    ///   3. Embed model (mxbai-embed-large — fire-and-forget, non-critical)
    ///   4. FAST model (qwen3:8b — sequential, 70s cold-load)
    ///   5. PRIMARY model (gemma4:26b — sequential, 22s cold-load) ← memory pressure cliff
    ///   6. PRIMARY keepalive task (long-lived; spawned only after PRIMARY is warm)
    ///
    /// Steps 4 and 5 are sequential (not concurrent) to avoid USB-SSD read-bandwidth
    /// contention that historically pushed FAST's cold-load from ~70s to several
    /// minutes when both started at once.
    ///
    /// Failures are non-fatal: a worker that won't spawn or a model that fails to
    /// load just leaves its `_warm` flag at false; sessions still open, just with
    /// a degraded subset of capabilities (logged at warn level).
    pub async fn run_startup_warmup(&self, cfg: Arc<crate::config::DexterConfig>) {
        info!("Daemon startup warmup beginning");

        // Step 1: TTS worker. Failure is non-fatal — voice stays text-only.
        self.voice.start_tts().await;
        if self.voice.is_tts_available() {
            info!("TTS worker ready (daemon-startup)");
        } else {
            warn!("TTS worker unavailable — daemon stays text-only mode");
        }

        // Step 2: Browser worker. Failure is non-fatal — browser actions degrade.
        self.browser.start().await;

        // Build an InferenceEngine for the warmup requests. CoreOrchestrator::new
        // will build its own per-session; this one is just for daemon startup.
        let engine = match crate::inference::engine::InferenceEngine::new(cfg.inference.clone()) {
            Ok(e) => e,
            Err(e) => {
                warn!(
                    error = %e,
                    "InferenceEngine setup failed at daemon startup — model warmups skipped"
                );
                return;
            }
        };

        // Step 3: Embed model (fire-and-forget). Memory recall depends on this
        // but isn't on the critical path for "Ready." TTS.
        spawn_embed_warmup(
            engine.clone(),
            cfg.models.embed.clone(),
            self.embed_model_warm.clone(),
        );

        // Step 4: FAST model (await). First-token latency for any chat depends
        // on this being warm.
        warm_fast_model_inline(&engine, &cfg.models.fast, &self.fast_model_warm).await;

        // Step 5: PRIMARY model (await). Routed-PRIMARY queries depend on this.
        warm_primary_model_inline(&engine, &cfg.models.primary, &self.primary_model_warm).await;

        // Step 6: PRIMARY keepalive task. Long-lived; runs for the daemon's
        // lifetime. Re-touches PRIMARY's mmap'd pages every
        // PRIMARY_KEEPALIVE_PING_INTERVAL_SECS to prevent macOS page reclamation.
        // Only spawned after PRIMARY is actually warm — the task gates on the
        // atomic flag, so spawning it earlier would just no-op until then, but
        // the order is clearer this way.
        if self.primary_model_warm.load(Ordering::SeqCst) {
            CoreOrchestrator::spawn_primary_keepalive_task(
                engine.clone(),
                cfg.models.primary.clone(),
                self.primary_model_warm.clone(),
            );
        }

        info!("Daemon startup warmup complete — sessions can connect with no warmup tax");
    }
}

/// Phase 38c helper — fire-and-forget embed model warmup.
fn spawn_embed_warmup(
    engine: crate::inference::engine::InferenceEngine,
    embed_model: String,
    warm_flag: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        info!("Warming up embed model: {embed_model}");
        let req = crate::inference::engine::EmbeddingRequest {
            model_name: embed_model.clone(),
            input: "warmup".to_string(),
        };
        match engine.embed(req).await {
            Ok(_) => {
                warm_flag.store(true, Ordering::SeqCst);
                info!("Embed model warm: {embed_model}");
            }
            Err(e) => warn!(
                error = %e,
                "Embed warmup failed — memory recall may be skipped on first query"
            ),
        }
    });
}

/// Phase 38c helper — sequential FAST model warmup. Awaited by daemon-startup.
async fn warm_fast_model_inline(
    engine: &crate::inference::engine::InferenceEngine,
    model: &str,
    warm_flag: &Arc<AtomicBool>,
) {
    info!("Warming up FAST model: {model}");
    let req = GenerationRequest {
        model_name: model.to_string(),
        messages: vec![crate::inference::engine::Message::user("hi".to_string())],
        temperature: None,
        unload_after: false,
        keep_alive_override: Some(FAST_MODEL_KEEP_ALIVE),
        num_predict: Some(1),
        // 300s window covers worst-case cold-loads on USB-SSD.
        inactivity_timeout_override_secs: Some(300),
        num_ctx_override: None,
    };
    match engine.generate_stream(req).await {
        Ok(mut rx) => {
            while rx.recv().await.is_some() {}
            warm_flag.store(true, Ordering::SeqCst);
            info!("FAST model warm: {model}");
        }
        Err(e) => warn!(
            error = %e,
            "FAST model warmup failed — first query may be slow"
        ),
    }
}

/// Phase 38c helper — sequential PRIMARY model warmup. Awaited by daemon-startup.
async fn warm_primary_model_inline(
    engine: &crate::inference::engine::InferenceEngine,
    model: &str,
    warm_flag: &Arc<AtomicBool>,
) {
    info!("Warming up PRIMARY model: {model}");
    let req = GenerationRequest {
        model_name: model.to_string(),
        messages: vec![crate::inference::engine::Message::user("hi".to_string())],
        temperature: None,
        unload_after: false,
        keep_alive_override: Some(PRIMARY_MODEL_KEEP_ALIVE),
        num_predict: Some(1),
        // 300s window covers worst-case cold-loads on USB-SSD.
        inactivity_timeout_override_secs: Some(300),
        num_ctx_override: None,
    };
    match engine.generate_stream(req).await {
        Ok(mut rx) => {
            while rx.recv().await.is_some() {}
            warm_flag.store(true, Ordering::SeqCst);
            info!("PRIMARY model warm: {model}");
        }
        Err(e) => warn!(
            error = %e,
            model = %model,
            "PRIMARY model warmup failed — first PRIMARY-routed query may stall. \
             Confirm the model is pulled in Ollama (`ollama list`)."
        ),
    }
}

// ── CoreOrchestrator ──────────────────────────────────────────────────────────

/// Per-session coordinator. Constructed once per `Session()` RPC call.
pub struct CoreOrchestrator {
    engine: InferenceEngine,
    router: ModelRouter,
    personality: PersonalityLayer,
    context: ConversationContext,
    /// Cached from DexterConfig at construction time — used in every handle_text_input
    /// to resolve `ModelId` → Ollama model tag via `ModelId::ollama_name(&model_config)`.
    model_config: ModelConfig,
    session_mgr: SessionStateManager,
    context_observer: ContextObserver, // Phase 7 — machine context aggregator
    action_engine: ActionEngine,       // Phase 8 — system action execution
    retrieval: RetrievalPipeline,      // Phase 9 — semantic memory + web grounding
    voice: VoiceCoordinator,           // Phase 10 — TTS worker lifecycle
    proactive_engine: ProactiveEngine, // Phase 17 — proactive observation rate-limiter
    hotkey_config: crate::config::HotkeyConfig, // Phase 18 — pushed to Swift via ConfigSync
    /// Phase 37.9 / T8: operator's own iMessage handle for self-send intercept.
    /// When set and the operator says "text myself"/"send me X", the orchestrator
    /// rewrites the LLM's Messages-send AppleScript to address this handle
    /// directly — bypassing Contacts lookup and the confabulation risk that
    /// produced the T8 live-smoke failure. None → self-send requests are rejected.
    operator_self_handle: Option<String>,
    /// Phase 37.9 / T8: operator's self-nicknames (case-insensitive, whole-word).
    /// Extends self-reference detection so "text jay my list" also resolves to
    /// `operator_self_handle` when "jay" is listed here. Empty by default.
    operator_self_aliases: Vec<String>,
    tx: mpsc::Sender<Result<ServerEvent, Status>>,
    session_id: String,
    // Phase 15: suppress repeated UI notifications after permanent worker failure.
    voice_degraded_notified: bool,
    browser_degraded_notified: bool,
    /// Phase 19: True while a CAUTIOUS/DESTRUCTIVE action awaits operator approval.
    /// Prevents AUDIO_PLAYBACK_COMPLETE from emitting a spurious EntityState::Idle that
    /// would cancel the ALERT state during the operator's confirmation window.
    /// Set in `handle_text_input` on ActionOutcome::PendingApproval.
    /// Cleared in `handle_action_approval` before transitioning back to IDLE.
    action_awaiting_approval: bool,
    /// Phase 24: sender half of the action result channel. Cloned into each
    /// spawned action task so it can deliver `ActionResult` back to the event loop.
    /// The receiver (`action_rx`) lives in `ipc::server`'s `select!` loop.
    action_tx: mpsc::Sender<ActionResult>,
    /// Phase 24: debounce timestamp for KV cache prefill requests.
    /// Prevents flooding Ollama with prefill requests on rapid context changes
    /// (e.g. every keystroke in a text editor). At most one prefill per
    /// `PREFILL_DEBOUNCE_SECS` (5 seconds).
    last_prefill_at: Option<Instant>,
    /// Timestamp of the most recent successful Vision-tier turn (image actually
    /// attached). Read by the vision-continuation router override: if this is
    /// within `VISION_CONTINUATION_WINDOW_SECS` AND the current user utterance
    /// contains an anaphoric or visual-reference marker, the route is upgraded
    /// from Chat → Vision so the image re-attaches and gemma4:26b can answer
    /// follow-ups about the same (or newly-displayed) image.
    ///
    /// Set in `handle_text_input` after `capture_screen()` succeeds and the
    /// image is appended to the user message. NOT set on Vision-route demotions
    /// (capture failed, route fell back to PRIMARY) — those turns had no image,
    /// so follow-ups have no visual context to continue.
    last_vision_turn_at: Option<Instant>,
    /// Timestamp of the most recent joke-request turn (joke override fired and
    /// PRIMARY actually generated the joke). Read by the joke-continuation
    /// override: if this is within `JOKE_CONTINUATION_WINDOW_SECS` AND the
    /// current utterance contains a joke-iteration marker (criticism,
    /// correction, "explain the joke", "another"), the route is upgraded
    /// FAST → PRIMARY so the same model that told the joke can iterate.
    ///
    /// Without this, follow-ups like "that wasn't NSFW enough" or "explain
    /// the joke" route to FAST (qwen3:8b), which lacks the breadth to iterate
    /// and hallucinates joke explanations from training data rather than
    /// reading the actual joke just told.
    last_joke_turn_at: Option<Instant>,
    /// Phase 24: in-flight interaction tracking. Keyed by `action_id`.
    /// Entries are inserted when an action is dispatched to a background task
    /// and removed when TTS feedback completes or by the 5-minute GC sweep.
    interactions: HashMap<String, Interaction>,
    /// Phase 27: the most-recently sent entity state, tracked so `handle_barge_in`
    /// knows whether a state transition is needed. Updated in `send_state`.
    current_state: EntityState,
    /// Phase 37.5 / B5: true while a HEAVY-routed generation is in flight after
    /// we explicitly unloaded PRIMARY to make VRAM room. `handle_generation_complete`
    /// checks this; if set, it spawns `warm_up_primary_model()` so PRIMARY is
    /// warm again before the next chat turn. Without this flag, the first chat
    /// turn after a HEAVY request would pay a 30–60 s PRIMARY cold-load stall
    /// (the same stall Phase 36 solved for startup). Cleared when the rewarm
    /// is spawned, so a second HEAVY request before PRIMARY finishes rewarming
    /// is a no-op (it'll be re-unloaded anyway). Field on `self` rather than
    /// threaded through `GenerationResult` because the rewarm decision depends
    /// on orchestrator state (whether we were the ones who unloaded PRIMARY)
    /// rather than on the generation output.
    pending_primary_rewarm: bool,
    /// Phase 27: cooperative cancellation token shared with the background generation
    /// task. `handle_barge_in` sets this to `true`; the token loop checks it per-token.
    /// Replaced with a fresh `Arc<AtomicBool>` after each barge-in so subsequent
    /// generations always start with a clean, uncancelled token.
    cancel_token: Arc<AtomicBool>,
    /// Phase 27: sender half of the generation result channel. Cloned into each
    /// spawned generation task so it can deliver `GenerationResult` back to the
    /// event loop via `gen_rx` in `ipc::server`.
    generation_tx: mpsc::Sender<GenerationResult>,
    /// Phase 33: JoinHandle for the currently-running generation background task.
    ///
    /// Stored so cancellation can call `handle.abort()`, which drops the reqwest
    /// response stream, closes the Ollama HTTP connection, and stops the server-side
    /// generation immediately. Without this, a cancelled task sits blocked at
    /// `await stream.next()` for up to GENERATION_WALL_TIMEOUT_SECS (160s) if Ollama
    /// hasn't sent the first token yet — blocking the Ollama queue for the next request.
    generation_handle: Option<tokio::task::JoinHandle<()>>,
    /// Phase 38 / Codex finding [7]: AbortHandle for the producer task spawned
    /// by `engine.generate_stream_cancellable()`. Stored inside an
    /// `Arc<std::sync::Mutex<Option<_>>>` because run_generation_background runs
    /// in a spawned task and needs to publish the handle back to the
    /// orchestrator's cancel paths. `Some` while a generation is in flight;
    /// `None` before the producer is spawned and after it completes/aborts.
    /// Aborting the producer drops `byte_stream` → drops the reqwest HTTP
    /// response → closes the Ollama HTTP connection immediately, regardless
    /// of whether the producer was parked at `byte_stream.next().await`.
    generation_producer_abort: Arc<std::sync::Mutex<Option<tokio::task::AbortHandle>>>,
    /// Phase 38 / Codex finding [10]: handles for all currently-executing action
    /// tasks, keyed by `action_id`. Inserted in the dispatch site, removed in
    /// `handle_action_result`, drained-and-aborted on cancel and shutdown so
    /// that "stop" / barge-in / SIGINT actually kill the underlying subprocess
    /// instead of leaving it running for up to `ACTION_DOWNLOAD_TIMEOUT_SECS`.
    /// Composes with Session 1's `kill_on_drop(true)` on `Command`: aborting
    /// the task drops the inner `Child`, which sends SIGKILL to the subprocess.
    in_flight_actions: std::collections::HashMap<String, tokio::task::JoinHandle<()>>,
    /// Phase 38 / Codex finding [13]: AbortHandle for the currently-running
    /// TTS read loop (the task spawned by `make_tts_channel`). Same slot
    /// pattern as `generation_producer_abort` from Codex [7] — populated by
    /// `make_tts_channel` after spawn, drained by `abort_active_generation`.
    /// Pre-Phase-38 the TTS task's JoinHandle was passed via parameter into
    /// `run_generation_background` and dropped on cancel (detaches without
    /// aborting), letting late MSG_TTS_AUDIO frames reach Swift after the
    /// barge-in transition. Aborting the TTS task drops its tx_arc lock guard
    /// AND the session_tx clone — both stop pushing audio frames immediately.
    ///
    /// Note: a residual race remains where AudioResponse frames already in the
    /// gRPC channel from the OLD generation can still play after Swift's
    /// `AudioPlayer.stop()` resets `nextExpectedSeq` to 0 (because the new
    /// generation's seq=0 frame is indistinguishable from a stale old-gen
    /// seq=0). The structural fix is a per-stream id field on AudioResponse
    /// (proto change). Deferred to Phase 38b along with the structured
    /// action types since that phase is already touching the proto.
    tts_handle_abort: Arc<std::sync::Mutex<Option<tokio::task::AbortHandle>>>,
    /// Set to `true` by `warm_up_fast_model()` when qwen3:8b finishes loading.
    /// Checked in `handle_text_input` — if false, the operator is told to wait and the
    /// request is dropped rather than hanging Ollama with a competing inference request
    /// while the model is still loading from disk.
    fast_model_warm: Arc<AtomicBool>,
    /// Phase 36: set to `true` by `warm_up_primary_model()` when gemma4:26b / mistral-small
    /// finishes loading. Not used as a gate (PRIMARY-routed requests still wait for the
    /// model on first use), but exposed for logging and potential telemetry: the warm
    /// state is the difference between "first PRIMARY query was instant" and
    /// "first PRIMARY query took 30-120s to emit a token".
    #[allow(dead_code)] // informational flag; no runtime gate yet
    primary_model_warm: Arc<AtomicBool>,
    /// Set to `true` by `warm_up_embed()` when mxbai-embed-large finishes loading.
    /// Checked before `recall_relevant()` — if false, memory recall is skipped for
    /// that turn rather than blocking the entire inference pipeline for ~30s waiting
    /// for the embed model to load from disk. Recall is non-critical; latency is.
    embed_model_warm: Arc<AtomicBool>,
    /// Phase 34: true when the current (or most recent) user turn came from voice input
    /// (hotkey → STT). Controls whether `make_tts_channel` spawns TTS synthesis.
    ///
    /// Voice in → voice+text out: set true in `handle_text_input` when `from_voice=true`.
    /// Typed in → text only:       set false when `from_voice=false`.
    /// Agentic continuations inherit the value from the turn that started the chain.
    voice_mode: bool,
    /// Phase 35: JSON representation of the last action dispatched in an agentic chain.
    /// Reset to `None` when a new user-initiated turn begins (agentic_depth == 0).
    /// Used to detect phantom retries: if depth > 0 and the model generates the exact
    /// same action spec as the previous step, the chain is stopped immediately with an
    /// error message rather than re-dispatching the identical failing action.
    last_agentic_action_json: Option<String>,
}

impl CoreOrchestrator {
    /// Build a fully-wired orchestrator for one session.
    ///
    /// Creates a fresh `ConversationContext` for this gRPC session.
    ///
    /// Session JSON is still persisted and `latest.json` is still read for logging, but
    /// prior transcript turns are deliberately not replayed into the live prompt. Raw
    /// bootstrap caused stale test/refusal turns to poison unrelated new sessions.
    ///
    /// Returns `Err(OrchestratorError::InferenceSetup)` if `InferenceEngine::new()` fails.
    /// That failure propagates to the reader task which logs and exits, causing the
    /// gRPC stream to close with `END_STREAM`. The daemon itself does not exit.
    pub fn new(
        cfg: &DexterConfig,
        session_id: String,
        tx: mpsc::Sender<Result<ServerEvent, Status>>,
        action_tx: mpsc::Sender<ActionResult>,
        generation_tx: mpsc::Sender<GenerationResult>,
        // Phase 38c: shared daemon-lifetime state (TTS worker, browser worker,
        // warmup flags, greeting-sent flag). Cloned by `CoreService` from the
        // single instance it constructs at daemon startup. Tests pass
        // `SharedDaemonState::new_degraded()`.
        shared: SharedDaemonState,
    ) -> Result<Self, OrchestratorError> {
        let engine = InferenceEngine::new(cfg.inference.clone())
            .map_err(OrchestratorError::InferenceSetup)?;

        let router = ModelRouter;
        // Phase 38 / Codex finding [33]: respect the operator-configured personality
        // path. `load_or_default_from()` falls back to defaults on missing/malformed
        // YAML the same way `load_or_default()` does — see PersonalityLayer impl.
        let personality = PersonalityLayer::load_or_default_from(&cfg.core.personality_path);
        let session_mgr = SessionStateManager::new(&cfg.core.state_dir, &session_id, &cfg.models);

        // Fresh per-session conversation context.
        //
        // Do NOT replay `latest.json` conversation_history here. That raw transcript
        // bootstrap has repeatedly acted as prompt contamination: a previous test turn
        // ("what color is the sky?") or refusal ("I do not generate NSFW...") becomes
        // "recent conversation" for the next unrelated session. Cross-session memory
        // must go through retrieval, where it can be relevance-ranked, framed as prior
        // reference material, and suppressed for sensitive contexts like jokes.
        let context = ConversationContext::new(session_id.clone(), CONVERSATION_MAX_TURNS);
        if let Some(prev) = SessionStateManager::load_latest(&cfg.core.state_dir) {
            let turn_count = prev.conversation_history.len();
            info!(
                session    = %session_id,
                previous_session = %prev.session_id,
                prev_turns = turn_count,
                "Previous session state found — not bootstrapping transcript into live context"
            );
        }

        // Phase 9 — Retrieval pipeline. Falls back to in-memory (non-persisted) on failure
        // so a VectorStore open error does not prevent the daemon from starting.
        let retrieval = match RetrievalPipeline::new(&cfg.core.state_dir) {
            Ok(r) => {
                info!(
                    path = %cfg.core.state_dir.join(MEMORY_DB_FILENAME).display(),
                    "Retrieval pipeline ready"
                );
                r
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "Retrieval DB init failed — using in-memory fallback (data not persisted)"
                );
                RetrievalPipeline::new_degraded()
            }
        };

        Ok(Self {
            engine,
            router,
            personality,
            context,
            model_config: cfg.models.clone(),
            session_mgr,
            context_observer: ContextObserver::new(),
            // Phase 38c: ActionEngine receives the shared BrowserCoordinator clone
            // so this session uses the daemon-lifetime chromium subprocess instead
            // of spawning its own.
            action_engine: ActionEngine::new(&cfg.core.state_dir, shared.browser.clone()),
            retrieval,
            // Phase 38c: clone the shared VoiceCoordinator so this session uses the
            // daemon-lifetime kokoro worker instead of spawning its own.
            voice: shared.voice.clone(),
            proactive_engine: ProactiveEngine::new(&cfg.behavior), // Phase 17
            hotkey_config: cfg.hotkey.clone(),                     // Phase 18
            operator_self_handle: cfg.behavior.operator_self_handle.clone(), // Phase 37.9 / T8
            operator_self_aliases: cfg.behavior.operator_self_aliases.clone(), // Phase 37.9 / T8
            tx,
            session_id,
            voice_degraded_notified: false,
            browser_degraded_notified: false,
            action_awaiting_approval: false,  // Phase 19
            action_tx,                        // Phase 24
            interactions: HashMap::new(),     // Phase 24
            last_prefill_at: None,            // Phase 24
            last_vision_turn_at: None,        // vision-continuation
            last_joke_turn_at: None,          // joke-continuation
            current_state: EntityState::Idle, // Phase 27
            cancel_token: Arc::new(AtomicBool::new(false)), // Phase 27
            generation_tx,                    // Phase 27
            generation_handle: None,          // Phase 33
            generation_producer_abort: Arc::new(std::sync::Mutex::new(None)), // Phase 38 / Codex [7]
            in_flight_actions: std::collections::HashMap::new(), // Phase 38 / Codex [10]
            tts_handle_abort: Arc::new(std::sync::Mutex::new(None)), // Phase 38 / Codex [13]
            // Phase 38c: clone shared warmup atomics. When CoreService's daemon-
            // startup task sets these to true, every session sees the change.
            // No more per-session warmup queries.
            fast_model_warm: shared.fast_model_warm.clone(),
            primary_model_warm: shared.primary_model_warm.clone(),
            embed_model_warm: shared.embed_model_warm.clone(),
            voice_mode: false,              // Phase 34
            last_agentic_action_json: None, // Phase 35
            pending_primary_rewarm: false,  // Phase 37.5 / B5
        })
    }

    // Phase 38c: `start_voice`, `warm_up_embed`, `warm_up_fast_model`, and
    // `warm_up_primary_model` were per-session methods on `CoreOrchestrator`.
    // They've been replaced by `SharedDaemonState::run_startup_warmup`, which
    // CoreService spawns at daemon startup. New sessions inherit the
    // already-warm shared state and skip these entirely. The standalone
    // helpers (`spawn_embed_warmup`, `warm_fast_model_inline`,
    // `warm_primary_model_inline`) live above next to `SharedDaemonState`.

    /// Phase 36 (revised): warm up the PRIMARY model sequentially after FAST,
    /// before "Ready." is announced.
    ///
    /// **Call site contract:** must be invoked AFTER `warm_up_fast_model()`'s
    /// JoinHandle has resolved, and the returned JoinHandle must be awaited
    /// BEFORE `send_startup_greeting()`. Rationale:
    ///
    ///   1. Sequential (not concurrent) with FAST: loading both in parallel
    ///      contends for USB-SSD read bandwidth and multiplies FAST's cold-load
    ///      time severalfold (observed pushing startup from ~70 s to many minutes).
    ///
    ///   2. Awaited before "Ready.": the announcement is a contract with the
    ///      operator — when they hear "Ready." they expect any routed query to
    ///      answer immediately. If PRIMARY is still loading when "Ready." plays,
    ///      the first complex question eats a 30-60 s cold-load penalty, which
    ///      is exactly the bug this phase was introduced to fix.
    ///
    /// Uses `PRIMARY_MODEL_KEEP_ALIVE` ("30m") — shorter than FAST's effectively-
    /// permanent pin because PRIMARY's footprint is 8-16 GB and long-idle sessions
    /// should reclaim the VRAM. 30 minutes covers natural session timings.
    ///
    /// Returns a `JoinHandle<()>` so server.rs can await it before the greeting.
    /// On warmup failure the task still completes (logs a warning); the handle
    /// resolving is the signal that startup may proceed.
    ///
    /// **Side effect (post-37.8 fix):** on successful warmup, spawns a
    /// long-lived background ping task that re-touches PRIMARY's weight pages
    /// every `PRIMARY_KEEPALIVE_PING_INTERVAL_SECS`. See
    /// `spawn_primary_keepalive_task` for the rationale — macOS can reclaim
    /// mmap'd GGUF pages despite Ollama's `keep_alive: "30m"` because Ollama
    /// does not `mlock`, and disk re-read of the 18 GB PRIMARY on USB-SSD is
    /// the ~22 s cold-load penalty observed in production.
    // (warm_up_primary_model removed — Phase 38c, see note above.)

    /// Spawn a long-lived background task that keeps PRIMARY's weight pages
    /// resident in macOS's unified memory.
    ///
    /// **The problem this fixes:**
    ///
    /// Ollama's `keep_alive: "30m"` is the eviction TTL — after 30 idle
    /// minutes Ollama unloads the model. But Ollama mmap's GGUF files without
    /// `mlock`, so the OS is free to page out those weights under memory
    /// pressure (Swift UI growth, browser Playwright worker churn, clipboard
    /// bursts). The next PRIMARY request "hits" an already-loaded model from
    /// Ollama's perspective, but every page fault pulls back from disk. On
    /// the project's USB-SSD (`/Volumes/BitHappens`), the 18 GB gemma4:26b
    /// warm-from-page-cache takes ~22 s — indistinguishable from a cold-load
    /// to the operator, who sees GENERATION_WALL_TIMEOUT_SECS fire before
    /// the first token arrives.
    ///
    /// **Why a tiny chat ping fixes it:**
    ///
    /// A `num_predict: 1` chat request walks the forward pass through every
    /// layer, touching the full weight set. OS page accounting marks those
    /// pages as recently used; the reclaim-candidate score resets. 3-minute
    /// cadence is well under PRIMARY_MODEL_KEEP_ALIVE (30 min) and under
    /// typical macOS LRU decay timescales. Cost: one tiny request every
    /// 180 s — negligible next to the 22 s cold-load it prevents.
    ///
    /// **Flag-guarded:** the ping loop checks `primary_model_warm` on every
    /// tick. HEAVY swap logic clears the flag when it unloads PRIMARY (see
    /// the B5 handler); the ping task silently pauses until a rewarm sets it
    /// true again. This keeps HEAVY's exclusive VRAM guarantee intact.
    ///
    /// **Why not `/api/show`:** it informs Ollama but doesn't touch the
    /// weight pages — the reason we picked this cadence is specifically to
    /// exercise the full load path.
    fn spawn_primary_keepalive_task(
        engine: InferenceEngine,
        model: String,
        warm_flag: Arc<AtomicBool>,
    ) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                PRIMARY_KEEPALIVE_PING_INTERVAL_SECS,
            ));
            // First tick fires immediately; skip it so we don't ping the model
            // we just finished warming up a moment ago.
            ticker.tick().await;
            loop {
                ticker.tick().await;

                if !warm_flag.load(Ordering::SeqCst) {
                    // Probably HEAVY is resident. The pending_primary_rewarm
                    // path will set warm_flag=true again when PRIMARY rewarms;
                    // we'll resume pinging on the next tick.
                    continue;
                }

                let req = GenerationRequest {
                    model_name: model.clone(),
                    messages: vec![crate::inference::engine::Message::user("hi".to_string())],
                    temperature: None,
                    unload_after: false,
                    keep_alive_override: Some(PRIMARY_MODEL_KEEP_ALIVE),
                    num_predict: Some(1),
                    // 60 s inactivity cap: under normal conditions a warm
                    // one-token ping returns in ~200 ms. If it blocks past
                    // 60 s, the model is in bad shape (unloaded, OOM, etc.) —
                    // bail rather than hanging the ping loop.
                    inactivity_timeout_override_secs: Some(60),
                    num_ctx_override: None,
                };
                match engine.generate_stream(req).await {
                    Ok(mut rx) => {
                        // Drain the token stream. Inspect the final chunk's
                        // load_duration_ms as the proof-of-work signal: a
                        // healthy ping on resident pages returns < ~500 ms;
                        // anything > 5 s means the ping itself had to page
                        // weights back from disk (macOS reclaimed them between
                        // ticks). Surfacing the duration at info! level is
                        // what makes the keepalive fix verifiable from the log
                        // rather than only from operator-perceived latency.
                        let mut final_ld_ms: Option<u64> = None;
                        while let Some(chunk) = rx.recv().await {
                            if let Ok(c) = chunk {
                                if c.done {
                                    final_ld_ms = c.load_duration_ms;
                                }
                            }
                        }
                        match final_ld_ms {
                            Some(ms) if ms > 5_000 => warn!(
                                model   = %model,
                                load_ms = ms,
                                "PRIMARY keepalive ping took a cold-load — OS reclaimed \
                                 pages between pings; consider shortening \
                                 PRIMARY_KEEPALIVE_PING_INTERVAL_SECS if this recurs"
                            ),
                            Some(ms) => info!(
                                model   = %model,
                                load_ms = ms,
                                "PRIMARY keepalive ping ok"
                            ),
                            None => info!(
                                model = %model,
                                "PRIMARY keepalive ping ok (no timing reported)"
                            ),
                        }
                    }
                    Err(e) => {
                        // Non-fatal: log and keep the loop alive. A transient
                        // Ollama hiccup shouldn't kill the page-resident task
                        // for the rest of the session.
                        warn!(
                            error = %e,
                            model = %model,
                            "PRIMARY keepalive ping failed — will retry on next tick"
                        );
                    }
                }
            }
        });
    }

    /// Announce readiness to the operator after the FAST model is warm.
    ///
    /// Sends SPEAKING state + "Ready." text to the HUD, then drives TTS if
    /// available. IDLE transition follows via AUDIO_PLAYBACK_COMPLETE (TTS path)
    /// or directly (text-only path). Called from `server.rs` after awaiting the
    /// `warm_up_fast_model` JoinHandle.
    pub async fn send_startup_greeting(&mut self, trace_id: &str) -> Result<(), OrchestratorError> {
        self.send_state(EntityState::Speaking, trace_id).await?;
        let spoke = self.speak_action_feedback("Ready.", trace_id).await?;
        if !spoke {
            // TTS unavailable — send IDLE directly since no AUDIO_PLAYBACK_COMPLETE will arrive.
            self.send_state(EntityState::Idle, trace_id).await?;
        }
        // Phase 37 / B2: Arm Gate 7 (proactive suppression) at the greeting moment.
        //
        // ProactiveEngine's Gate 7 uses `last_user_turn_at` to suppress proactive
        // triggers for `PROACTIVE_USER_ACTIVE_WINDOW_SECS` (60s) after any operator
        // turn. Without arming it here, the context observer's first post-warmup
        // tick (shell/app/focus snapshot gathered during the ~2-4 min warmup window)
        // can fire a proactive response the instant we declare Ready, stomping on
        // top of "Ready." with an unsolicited observation. Treating the greeting
        // itself as the anchor event gives the operator a 60s grace period to
        // speak first before proactive behavior resumes.
        self.proactive_engine.record_user_turn();
        Ok(())
    }

    /// Phase 24: fire a background KV cache prefill with the current prompt prefix.
    ///
    /// Sends a `num_predict: 1` request containing the system prompt + context
    /// snapshot (the invariant prefix for any future voice query). Ollama processes
    /// the entire prompt into the KV cache, generates 1 discarded token, and stops.
    ///
    /// The next real request with the same prefix reuses the cached KV entries —
    /// only the user query tokens (~20–50) need processing. Latency savings:
    /// 200–500ms in the common case (single-client Ollama, same slot).
    ///
    /// **Debounce:** at most one prefill per `PREFILL_DEBOUNCE_SECS` (5 seconds).
    /// AXElementChanged fires on every keystroke; without debounce, Ollama would be
    /// flooded with prefill requests.
    ///
    /// **Cost of wrong prediction:** zero. If the context changes between prefill
    /// and real query (unlikely during a 2–3s utterance), Ollama detects the prefix
    /// divergence and falls through to full processing — no incorrect output, just
    /// no cache benefit. Degrades to current behavior.
    pub fn prefill_inference_cache(&mut self) {
        // Debounce: skip if last prefill was less than PREFILL_DEBOUNCE_SECS ago.
        if let Some(last) = self.last_prefill_at {
            if last.elapsed() < std::time::Duration::from_secs(PREFILL_DEBOUNCE_SECS) {
                return;
            }
        }
        self.last_prefill_at = Some(Instant::now());

        // Build the invariant prefix: system prompt + context snapshot.
        // No recall entries — we don't have a query yet.
        let recall_entries = vec![];
        let messages = self.prepare_messages_for_inference(&recall_entries);

        let engine = self.engine.clone();
        let model_name = self.model_config.fast.clone();

        tokio::spawn(async move {
            let req = GenerationRequest {
                model_name: model_name.clone(),
                messages,
                temperature: None,
                unload_after: false,
                keep_alive_override: Some(FAST_MODEL_KEEP_ALIVE),
                num_predict: Some(1), // Process prefix → KV cache, generate 1 token, stop
                inactivity_timeout_override_secs: None,
                num_ctx_override: None,
            };

            // Fire and forget — if it fails, the normal path runs.
            // The prefill races against the user's speech + STT processing.
            // Speech: ~1.5–3s. STT: ~1s. Prefill: ~200ms. The prefill wins by 2+ seconds.
            match engine.generate_stream(req).await {
                Ok(mut rx) => {
                    while rx.recv().await.is_some() {}
                    debug!("KV cache prefill complete for {model_name}");
                }
                Err(e) => {
                    debug!(error = %e, "KV cache prefill failed — normal path will run");
                }
            }
        });
    }

    /// Health-check the TTS worker and restart it if dead (Phase 13).
    ///
    /// Delegates to `VoiceCoordinator::health_check_and_restart`, which applies
    /// exponential back-off and gives up after `VOICE_WORKER_RESTART_MAX_ATTEMPTS`.
    /// Phase 15: emits a one-time TextResponse to the UI on permanent failure.
    /// Called on a periodic timer from the session reader task in `ipc/server.rs`.
    pub async fn voice_health_check(&mut self) {
        self.voice.health_check_and_restart().await;

        // Phase 15: one-time UI notification on permanent degradation.
        if self.voice.is_permanently_degraded() && !self.voice_degraded_notified {
            self.voice_degraded_notified = true;
            warn!(session = %self.session_id, "TTS worker permanently degraded — notifying UI");
            self.send_text_response_to_ui(
                "Voice capability lost after repeated failures. Text-only mode is now active.",
                true,
            )
            .await;
        }
    }

    // Phase 38c: `start_browser` removed — browser worker is now spawned at
    // daemon startup via `SharedDaemonState::run_startup_warmup` and shared
    // across sessions. Each `CoreOrchestrator::new` clones the shared
    // `BrowserCoordinator` into its `ActionEngine`.

    /// Health-check the browser worker and restart it if dead (Phase 14).
    /// Phase 15: emits a one-time TextResponse to the UI on permanent failure.
    /// Called on a periodic timer from the session reader task in `ipc/server.rs`.
    pub async fn browser_health_check(&mut self) {
        self.action_engine.browser_health_check().await;

        // Phase 15: one-time UI notification on permanent degradation.
        if self.action_engine.is_browser_permanently_degraded() && !self.browser_degraded_notified {
            self.browser_degraded_notified = true;
            warn!(session = %self.session_id, "Browser worker permanently degraded — notifying UI");
            self.send_text_response_to_ui(
                "Browser automation unavailable after repeated failures.",
                true,
            )
            .await;
        }
    }

    // ── Public dispatch ───────────────────────────────────────────────────────

    /// Route one inbound ClientEvent to the appropriate handler.
    ///
    /// Called once per event in the reader task's event loop. Returns `Ok(())` on
    /// success. Returns `Err` when the gRPC channel is closed (unrecoverable for
    /// this session — the reader task will break and call `shutdown()`).
    pub async fn handle_event(&mut self, event: ClientEvent) -> Result<(), OrchestratorError> {
        let trace_id = event.trace_id.clone();
        match event.event {
            Some(client_event::Event::TextInput(input)) => {
                self.handle_text_input(input.content, trace_id, input.from_voice)
                    .await
            }
            Some(client_event::Event::SystemEvent(sys)) => {
                self.handle_system_event(sys, trace_id).await
            }
            Some(client_event::Event::UiAction(action)) => {
                self.handle_ui_action(action, trace_id).await
            }
            Some(client_event::Event::ActionApproval(appr)) => {
                self.handle_action_approval(appr, trace_id).await
            }
            Some(client_event::Event::BargIn(_)) => {
                // Phase 27: operator spoke over TTS — cancel in-flight generation.
                self.handle_barge_in(trace_id).await
            }
            None => Ok(()), // Malformed event with no variant — ignore silently.
        }
    }

    /// Phase 38 / Codex findings [7]+[8]+[10]: unified cancel sequence used by every
    /// path that interrupts an in-flight generation. Returns whether a generation
    /// was actually aborted (so callers can decide whether to invoke the HEAVY
    /// PRIMARY-rewarm helper [9]).
    ///
    /// Aborts in this order:
    ///   1. The consumer task (`run_generation_background` JoinHandle) — stops
    ///      token forwarding to the UI and TTS.
    ///   2. The producer task (engine's spawned bytes loop, exposed via
    ///      `generation_producer_abort` slot, Phase 38 fix for [7]) — drops
    ///      `byte_stream` and the reqwest HTTP response, closing the Ollama
    ///      connection immediately so it stops generating server-side.
    ///   3. All in-flight action subprocesses ([10]) — drained from
    ///      `in_flight_actions` and aborted; with `kill_on_drop(true)` from
    ///      Session 1 [4]/[25], this SIGKILLs the underlying child.
    ///   4. Sets the cooperative `cancel_token` (defense-in-depth for any code
    ///      path that polls it before its next await yields to the abort).
    ///   5. Replaces the token with a fresh `Arc<AtomicBool>` so the next
    ///      generation starts uncancelled.
    ///
    /// Returns `true` if any generation/action was actually aborted; `false`
    /// for the no-op case (cancel called when nothing was in flight).
    fn abort_active_generation(&mut self) -> bool {
        let mut aborted_anything = false;

        if let Some(handle) = self.generation_handle.take() {
            handle.abort();
            aborted_anything = true;
        }
        // Phase 38 / Codex [7]: also abort the producer. Without this, aborting
        // the consumer just dropped `rx`, which closed the channel — but the
        // producer's `byte_stream.next().await` could still park for up to
        // `inactivity` seconds before its next `tx.send()` discovered the closed
        // channel. The producer's abort_handle is published into this slot by
        // run_generation_background after a successful generate_stream_cancellable.
        if let Some(producer_abort) = self
            .generation_producer_abort
            .lock()
            .ok()
            .and_then(|mut g| g.take())
        {
            producer_abort.abort();
            aborted_anything = true;
        }
        // Phase 38 / Codex [10]: aborted action handles SIGKILL their child via
        // Session 1's kill_on_drop. Drain so a future cancel doesn't double-abort.
        for (_, h) in self.in_flight_actions.drain() {
            h.abort();
            aborted_anything = true;
        }
        // Phase 38 / Codex [13]: abort the TTS task too. Pre-Phase-38, late
        // MSG_TTS_AUDIO frames continued reaching Swift after barge-in;
        // aborting drops the session_tx clone the task holds, stopping audio
        // delivery immediately. Audio frames already buffered in the gRPC
        // channel are handled by Swift's `AudioPlayer.stop()` on the
        // LISTENING transition.
        if let Some(tts_abort) = self.tts_handle_abort.lock().ok().and_then(|mut g| g.take()) {
            tts_abort.abort();
            aborted_anything = true;
        }

        self.cancel_token.store(true, Ordering::SeqCst);
        self.cancel_token = Arc::new(AtomicBool::new(false));

        aborted_anything
    }

    /// Phase 27: cancel the in-flight background generation on VAD rising-edge during SPEAKING.
    ///
    /// Sets the shared cancel token so the background task stops at its next token check,
    /// then replaces it with a fresh `Arc<AtomicBool>` so the next generation starts clean.
    /// Transitions to LISTENING when the entity was SPEAKING or THINKING — these are the
    /// only states where an in-flight generation exists that needs cancelling.
    pub async fn handle_barge_in(&mut self, trace_id: String) -> Result<(), OrchestratorError> {
        info!(
            session   = %self.session_id,
            trace_id  = %trace_id,
            state     = ?self.current_state,
            "BargIn received — cancelling in-flight generation"
        );
        // Phase 38 / Codex [7]+[10]: unified cancel sequence — aborts both the
        // consumer (run_generation_background) AND the producer (engine bytes
        // loop) AND any in-flight action subprocesses. Pre-Phase-38 this only
        // aborted the consumer.
        let was_in_flight = self.abort_active_generation();
        // Phase 38 / Codex [9]: if the operator barged in mid-HEAVY, the
        // pending PRIMARY rewarm needs to fire from this code path. Pre-Phase-
        // 38 it only fired from handle_generation_complete, which never runs
        // for an aborted generation — leaving PRIMARY cold for the next chat
        // turn after every HEAVY barge-in.
        if was_in_flight {
            self.maybe_rewarm_primary(true, &trace_id);
        }
        match self.current_state {
            EntityState::Speaking | EntityState::Thinking => {
                self.send_state(EntityState::Listening, &trace_id).await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Phase 37.5 / B5 + Phase 38 / Codex finding [9]: spawn the PRIMARY rewarm
    /// when a HEAVY generation finishes (normal completion OR cancel).
    ///
    /// Pre-Phase-38 this logic was inlined inside `handle_generation_complete`
    /// — meaning a barge-in mid-HEAVY (which aborts the generation task before
    /// `handle_generation_complete` ever runs) skipped the rewarm entirely,
    /// leaving PRIMARY unloaded for the next chat turn.
    ///
    /// Extracted as a method so abort paths (handle_barge_in, etc.) can call
    /// it after their abort sequences. Idempotent: returns early if there's no
    /// pending rewarm. Safe to call multiple times — `pending_primary_rewarm`
    /// is consumed on first invocation.
    ///
    /// `cancelled` parameter controls whether HEAVY needs an explicit unload
    /// before the PRIMARY warmup request. On normal completion HEAVY's
    /// `unload_after=true` keep_alive:0 sentinel was already honored by Ollama.
    /// On cancel it was NOT (the reqwest stream was dropped before Ollama
    /// processed the body) — so we explicitly unload first, poll until
    /// eviction, then warmup PRIMARY.
    fn maybe_rewarm_primary(&mut self, cancelled: bool, trace_id: &str) {
        if !self.pending_primary_rewarm {
            return;
        }
        self.pending_primary_rewarm = false;

        // Phase 37.6 / Cluster-E (B5 refinement): the prior implementation
        // spawned `warm_up_primary_model()` immediately, racing against an
        // in-flight HEAVY eviction. When HEAVY was still resident at the
        // moment PRIMARY's warmup request hit Ollama, Ollama had no VRAM
        // headroom and the warmup silently failed (observed: "only 1010.5
        // MiB VRAM available" while HEAVY occupied 26.6 GB). PRIMARY then
        // stayed cold for the next query despite `pending_primary_rewarm`
        // having fired.
        //
        // Fix: spawn a single task that (1) on cancel, explicitly unloads
        // HEAVY; (2) polls `/api/ps` until HEAVY is actually gone from the
        // resident set; (3) only then issues the PRIMARY warmup request.
        // Polling caps at 10 s — well past typical eviction latency — after
        // which we proceed anyway on the theory that something unusual is
        // happening and the warmup's own error reporting is more useful
        // than a silent giveup.
        let engine = self.engine.clone();
        let heavy_name = self.model_config.heavy.clone();
        let primary_name = self.model_config.primary.clone();
        let warm_flag = self.primary_model_warm.clone();
        let session_id = self.session_id.clone();
        // Clone twice: one moves into the spawned task; one stays in this scope
        // for the trailing `info!` after spawn.
        let trace_id_task = trace_id.to_string();
        let trace_id_log = trace_id.to_string();
        tokio::spawn(async move {
            let trace_id = trace_id_task;
            // Cancel path: HEAVY's unload_after=true sentinel is only honored on
            // normal stream completion. A barge-in aborted the reqwest stream
            // before Ollama processed `keep_alive:0`, so HEAVY is still pinned.
            // Explicit unload kicks the eviction.
            if cancelled {
                if let Err(e) = engine.unload_model(&heavy_name).await {
                    warn!(
                        session = %session_id,
                        model   = %heavy_name,
                        error   = %e,
                        "HEAVY unload after cancel failed — continuing to poll"
                    );
                }
            }

            // Poll /api/ps until HEAVY disappears. 500 ms × 20 = 10 s cap.
            const MAX_ATTEMPTS: u32 = 20;
            const POLL_INTERVAL_MS: u64 = 500;
            let mut evicted = false;
            for attempt in 0..MAX_ATTEMPTS {
                match engine.ps().await {
                    Ok(entries) => {
                        let heavy_resident = entries.iter().any(|e| e.name == heavy_name);
                        if !heavy_resident {
                            info!(
                                session  = %session_id,
                                trace_id = %trace_id,
                                attempt,
                                "HEAVY evicted — starting PRIMARY rewarm"
                            );
                            evicted = true;
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(
                            session = %session_id,
                            error   = %e,
                            "ps() probe failed during rewarm gate — proceeding"
                        );
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
            }
            if !evicted {
                warn!(
                    session = %session_id,
                    model   = %heavy_name,
                    "HEAVY still resident after 10s — starting PRIMARY rewarm anyway"
                );
            }

            // Inline warmup (rather than calling self.warm_up_primary_model)
            // because we are now inside a detached tokio::spawn with no
            // `self` access. Same request shape as warm_up_primary_model
            // — any future change there must be mirrored here.
            info!(
                session = %session_id,
                model   = %primary_name,
                "Warming up PRIMARY model"
            );
            let req = GenerationRequest {
                model_name: primary_name.clone(),
                messages: vec![crate::inference::engine::Message::user("hi".to_string())],
                temperature: None,
                unload_after: false,
                keep_alive_override: Some(PRIMARY_MODEL_KEEP_ALIVE),
                num_predict: Some(1),
                inactivity_timeout_override_secs: Some(300),
                num_ctx_override: None,
            };
            match engine.generate_stream(req).await {
                Ok(mut rx) => {
                    while rx.recv().await.is_some() {}
                    warm_flag.store(true, Ordering::SeqCst);
                    info!(
                        session = %session_id,
                        model   = %primary_name,
                        "PRIMARY model warm"
                    );
                }
                Err(e) => warn!(
                    session = %session_id,
                    error   = %e,
                    model   = %primary_name,
                    "PRIMARY model warmup failed — first PRIMARY-routed query may stall"
                ),
            }
        });

        info!(
            session  = %self.session_id,
            trace_id = %trace_id_log,
            cancelled,
            "HEAVY generation finished — PRIMARY rewarm scheduled after eviction gate"
        );
    }

    /// Phase 27: handle the result from a background generation task.
    ///
    /// Called from the `gen_rx` arm of the session reader's `select!` loop when
    /// `run_generation_background` delivers its `GenerationResult`.
    ///
    /// When `result.cancelled = true` (barge-in interrupted the generation mid-stream),
    /// all post-processing is skipped — partial text must not be stored in memory or
    /// trigger action dispatch, and the IDLE transition is handled by the new generation
    /// that the operator's barge-in utterance will produce.
    pub async fn handle_generation_complete(
        &mut self,
        result: GenerationResult,
    ) -> Result<(), OrchestratorError> {
        // Clear the stored handle — this generation has completed (or was cancelled).
        // Prevents abort() from firing on a long-dead task if a new barge-in arrives
        // after the generation result has already been delivered.
        self.generation_handle = None;
        // Phase 38 / Codex [7]: also clear the producer slot. Producer task
        // exits on its own at done:true (or send-failed), so abort isn't needed
        // here — but the slot must be cleared so the next generation starts
        // with a clean None.
        if let Ok(mut g) = self.generation_producer_abort.lock() {
            *g = None;
        }

        // Phase 37.5 / B5 + Phase 38 / Codex [9]: rewarm PRIMARY if HEAVY was
        // routed for this generation. Extracted into `maybe_rewarm_primary` so
        // the abort paths (handle_barge_in, etc.) can call it too — pre-Phase-38
        // rewarm only fired here, meaning a barge-in mid-HEAVY left PRIMARY
        // permanently cold.
        self.maybe_rewarm_primary(result.cancelled, &result.trace_id);

        // Phase 31: shell-error proactive result — run_shell_error_proactive_background
        // handled text delivery, TTS, and IDLE transition directly. The only action
        // needed here is to refund the rate-limit slot if the model returned [SILENT].
        if result.is_shell_proactive {
            if result.proactive_silent {
                self.proactive_engine.undo_fire();
                info!(
                    session  = %self.session_id,
                    trace_id = %result.trace_id,
                    "Shell error proactive: [SILENT] returned — rate-limit slot refunded"
                );
            }
            return Ok(());
        }

        if result.cancelled {
            info!(
                session  = %self.session_id,
                trace_id = %result.trace_id,
                "Generation cancelled by barge-in — skipping post-processing"
            );
            return Ok(());
        }

        let mut full_response = result.full_response;
        let intercepted_q = result.intercepted_q;
        let tts_was_active = result.tts_was_active;
        let trace_id = result.trace_id;
        let content = result.content;
        let embed_model = result.embed_model;

        // 7a. Phase 19 — Uncertainty sentinel handling.
        let mut response_already_recorded = false;
        if let Some(ref query) = intercepted_q {
            info!(
                session  = %self.session_id,
                trace_id = %trace_id,
                query    = %query,
                "Uncertainty sentinel intercepted — retrieving grounding context"
            );
            let bridge = Self::bridging_phrase(&trace_id);
            self.send_text(bridge, false, &trace_id).await?;

            let retrieval_result = tokio::time::timeout(
                std::time::Duration::from_secs(RETRIEVAL_TIMEOUT_SECS),
                self.retrieval.retrieve_web_only(query),
            )
            .await;

            let tool_content = match retrieval_result {
                Ok(Ok(result)) => {
                    info!(
                        session    = %self.session_id,
                        source     = %result.source,
                        confidence = result.confidence,
                        "Uncertainty retrieval succeeded"
                    );
                    format!(
                        "[Retrieved: {}]\nSource: {}\nConfidence: {:.0}%\n\n{}",
                        result.query,
                        result.source,
                        result.confidence * 100.0,
                        result.text,
                    )
                }
                Ok(Err(e)) => {
                    warn!(session = %self.session_id, error = %e, "Uncertainty retrieval failed");
                    format!("[Retrieval failed for: {}]", query)
                }
                Err(_timeout) => {
                    warn!(session = %self.session_id, query = %query, "Uncertainty retrieval timed out");
                    format!("[Retrieval timed out for: {}]", query)
                }
            };

            self.context.push_tool_result(&tool_content);

            let reprompt_messages = self.prepare_messages_for_inference(&[]);
            let reprompt_model = self.model_config.primary.clone();
            let reprompt_response = self
                .generate_and_stream(&reprompt_model, reprompt_messages, &trace_id, false, None)
                .await?;

            full_response.push_str(&reprompt_response);
            if !reprompt_response.is_empty() {
                self.context
                    .push_assistant(strip_context_markers(&reprompt_response));
                self.session_mgr.push_turn("assistant", &reprompt_response);
            }
            response_already_recorded = true;
        }

        // 7b. Post-generation uncertainty retrieval (Phase 9).
        if let Some(trigger) = self.retrieval.detect_post_trigger(&full_response) {
            info!(session = %self.session_id, ?trigger, "Uncertainty marker — retrieving context");
            match self
                .retrieval
                .retrieve(&self.engine, &embed_model, &trigger)
                .await
            {
                Ok(ctx) => {
                    let injection = self.retrieval.format_for_injection(&ctx);
                    if !injection.is_empty() {
                        let tool_msg = format!("[Retrieved context]\n{}", injection);
                        self.context.push_user(tool_msg.clone());
                        self.session_mgr.push_turn("retrieval", &tool_msg);
                        let follow_model = self.model_config.primary.clone();
                        let follow_messages = self.prepare_messages_for_inference(&[]);
                        let follow_response = self
                            .generate_and_stream(
                                &follow_model,
                                follow_messages,
                                &trace_id,
                                false,
                                None,
                            )
                            .await?;
                        self.context
                            .push_assistant(strip_context_markers(&follow_response));
                        self.session_mgr.push_turn("assistant", &follow_response);
                        info!(session = %self.session_id, "Uncertainty follow-up generated successfully");
                    }
                }
                Err(e) => warn!(
                    session = %self.session_id,
                    error   = %e,
                    "Post-retrieval failed — uncertainty response not grounded"
                ),
            }
        }

        // 7c. Scan the full response for an embedded action block.
        let (display_text, action_spec) = extract_action_block(&full_response);
        let record_text = if display_text.is_empty() {
            &full_response
        } else {
            &display_text
        };

        // 7d. Memory accumulation (Phase 9).
        if !record_text.is_empty() {
            let session_id = self.session_id.clone();
            if let Err(e) = self
                .retrieval
                .store_conversation_turn(&self.engine, &embed_model, record_text, &session_id)
                .await
            {
                warn!(
                    session = %self.session_id,
                    error   = %e,
                    "Retrieval memory store failed — non-fatal, response delivered normally"
                );
            }
        }

        // 8. Record assistant reply.
        if !record_text.is_empty() && !response_already_recorded {
            self.context
                .push_assistant(strip_context_markers(record_text));
            self.session_mgr.push_turn("assistant", record_text);
        }

        // 8b. Embed and store the completed turn (Phase 21).
        if !full_response.is_empty() && !response_already_recorded {
            let turn_content = format!("User: {content}\nAssistant: {full_response}");
            self.retrieval
                .embed_and_store_turn(
                    &self.engine,
                    &embed_model,
                    &self.session_id,
                    &trace_id,
                    &turn_content,
                )
                .await;
        }

        // 8c. Extract and store implicit operator facts (Phase 22).
        if !full_response.is_empty() && !response_already_recorded {
            let extracted = extract_facts(&content);
            if !extracted.is_empty() {
                for fact in &extracted {
                    let slug = slug_id(fact);
                    self.retrieval
                        .store_fact(&self.engine, &embed_model, &slug, fact)
                        .await;
                    info!(fact = %fact, "Implicit fact extracted and stored");
                }
            }
        }

        // 9. Dispatch or gate the action (if present).
        let mut action_is_pending = false;
        let mut action_dispatched = false;
        // Phase 37.9 / T8: `spec` is `mut` so the self-send intercept can rewrite
        // the AppleScript body to a deterministic template before dispatch.
        if let Some(mut spec) = action_spec {
            // Command-query intercept guard.
            //
            // The personality instructs the model not to execute shell actions when the
            // operator asked a question about a command ("what command do I run?", "how do
            // I list processes by memory?"). qwen3:8b occasionally ignores this rule,
            // especially for well-known commands like `ps` where its training bias toward
            // action is strong.
            //
            // This guard enforces the rule at the Rust layer, where it is inviolable:
            //   - Only triggered on user-initiated turns (agentic_depth == 0) — continuation
            //     steps inside an established workflow are never blocked this way.
            //   - Only triggered for Shell actions — browser navigation, AppleScript, etc. are
            //     intentional even for informational queries ("what URL is that?" → navigate+extract).
            //   - When triggered: surface the command as plain text rather than executing it.
            if result.agentic_depth == 0 {
                // Phase 37 / B8: off-host intercept.
                //
                // The operator asked about another machine ("on my linux box",
                // "on the server", "via ssh"). The model nonetheless generated
                // a local Shell action — if we execute it, it runs on THIS Mac
                // and answers the wrong question (or, on mutating commands,
                // damages the wrong machine). Emit the command as plain text
                // the operator can copy to the remote session instead.
                //
                // Only triggered for Shell actions on user-initiated turns
                // (agentic_depth == 0). Continuation steps inside an already-
                // approved local workflow must not be disrupted by a stray
                // off-host phrase in the original utterance (e.g. "...the same
                // way I would on my linux box" as analogy, not target).
                // Continuations are protected because this check runs only at
                // depth 0; the 'analogy' false-positive at depth 0 is the
                // small cost for catching the much more dangerous true positive.
                if let ActionSpec::Shell { ref args, .. } = spec {
                    if is_off_host_request(&content) {
                        let cmd_text =
                            crate::action::executor::describe_normalized_shell_command(args)
                                .unwrap_or_else(|| args.join(" "));
                        warn!(
                            session = %self.session_id,
                            query   = %content,
                            cmd     = %cmd_text,
                            "Off-host request detected — surfacing command as text, not executing on this Mac"
                        );
                        let reply = format!(
                            "That looks like it's for a different machine — I'd only run it here. \
                             Here's the command to run there:\n\n```\n{cmd_text}\n```"
                        );
                        self.send_text(&reply, true, &trace_id).await?;
                        self.context.push_assistant(&reply);
                        self.session_mgr.push_turn("assistant", &reply);
                        self.send_state(EntityState::Idle, &trace_id).await?;
                        return Ok(());
                    }
                }

                if let ActionSpec::Shell { ref args, .. } = spec {
                    if is_command_query(&content) {
                        // Normalise the command before displaying — the model reliably
                        // generates GNU/Linux ps syntax even for informational answers.
                        // describe_normalized_shell_command returns the correct BSD
                        // equivalent when GNU flags are detected; falls back to the
                        // model's raw args when no rewrite is needed.
                        let cmd_text =
                            crate::action::executor::describe_normalized_shell_command(args)
                                .unwrap_or_else(|| args.join(" "));
                        warn!(
                            session  = %self.session_id,
                            query    = %content,
                            cmd      = %cmd_text,
                            "Command-query detected — displaying command as text rather than executing"
                        );
                        // Surface the corrected command so the operator can copy and run it.
                        let reply = format!("Here's the command:\n\n```\n{cmd_text}\n```");
                        self.send_text(&reply, true, &trace_id).await?;
                        self.context.push_assistant(&reply);
                        self.session_mgr.push_turn("assistant", &reply);
                        self.send_state(EntityState::Idle, &trace_id).await?;
                        return Ok(());
                    }
                }
            }

            // Phase 37.9 / T8: iMessage self-send intercept (runs at ALL depths).
            //
            // Live-smoke T8 surfaced a confabulation bug: the operator said
            // "text myself", the model's Contacts-lookup AppleScript errored
            // twice with `-2741` syntax errors, and on the third attempt the
            // model bypassed the lookup and fabricated an 855-prefix number
            // into the Messages-send script. The terminal-workflow short-
            // circuit (Phase 36 H3) then confirmed "Sent." — to a stranger.
            //
            // Root-cause fix: recipient resolution for self-sends belongs in
            // Rust, not in whatever AppleScript the model improvs. When the
            // operator's ORIGINAL utterance is a self-send intent AND the
            // proposed action is a Messages send, rewrite the script to a
            // deterministic template addressed to `operator_self_handle`.
            // When no handle is configured, reject with a HUD hint rather
            // than allowing the LLM-generated recipient to pass through.
            //
            // Fires at ALL agentic depths: the personality's 2-step flow
            // (Contacts lookup → Messages send) puts the send at depth 1.
            // The user's original content is carried through `result.content`
            // (→ `content` local) on every continuation, so self-reference
            // detection works at depth > 0 too. Intercept applies regardless
            // of what buddy the model put in the script — the rewrite is
            // deterministic from operator intent + configured handle.
            if is_terminal_send_action(&spec)
                && is_self_reference_request(&content, &self.operator_self_aliases)
            {
                let body_opt = match &spec {
                    ActionSpec::AppleScript { script, .. } => extract_messages_body(script),
                    _ => None,
                };
                let body = match body_opt {
                    Some(b) if !b.trim().is_empty() => b,
                    _ => {
                        warn!(
                            session = %self.session_id,
                            query   = %content,
                            agentic_depth = result.agentic_depth,
                            "iMessage self-send intercept — could not extract a non-empty body; asking operator to restate"
                        );
                        let reply = "I can't tell what you want me to text yourself. \
                                     Say the message and I'll send it.";
                        self.send_text(reply, true, &trace_id).await?;
                        self.context.push_assistant(reply);
                        self.session_mgr.push_turn("assistant", reply);
                        self.send_state(EntityState::Idle, &trace_id).await?;
                        return Ok(());
                    }
                };
                match self.operator_self_handle.as_deref() {
                    Some(handle) if !handle.trim().is_empty() => {
                        let new_script = build_self_send_script(handle, &body);
                        info!(
                            session = %self.session_id,
                            handle  = %handle,
                            body_chars = body.chars().count(),
                            agentic_depth = result.agentic_depth,
                            "iMessage self-send intercept — rewriting recipient to configured operator_self_handle"
                        );
                        spec = ActionSpec::AppleScript {
                            script:    new_script,
                            rationale: Some(
                                "Self-send via configured operator_self_handle (intercepted from LLM-generated script)".to_string()
                            ),
                        };
                    }
                    _ => {
                        warn!(
                            session = %self.session_id,
                            query   = %content,
                            agentic_depth = result.agentic_depth,
                            "iMessage self-send intercept — no operator_self_handle configured; rejecting"
                        );
                        let reply = "I don't have your iMessage handle configured, so I'm \
                                     not going to guess. Add `operator_self_handle = \"+1…\"` \
                                     to the `[behavior]` section of `~/.dexter/config.toml`, \
                                     or name the recipient explicitly.";
                        self.send_text(reply, true, &trace_id).await?;
                        self.context.push_assistant(reply);
                        self.session_mgr.push_turn("assistant", reply);
                        self.send_state(EntityState::Idle, &trace_id).await?;
                        return Ok(());
                    }
                }
            }

            // Phase 35: phantom-retry guard.
            //
            // qwen3 occasionally enters an agentic loop where it retries the EXACT same
            // failing action on every continuation step (depth 1→6) because the tool-result
            // error message doesn't penetrate its inference context strongly enough.
            //
            // If depth > 0 and the serialised ActionSpec matches the previous step's action,
            // stop the chain and surface the failure rather than re-running the same operation.
            // `last_agentic_action_json` is reset to None on every user-initiated turn
            // (depth == 0) so it never bleeds across unrelated turns.
            if result.agentic_depth > 0 {
                let spec_json = serde_json::to_string(&spec).unwrap_or_default();
                if self.last_agentic_action_json.as_deref() == Some(spec_json.as_str()) {
                    warn!(
                        session       = %self.session_id,
                        agentic_depth = result.agentic_depth,
                        action_json   = %spec_json,
                        "Phantom retry detected — model repeating identical action; stopping chain"
                    );
                    // Surface WHAT failed so the operator can decide a next step rather
                    // than getting a generic "didn't work" response. Short verb phrases
                    // are TTS-friendly (Kokoro cuts long sentences awkwardly at commas).
                    let action_phrase = match &spec {
                        ActionSpec::Shell { .. } => "run that shell command",
                        ActionSpec::AppleScript { .. } => "run that AppleScript",
                        ActionSpec::Browser { .. } => "do that in the browser",
                        ActionSpec::FileRead { .. } => "read that file",
                        ActionSpec::FileWrite { .. } => "save that file",
                    };
                    let msg = format!(
                        "I tried to {action_phrase} twice with the same result. \
                         Looping won't help — what should I try instead?"
                    );
                    let spoke = self.speak_action_feedback(&msg, &trace_id).await?;
                    if !spoke {
                        self.send_state(EntityState::Idle, &trace_id).await?;
                    }
                    return Ok(());
                }
                self.last_agentic_action_json = Some(spec_json);
            } else {
                // User-initiated turn: reset duplicate-action tracker for the new chain.
                self.last_agentic_action_json = None;
            }

            let category = PolicyEngine::classify(&spec);

            if category == ActionCategory::Destructive {
                match self.action_engine.submit(spec, &trace_id).await {
                    ActionOutcome::PendingApproval {
                        action_id,
                        description,
                        category,
                    } => {
                        info!(
                            session     = %self.session_id,
                            trace_id    = %trace_id,
                            action_id   = %action_id,
                            %description,
                            "DESTRUCTIVE action awaiting operator approval"
                        );
                        // Phase 37 / B10: surface a durable, visible warning in the
                        // HUD conversation history *before* sending the ActionRequest.
                        //
                        // The existing flow delivers the action description via
                        // `ActionRequest.description` → Swift confirmation dialog.
                        // That dialog is transient: it vanishes after approve/deny
                        // and leaves no trace in the conversation history, so a
                        // voice-only or head-turned operator never sees what was
                        // proposed. A short text message in the main channel puts
                        // the proposed action in HUD history where it's readable
                        // alongside every other turn, and it's the same channel
                        // the operator's yes/no reply will be recorded against.
                        //
                        // Kept to a single line + fenced description so it renders
                        // compactly. No TTS: voice prompts during approval would
                        // be a UX decision (interrupt/override, re-ask on timeout,
                        // etc.) that deserves its own design pass.
                        let warning = format!(
                            "⚠️ Needs approval before I run this ({cat}):\n\n```\n{desc}\n```\n\nSay yes or no.",
                            cat  = category_label(&category),
                            desc = description,
                        );
                        self.send_text(&warning, false, &trace_id).await?;

                        let req = ActionRequest {
                            action_id,
                            description,
                            category: category.into(),
                            payload: String::new(),
                        };
                        self.send_action_request(req, &trace_id).await?;
                        self.send_state(EntityState::Alert, &trace_id).await?;
                        action_is_pending = true;
                        self.action_awaiting_approval = true;
                    }
                    other => {
                        warn!(
                            session = %self.session_id,
                            outcome = ?other,
                            "submit() returned unexpected outcome for DESTRUCTIVE action"
                        );
                    }
                }
            } else {
                let action_id = uuid::Uuid::new_v4().to_string();
                let executor = self.action_engine.executor_handle();
                let action_tx = self.action_tx.clone();
                let aid = action_id.clone();
                let tid = trace_id.clone();

                // Phase 36: flag iMessage-send AppleScripts so handle_action_result
                // skips the continuation after a successful completion.
                let is_terminal_workflow = is_terminal_send_action(&spec);

                self.interactions.insert(
                    action_id.clone(),
                    Interaction {
                        trace_id: trace_id.clone(),
                        stage: InteractionStage::ActionInFlight,
                        priority: InteractionPriority::Active,
                        created_at: Instant::now(),
                        // Phase 32: thread depth + original query through the chain.
                        // agentic_depth is incremented here so handle_action_result can
                        // enforce AGENTIC_MAX_DEPTH and pass it to the next continuation.
                        agentic_depth: result.agentic_depth + 1,
                        original_content: content.clone(),
                        is_terminal_workflow,
                    },
                );

                info!(
                    session       = %self.session_id,
                    trace_id      = %trace_id,
                    action_id     = %action_id,
                    agentic_depth = result.agentic_depth + 1,
                    "Action dispatched to background task"
                );

                // Phase 38 / Codex finding [10]: track the spawned action so
                // cancel paths (handle_barge_in, hotkey, shutdown) can abort
                // it and SIGKILL its subprocess via Session 1's kill_on_drop.
                // Pre-Phase-38 the JoinHandle was discarded — "stop" returned
                // the UI to IDLE while a 30-second curl or 300-second yt-dlp
                // continued in the background.
                let action_handle = tokio::spawn(async move {
                    let outcome = executor.execute(&aid, &spec, category, None).await;
                    let _ = action_tx
                        .send(ActionResult {
                            action_id: aid,
                            outcome,
                            trace_id: tid,
                        })
                        .await;
                });
                self.in_flight_actions
                    .insert(action_id.clone(), action_handle);

                self.send_state(EntityState::Focused, &trace_id).await?;
                action_dispatched = true;
            }
        }

        // 10. IDLE transition.
        //
        // Round 3 / T0.3: agentic silent-exit guard.
        //
        // When a continuation step (agentic_depth > 0) yields an empty response AND
        // no follow-up action, the original handler just dropped to IDLE. From the
        // operator's perspective that reads as "I asked Dexter to do X, it ran some
        // commands, and then silently stopped" — an unrecoverable UX hole because
        // the operator can't tell whether the task succeeded, the model got confused,
        // or the task is still running.
        //
        // The guard only fires when:
        //   a) this is an agentic continuation (depth > 0 — never on user-turn
        //      empty responses, which are valid for e.g. `[SILENT]` sentinels),
        //   b) no action was dispatched or is pending (no more work queued),
        //   c) there's no visible/recordable text (`record_text` is empty),
        //   d) TTS wasn't active (the model didn't already say something).
        //
        // We speak a brief "I ran what I could but don't have a clean answer" so
        // the operator at least knows the chain ended rather than staring at an
        // entity that went IDLE with nothing to show for the action(s) it ran.
        let agentic_depth = result.agentic_depth;
        let went_silent = agentic_depth > 0
            && !action_is_pending
            && !action_dispatched
            && !tts_was_active
            && record_text.trim().is_empty();

        if went_silent {
            warn!(
                session       = %self.session_id,
                trace_id      = %trace_id,
                agentic_depth = agentic_depth,
                "Agentic continuation returned empty response — surfacing fallback"
            );
            let msg = "I ran the steps but don't have a clean answer to give. \
                       Want me to try a different approach?";
            let spoke = self.speak_action_feedback(msg, &trace_id).await?;
            if !spoke && !action_is_pending && !action_dispatched {
                self.send_state(EntityState::Idle, &trace_id).await?;
            }
            return Ok(());
        }

        if !action_is_pending && !action_dispatched && !tts_was_active {
            self.send_state(EntityState::Idle, &trace_id).await?;
        }

        Ok(())
    }

    /// Deliver a voice transcript directly from the STT fast path.
    ///
    /// Phase 24c: `stream_audio` sends final transcripts here via `InternalEvent`
    /// instead of waiting for Swift to echo them back as `TextInput`.  Saves 50–100ms
    /// per utterance and eliminates the echo race when the gRPC round-trip is slow.
    ///
    /// Behaviorally identical to receiving a `TextInput` with the same text.
    pub async fn handle_fast_transcript(
        &mut self,
        text: String,
        trace_id: String,
    ) -> Result<(), OrchestratorError> {
        // Fast-path transcripts always come from STT (hotkey → voice).
        self.handle_text_input(text, trace_id, true).await
    }

    /// Receives a shell command-completion event from the zsh integration hook
    /// (via `InternalEvent::ShellCommand` in the server.rs `select!` loop).
    ///
    /// Updates `ContextObserver` with the new shell context (Phase 30).
    ///
    /// Phase 31: when the command exits non-zero (excluding deliberate Ctrl+C / exit 130),
    /// checks the proactive rate-limit gates and spawns a background inference task.
    /// The task is non-blocking — handle_shell_command returns immediately after spawn
    /// so the select! loop remains responsive during the ~1–3s inference call.
    pub(crate) async fn handle_shell_command(
        &mut self,
        command: String,
        cwd: String,
        exit_code: Option<i32>,
    ) {
        self.context_observer
            .update_shell_command(command.clone(), cwd.clone(), exit_code);
        info!(
            session   = %self.session_id,
            command   = %command,
            cwd       = %cwd,
            exit_code = ?exit_code,
            "Shell command context updated"
        );

        // Phase 31: proactive observation on non-zero exit.
        //
        // Guard: skip for success (0), unknown (None), and deliberate Ctrl+C (130).
        // Benign exits like `grep` returning 1 are filtered by the model's [SILENT]
        // opt-out — not a hardcoded command list.
        //
        // Messages are built synchronously here (while &mut self is held), then the
        // inference is spawned as a background task. handle_shell_command returns
        // immediately; results arrive via gen_tx → handle_generation_complete.
        let is_error = exit_code.map_or(false, |c| c != 0 && c != 130);
        if is_error
            && self
                .proactive_engine
                .should_fire(self.context_observer.snapshot())
        {
            let exit_nonzero = exit_code.unwrap(); // safe: is_error guarantees Some(non-zero)
            let trace_id = uuid::Uuid::new_v4().to_string();

            // Build message list while &mut self is held.
            // Background task receives Vec<Message> — no access to self after spawn.
            let proactive_user = crate::inference::engine::Message::user(
                crate::proactive::ProactiveEngine::build_shell_error_prompt(
                    &command,
                    exit_nonzero,
                    &cwd,
                ),
            );
            let mut messages = self.personality.apply_to_messages(&[proactive_user]);
            if let Some(ax_summary) = self.context_observer.context_summary() {
                messages.insert(
                    1,
                    crate::inference::engine::Message::system(format!("Context: {}", ax_summary)),
                );
            }
            // Shell: after Context: — same take_while ordering idiom.
            let shell_insert = messages.iter().take_while(|m| m.role == "system").count();
            messages.insert(
                shell_insert,
                crate::inference::engine::Message::system(format!(
                    "Shell: $ {} → exit {} in {}",
                    command, exit_nonzero, cwd
                )),
            );

            let engine = self.engine.clone();
            let tx = self.tx.clone();
            let gen_tx = self.generation_tx.clone();
            let model = self.model_config.fast.clone();
            let tts_arc = if self.voice.is_tts_available() {
                Some(self.voice.tts_arc())
            } else {
                None
            };
            let sess = self.session_id.clone();
            let cmd_log = command.clone();

            // Burn the slot before spawning. An inference error keeps it burned
            // (prevents rapid re-fire loops). Only [SILENT] refunds via gen_tx.
            self.proactive_engine.record_fire();

            tokio::spawn(run_shell_error_proactive_background(
                engine,
                tx,
                sess,
                model,
                messages,
                trace_id,
                tts_arc,
                gen_tx,
                cmd_log,
                exit_nonzero,
            ));
        }
    }

    /// Consume the orchestrator, persist session state to disk.
    ///
    /// Called by the reader task after its event loop exits (EOF, error, or signal).
    /// Drains any pending DESTRUCTIVE actions (logs them as rejected before dropping
    /// the ActionEngine) before writing session state. If `persist()` fails, logs the
    /// error but does not propagate — the session is ending regardless.
    pub async fn shutdown(mut self) {
        // Phase 38 / Codex finding [8]: abort any in-flight generation BEFORE
        // closing workers. Pre-Phase-38 shutdown skipped this entirely — the
        // consumer JoinHandle was dropped (which detaches, doesn't abort), so
        // run_generation_background, the producer task, and any in-flight
        // action subprocesses kept running until they finished or hit their
        // timeouts. With this call the cancel sequence (consumer + producer +
        // actions) fires synchronously, kill_on_drop(true) from Session 1 [4]
        // SIGKILLs subprocess children, and shutdown completes promptly.
        //
        // Deliberately does NOT call maybe_rewarm_primary — the daemon is
        // exiting; warming PRIMARY for a "next chat turn" that will never come
        // is wasted work. Ollama's keep_alive TTL governs eviction after exit
        // anyway.
        let _ = self.abort_active_generation();

        // Phase 38c: do NOT shut down voice or browser here — they're shared
        // across sessions and owned by `CoreService`. Daemon shutdown is the
        // only correct lifecycle moment to stop them; per-session shutdown
        // would kill workers used by other sessions or the next reconnect.
        // `CoreService::shutdown_shared_workers` handles that, called from
        // main.rs on SIGINT/SIGTERM.

        // Drain pending actions — session end counts as implicit rejection.
        self.action_engine.drain_pending_on_shutdown().await;

        let session_id = self.session_id.clone();
        match self.session_mgr.persist() {
            Ok(path) => {
                info!(
                    session = %session_id,
                    path    = %path.display(),
                    "Session state persisted"
                );
            }
            Err(e) => {
                error!(
                    session = %session_id,
                    error   = %e,
                    "Session state persist failed — conversation history will not be recovered"
                );
            }
        }
    }

    // ── Private handlers ──────────────────────────────────────────────────────

    /// Handle a `TextInput` event — the core inference pipeline.
    ///
    /// Pipeline:
    ///   1. Record user turn in context and state manager
    ///   2. Transition entity to THINKING
    ///   3. Route to model tier (ModelRouter)
    ///   3b. [Phase 9] Pre-generation retrieval (RetrievalFirst category only)
    ///   4. Apply personality (PersonalityLayer → prepend system prompt)
    ///   4b. [Phase 16] Inject context snapshot (current app + element, if known)
    ///   4c. [Phase 9]  Inject retrieval context as second system message (if retrieved)
    /// Spawn a TTS synthesis task wired to an unbounded sentence channel.
    ///
    /// The returned sender is passed to `run_generation_background`; as tokens arrive
    /// the sentence splitter pushes complete sentences onto it. The spawned task drives
    /// the TTS worker: TEXT_INPUT → TTS_AUDIO frames → TTS_DONE → loop again.
    ///
    /// Returns `(None, None)` when TTS is unavailable (degraded voice).
    /// The caller must `drop(tts_tx)` after generation finishes to signal the task to exit.
    fn make_tts_channel(
        &self,
    ) -> (
        Option<tokio::sync::mpsc::UnboundedSender<String>>,
        Option<tokio::task::JoinHandle<()>>,
    ) {
        // Phase 34: TTS is only active when the current turn came from voice input (hotkey).
        // Typed HUD input always stays text-only — no synthesis wasted on muted output.
        if !self.voice.is_tts_available() || !self.voice_mode {
            return (None, None);
        }
        use crate::voice::protocol::msg;
        let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let tts_arc = self.voice.tts_arc();
        let session_tx = self.tx.clone();
        let mut seq = 0u32;

        // Phase 38 / Codex finding [13]: capture this slot before spawning so
        // we can publish the handle's AbortHandle into it after spawn returns.
        // Cancel paths (handle_barge_in, hotkey, shutdown) drain it via
        // abort_active_generation.
        let tts_handle_slot = self.tts_handle_abort.clone();

        let handle = tokio::spawn(async move {
            use crate::ipc::proto::{server_event, AudioResponse, EntityState, EntityStateChange};
            let mut first_sentence = true;
            while let Some(sentence) = srx.recv().await {
                // Transition to SPEAKING on the first sentence so the entity
                // shows the correct animation, Swift's isSpeaking gate arms
                // (enabling barge-in), and AudioPlayer's awaitingFinalCallback
                // is primed before any PCM arrives.
                if first_sentence {
                    first_sentence = false;
                    let speaking_evt = ServerEvent {
                        trace_id: String::new(),
                        event: Some(server_event::Event::EntityState(EntityStateChange {
                            state: EntityState::Speaking.into(),
                        })),
                    };
                    let _ = session_tx.send(Ok(speaking_evt)).await;
                }
                let mut guard = tts_arc.lock().await;
                if let Some(client) = guard.as_mut() {
                    if client
                        .write_frame(msg::TEXT_INPUT, sentence.as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    loop {
                        // Phase 38 / Codex [14]: bound the per-frame read so a
                        // stalled kokoro synth thread can't park us forever.
                        let frame_result = match tokio::time::timeout(
                            std::time::Duration::from_secs(
                                crate::constants::TTS_FRAME_READ_TIMEOUT_SECS,
                            ),
                            client.read_frame(),
                        )
                        .await
                        {
                            Err(_elapsed) => {
                                warn!(
                                    "TTS read_frame timed out after {}s — kokoro stalled, breaking out",
                                    crate::constants::TTS_FRAME_READ_TIMEOUT_SECS,
                                );
                                break;
                            }
                            Ok(r) => r,
                        };
                        match frame_result {
                            Ok(Some((msg::TTS_AUDIO, pcm))) => {
                                let evt = ServerEvent {
                                    trace_id: String::new(),
                                    event: Some(server_event::Event::AudioResponse(
                                        AudioResponse {
                                            data: pcm,
                                            sequence_number: seq,
                                            is_final: false,
                                        },
                                    )),
                                };
                                let _ = session_tx.send(Ok(evt)).await;
                                seq += 1;
                            }
                            Ok(Some((msg::TTS_DONE, _))) => break,
                            Ok(Some(_)) => {} // discard unexpected frames
                            _ => break,
                        }
                    }
                }
            }
        });
        // Phase 38 / Codex [13]: publish the abort handle so cancel paths can
        // stop audio delivery immediately on barge-in.
        if let Ok(mut g) = tts_handle_slot.lock() {
            *g = Some(handle.abort_handle());
        }

        (Some(stx), Some(handle))
    }

    ///   5+6. Stream tokens via generate_and_stream() helper
    ///   7. Send TextResponse(is_final=true) to close Swift's streaming display
    ///   7b. [Phase 9] Post-generation uncertainty check → follow-up retrieval + generation
    ///   7c. [Phase 9] Memory accumulation — embed + store assistant reply in VectorStore
    ///   8. Scan full response for action block; strip block; route to ActionEngine
    ///   9. Record assistant reply (stripped) in context + state manager
    ///  10. Transition entity to IDLE (or ALERT if DESTRUCTIVE action is pending)
    async fn handle_text_input(
        &mut self,
        content: String,
        trace_id: String,
        from_voice: bool,
    ) -> Result<(), OrchestratorError> {
        // Phase 34: record whether this turn came from voice so make_tts_channel can
        // decide whether to synthesise audio.  Agentic continuation chains inherit this
        // value — they call make_tts_channel on the same &self without going through
        // handle_text_input again.
        self.voice_mode = from_voice;

        // 0. STT noise gate (voice-only).
        //
        // whisper base.en frequently hallucinates single words or very short fragments
        // on quiet or ambiguous audio (e.g. room noise misheard as "Up.", "next year.").
        // Sending these to the model causes runaway agentic chains when the AX context
        // makes the model think an action is implied.  Discard transcripts under 4 chars
        // (after trimming) and return to IDLE — the operator can simply speak again.
        // 3 chars passes "OK", "Hi", "Yes", "No", "Go" while blocking most noise fragments.
        // "Up." is 3 chars including the period — whisper almost always includes punctuation
        // so the effective floor for single whisper words is 4+ chars with the period.
        //
        // NOT applied to typed input: the operator may deliberately type "y", "ok", etc.
        if from_voice && content.trim().len() < 3 {
            warn!(
                session  = %self.session_id,
                trace_id = %trace_id,
                content  = %content.trim(),
                "STT transcript too short — discarding (likely noise or whisper hallucination)"
            );
            self.send_state(EntityState::Idle, &trace_id).await?;
            return Ok(());
        }

        // Phase 36 / C1 fix: record the timestamp of this operator turn so the
        // ProactiveEngine can suppress observations for `PROACTIVE_USER_ACTIVE_WINDOW_SECS`
        // after any real input. Placed after the noise-gate (hallucinated fragments
        // should not count) but before the cancellation fast-path (cancellation IS an
        // active turn — the operator is present and engaged, don't blurt observations).
        self.proactive_engine.record_user_turn();

        // 0b. Cancellation fast-path.
        //
        // Single-word commands like "stop", "cancel", "never mind" are intercepted
        // before inference. When the operator types these during an ongoing generation or
        // action flood they want to halt immediately — routing them to the model would
        // cancel the current generation and then spawn a NEW generation with "stop" as the
        // query, repeating the exact action that was just cancelled.
        //
        // Words matched (case-insensitive, leading/trailing whitespace stripped):
        //   stop · cancel · halt · abort · nevermind · never mind · enough · quit
        //
        // If a generation is in flight it is cancelled (same path as barge-in).
        // The entity returns to IDLE with no response — silence is the correct reply.
        {
            let trimmed_lc = content.trim().to_lowercase();
            // Strip trailing punctuation that STT often appends ("stop.", "cancel!")
            // before matching against the cancellation vocabulary.
            let stripped = trimmed_lc.trim_end_matches(|c: char| c.is_ascii_punctuation());
            let is_cancel = matches!(
                stripped,
                "stop"
                    | "cancel"
                    | "halt"
                    | "abort"
                    | "nevermind"
                    | "never mind"
                    | "enough"
                    | "quit"
                    | "ok stop"
                    | "ok cancel"
            );
            if is_cancel {
                info!(
                    session  = %self.session_id,
                    trace_id = %trace_id,
                    word     = %trimmed_lc,
                    "Cancellation command — stopping any in-flight generation and returning to IDLE"
                );
                // Phase 38 / Codex [7]+[9]+[10]: unified cancel via the helper.
                // Aborts consumer + producer + in-flight actions, then schedules
                // PRIMARY rewarm if HEAVY was in flight.
                if self.abort_active_generation() {
                    self.maybe_rewarm_primary(true, &trace_id);
                }
                // Round 3 / T0.4: if an action approval dialog is open, treat "stop/cancel"
                // as an explicit denial. Without this the dialog remains stranded in
                // pending_actions — drain_pending_on_shutdown would eventually log it, but
                // meanwhile the ALERT state is cleared and the operator sees a ghost dialog.
                if self.action_awaiting_approval {
                    info!(
                        session  = %self.session_id,
                        trace_id = %trace_id,
                        "Cancellation word arrived during ALERT — rejecting pending action"
                    );
                    self.action_engine.reject_all_pending().await;
                    self.action_awaiting_approval = false;
                }
                self.send_state(EntityState::Idle, &trace_id).await?;
                return Ok(());
            }
        }

        // 0b.1. Round 3 / T0.4: ALERT-state guard.
        //
        // When action_awaiting_approval is set, the entity is in ALERT and a
        // DESTRUCTIVE action is waiting for operator approval via the gRPC
        // ActionApproval message (delivered through handle_action_approval).
        //
        // Historically, any text input here would:
        //   1. push_user() the content onto ConversationContext,
        //   2. send_state(THINKING) — clobbering ALERT,
        //   3. spawn a new generation, which could produce a second action spec,
        //   4. leave the original pending action stranded in action_engine.pending_actions
        //      until session shutdown (drain_pending_on_shutdown logs it as rejected).
        //
        // Step 2 is the visible defect: the operator's approval dialog silently
        // loses its ALERT animation and the approve/deny buttons no longer match
        // the animation state. Step 4 is the audit defect.
        //
        // Fix: interpret bare affirmatives ("yes", "ok", "do it") and negatives
        // ("no", "deny") as implicit approval responses. For everything else,
        // reject the input with a clarifying message so the operator knows why
        // the query was ignored. They can dismiss with "cancel" (handled above)
        // or explicitly respond yes/no.
        if self.action_awaiting_approval {
            let trimmed_lc = content.trim().to_lowercase();
            let is_yes = matches!(
                trimmed_lc.as_str(),
                "yes"
                    | "y"
                    | "ok"
                    | "okay"
                    | "sure"
                    | "do it"
                    | "go"
                    | "go ahead"
                    | "approve"
                    | "approved"
                    | "confirm"
                    | "confirmed"
            );
            let is_no = matches!(
                trimmed_lc.as_str(),
                "no" | "n" | "deny" | "denied" | "reject" | "rejected" | "don't"
            );

            if is_yes || is_no {
                info!(
                    session  = %self.session_id,
                    trace_id = %trace_id,
                    approved = is_yes,
                    "Typed approval response received during ALERT — resolving pending action"
                );
                // Resolve all pending actions with the operator's response. In practice
                // there is at most one at a time (PolicyEngine serializes DESTRUCTIVE
                // actions behind approval), but the "_all" name documents the invariant.
                if is_yes {
                    self.action_engine.approve_all_pending().await;
                } else {
                    self.action_engine.reject_all_pending().await;
                }
                self.action_awaiting_approval = false;
                self.send_state(EntityState::Idle, &trace_id).await?;
                return Ok(());
            }

            warn!(
                session  = %self.session_id,
                trace_id = %trace_id,
                content  = %content.trim(),
                "Text input received during ALERT (pending action approval) — ignored"
            );
            let msg = "There's an action waiting for your approval. \
                       Say \"yes\" to approve, \"no\" to deny, or \"cancel\" to dismiss.";
            self.send_text(msg, true, &trace_id).await?;
            // Entity stays in ALERT — do NOT transition. The approval dialog is still live.
            return Ok(());
        }

        // 0c. [Phase 21] Memory command fast-path.
        //
        // Explicit memory management commands are handled without routing or inference.
        // The function returns immediately after sending a deterministic confirmation.
        if let Some(cmd) = detect_memory_command(&content) {
            self.send_state(EntityState::Thinking, &trace_id).await?;
            match cmd {
                MemoryCommand::Remember(fact) => {
                    let slug = slug_id(&fact);
                    let model = self.model_config.embed.clone();
                    self.retrieval
                        .store_fact(&self.engine, &model, &slug, &fact)
                        .await;
                    self.send_text("Got it. I'll remember that.", true, &trace_id)
                        .await?;
                }
                MemoryCommand::Forget(target) => {
                    let slug = slug_id(&target);
                    let found = self.retrieval.delete_fact(&slug);
                    let reply = if found {
                        "Forgotten."
                    } else {
                        "I don't have that stored."
                    };
                    self.send_text(reply, true, &trace_id).await?;
                }
                MemoryCommand::List => {
                    let facts = self.retrieval.list_facts();
                    let reply = if facts.is_empty() {
                        "I don't have anything stored about you yet.".to_string()
                    } else {
                        facts
                            .iter()
                            .enumerate()
                            .map(|(i, e)| format!("{}. {}", i + 1, e.content))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    self.send_text(&reply, true, &trace_id).await?;
                }
            }
            self.send_state(EntityState::Idle, &trace_id).await?;
            return Ok(());
        }

        // 1a. Text barge-in: if generation is already in progress (THINKING state), cancel
        //     it before starting a new one. Mirrors the voice BargIn path in handle_barge_in.
        //     Without this, a second typed message spawns a parallel generation while the
        //     first is still holding Ollama — two concurrent requests make both slower and
        //     the entity appears stuck in THINKING indefinitely.
        if self.current_state == EntityState::Thinking {
            info!(
                session  = %self.session_id,
                trace_id = %trace_id,
                "Text input during THINKING — cancelling in-flight generation"
            );
            // Phase 38 / Codex [7]+[9]+[10]: unified cancel via the helper.
            if self.abort_active_generation() {
                self.maybe_rewarm_primary(true, &trace_id);
            }
        }

        // 1. Record user turn.
        self.context.push_user(content.clone());
        self.session_mgr.push_turn("user", &content);

        // 2. Transition to THINKING.
        self.send_state(EntityState::Thinking, &trace_id).await?;

        // 3. Route to model tier + begin memory recall concurrently (Phase 24).
        //
        // Routing is CPU-only (~µs). Memory recall embeds the query via Ollama
        // (~200-400ms). By starting both in tokio::join!, the embed call begins
        // immediately rather than waiting for routing + retrieval-first checks.
        //
        // Recall is started speculatively — it may be discarded if routing selects
        // Vision tier or if retrieval-first fires. The wasted embed call costs ~200ms
        // but these paths are rare (Vision queries, time/version questions).
        let context_messages = self.context.messages();
        let engine_ref = &self.engine;
        let embed_model = self.model_config.embed.clone();
        let retrieval_ref = &self.retrieval;
        let router_ref = &self.router;
        let content_ref = &content;
        let embed_is_warm = self.embed_model_warm.load(Ordering::Relaxed);

        // Skip memory recall when embed model isn't warm yet. The first query after
        // startup often arrives before mxbai finishes loading (embed warms in parallel
        // with the FAST model, but fast model warmup is awaited — embed is not).
        // Blocking here would freeze inference for ~30s. Skipping recall is harmless
        // for the first turn; the model is warm for all subsequent turns.
        let (decision, speculative_recall) =
            tokio::join!(async { router_ref.route(&context_messages) }, async {
                if embed_is_warm {
                    retrieval_ref
                        .recall_relevant(engine_ref, &embed_model, content_ref)
                        .await
                } else {
                    warn!("Embed model not yet warm — skipping memory recall for this turn");
                    vec![]
                }
            },);

        // Round 3 / systemic fix: when a domain block triggers (iMessage, yt-dlp, etc.),
        // the query requires multi-step agentic execution (sqlite3 → contacts → format).
        // FAST (qwen3:8b) consistently fails at these chains. Override FAST → PRIMARY
        // for any query that loads a domain block, since domain blocks are only loaded
        // for complex, tool-dependent workflows.
        let matched_domains: Vec<String> = {
            let ql = content.to_lowercase();
            self.personality
                .profile()
                .domains
                .iter()
                .filter(|d| {
                    d.triggers
                        .iter()
                        .any(|t| !t.is_empty() && ql.contains(&t.to_lowercase()))
                })
                .map(|d| d.name.clone())
                .collect()
        };
        let mut decision = decision;
        if !matched_domains.is_empty() && matches!(decision.model, ModelId::Fast) {
            info!(
                session  = %self.session_id,
                trace_id = %trace_id,
                domains  = ?matched_domains,
                "Domain triggers matched — upgrading FAST → PRIMARY for agentic reliability"
            );
            decision.model = ModelId::Primary;
            decision.reasoning = format!(
                "{} [domain override: {} → PRIMARY]",
                decision.reasoning,
                matched_domains.join(", ")
            );
        }

        // Joke-request override.
        //
        // qwen3:8b (FAST) handles humor badly: every response opens with a
        // hedge preamble ("I'm not sure if this qualifies as a dad joke, but
        // here goes:") and the model recycles the same 2-3 jokes across
        // distinct requests. PRIMARY (gemma4:26b) is meaningfully better at
        // both variety and tone. The cost of routing trivial joke requests to
        // PRIMARY is one extra second of latency, vs the benefit of actual
        // humor instead of a recycled brothel/ladder joke.
        let joke_override_fired =
            matches!(decision.model, ModelId::Fast) && is_joke_request(&content);
        if joke_override_fired {
            info!(
                session  = %self.session_id,
                trace_id = %trace_id,
                "Joke request detected — upgrading FAST → PRIMARY for variety and tone"
            );
            decision.model = ModelId::Primary;
            decision.reasoning = format!(
                "{} [joke override: → PRIMARY for warmer delivery]",
                decision.reasoning
            );
            // Arm the joke-continuation window so iteration follow-ups
            // ("not NSFW enough", "explain the joke", "another one") stay
            // on PRIMARY instead of falling back to FAST.
            self.last_joke_turn_at = Some(Instant::now());
        }

        // Joke continuation override.
        //
        // Once a joke turn has happened, operator-style iterations don't
        // contain the word "joke" — they're criticism ("wasn't NSFW enough"),
        // correction ("doesn't need to be about a step dad"), or explanation
        // requests ("why is that a dirty joke?"). Without continuation,
        // these route to FAST and qwen3:8b either repeats its template or
        // hallucinates explanations of jokes that were never told.
        //
        // Symmetric to vision continuation: timestamp + reference detector +
        // bounded window. Only fires when the current decision is FAST (we
        // never demote PRIMARY-or-higher routes here).
        if matches!(decision.model, ModelId::Fast) {
            if let Some(last_joke) = self.last_joke_turn_at {
                let elapsed = last_joke.elapsed().as_secs();
                if elapsed <= crate::constants::JOKE_CONTINUATION_WINDOW_SECS
                    && is_joke_followup_reference(&content)
                {
                    info!(
                        session         = %self.session_id,
                        trace_id        = %trace_id,
                        secs_since_last = elapsed,
                        "Joke continuation: follow-up to recent joke turn — upgrading FAST → PRIMARY"
                    );
                    decision.model = ModelId::Primary;
                    decision.reasoning = format!(
                        "{} [joke continuation: iteration/criticism/explain marker within {}s window → PRIMARY]",
                        decision.reasoning,
                        crate::constants::JOKE_CONTINUATION_WINDOW_SECS
                    );
                    // Refresh the timestamp so a chain of iterations all stay
                    // on PRIMARY instead of expiring mid-conversation.
                    self.last_joke_turn_at = Some(Instant::now());
                }
            }
        }

        // Vision continuation override.
        //
        // The router classifies based on the current utterance alone. That's
        // correct for the FIRST turn that introduces an image ("take a look at
        // my screen") but wrong for follow-ups: "how big is that?" / "what
        // color?" / "show me another" all classify as Chat and route to FAST,
        // which is text-only and will hallucinate visual details.
        //
        // If the most recent successful Vision turn was within
        // `VISION_CONTINUATION_WINDOW_SECS` AND the new utterance contains an
        // anaphoric or visual-reference marker, override to Vision so the
        // existing capture+attach pipeline at line ~2956 fires again with a
        // fresh screen capture. Re-capturing (vs caching bytes) means follow-
        // ups about a *different* image still work — operator scrolls to a new
        // image, says "what about this one?", and the live screen state goes
        // to gemma4:26b.
        //
        // Only fires when the current decision is Chat-flavored (Fast or
        // Primary). Vision/Code/Heavy decisions are left alone.
        if matches!(decision.model, ModelId::Fast | ModelId::Primary) {
            if let Some(last_vision) = self.last_vision_turn_at {
                let elapsed = last_vision.elapsed().as_secs();
                if elapsed <= crate::constants::VISION_CONTINUATION_WINDOW_SECS
                    && is_vision_followup_reference(&content)
                {
                    let demoted_from = decision.model;
                    info!(
                        session         = %self.session_id,
                        trace_id        = %trace_id,
                        secs_since_last = elapsed,
                        from            = ?demoted_from,
                        "Vision continuation: follow-up to recent vision turn — upgrading to VISION"
                    );
                    decision.model = ModelId::Vision;
                    decision.category = crate::inference::router::Category::Vision;
                    decision.reasoning = format!(
                        "{} [vision continuation: anaphoric/visual reference within {}s window → VISION]",
                        decision.reasoning,
                        crate::constants::VISION_CONTINUATION_WINDOW_SECS
                    );
                }
            }
        }

        // Phase 37.7: surface sticky-inheritance provenance in the structured log.
        // `Some(cat)` means the routed category was inherited from a prior turn
        // because the current utterance was ambiguous; `None` means direct
        // classification was used (no-op inheritance is intentionally suppressed).
        info!(
            session           = %self.session_id,
            trace_id          = %trace_id,
            model             = decision.model.tier_name(),
            category          = ?decision.category,
            complexity        = decision.complexity.0,
            inherited_category = ?decision.inherited_category,
            reasoning         = %decision.reasoning,
            "Routing decision"
        );

        // Phase 15 / Phase 37.5 B5: HEAVY memory-swap orchestration.
        //
        // HEAVY (deepseek-r1:32b, ~19 GB) and PRIMARY (gemma4:26b MoE, ~18 GB)
        // cannot both be resident in the 36 GB unified-memory budget alongside
        // FAST (~5 GB) + EMBED (~0.7 GB) + OS/UI/workers. Ollama will try to
        // evict PRIMARY on HEAVY load, but the USB-SSD storage path makes that
        // unreliable: on live tests the operator saw HEAVY either fail to load
        // or take several minutes while Ollama re-shuffled VRAM. Explicit
        // pre-unload is more predictable than implicit eviction.
        //
        // Sequence:
        //   1. Await `unload_model(primary)` — tells Ollama `keep_alive: 0` for
        //      PRIMARY. The call returns once Ollama has released the weights.
        //   2. Clear `primary_model_warm` so the next PRIMARY-routed query
        //      doesn't assume warm latency.
        //   3. Set `pending_primary_rewarm = true`; `handle_generation_complete`
        //      uses this to spawn a warmup task after HEAVY unloads.
        //   4. Dispatch HEAVY with `unload_after = true` (existing behavior —
        //      see `ModelId::unload_after_use`).
        //   5. After HEAVY's generation result arrives, the post-gen handler
        //      spawns `warm_up_primary_model()` so the next chat turn hits a
        //      warm PRIMARY.
        //
        // Rewarm is spawned, not awaited: the operator is consuming HEAVY's
        // response output (TTS or text); PRIMARY can warm in parallel. If the
        // operator asks a follow-up before PRIMARY finishes rewarming, that
        // turn pays a partial cold-load cost — still better than keeping HEAVY
        // warm (which would lock PRIMARY out entirely) or never rewarming.
        if matches!(decision.model, ModelId::Heavy) {
            system::memory::warn_if_low_for_heavy();

            if self.primary_model_warm.load(Ordering::SeqCst) {
                let primary_name = self.model_config.primary.clone();
                info!(
                    session  = %self.session_id,
                    trace_id = %trace_id,
                    model    = %primary_name,
                    "HEAVY routed — unloading PRIMARY to free VRAM"
                );
                match self.engine.unload_model(&primary_name).await {
                    Ok(()) => {
                        self.primary_model_warm.store(false, Ordering::SeqCst);
                        self.pending_primary_rewarm = true;
                        info!(
                            session  = %self.session_id,
                            trace_id = %trace_id,
                            "PRIMARY unloaded — will rewarm after HEAVY completes"
                        );
                    }
                    Err(e) => {
                        // Non-fatal: HEAVY can still dispatch; Ollama will fall
                        // back to implicit eviction. Log and proceed. Do NOT set
                        // pending_primary_rewarm — PRIMARY may still be resident.
                        warn!(
                            session  = %self.session_id,
                            trace_id = %trace_id,
                            error    = %e,
                            "PRIMARY unload failed — proceeding with implicit eviction"
                        );
                    }
                }
            }

            // Phase 37.6 / Cluster-E diagnostics: probe Ollama's resident-model
            // state at HEAVY dispatch. Answers three different failure modes:
            //   1. PRIMARY didn't actually unload (unload_model returned OK but
            //      Ollama kept it resident) → HEAVY load will OOM
            //   2. HEAVY is already resident from a prior turn → fast path,
            //      no cold-load penalty expected
            //   3. Something ELSE is resident (stray model from a different
            //      app, previous CODE request that never unloaded) → explains
            //      mysterious memory pressure
            // Failures are diagnostic — never abort dispatch on a ps() error.
            match self.engine.ps().await {
                Ok(entries) => {
                    let summary: Vec<String> = entries
                        .iter()
                        .map(|e| {
                            let vram_gb = (e.size_vram as f64) / 1_073_741_824.0;
                            let size_gb = (e.size as f64) / 1_073_741_824.0;
                            let spill_pct = if e.size > 0 {
                                100.0 - (e.size_vram as f64 / e.size as f64 * 100.0)
                            } else {
                                0.0
                            };
                            format!(
                                "{}: vram={:.1}GB size={:.1}GB cpu_spill={:.0}%",
                                e.name, vram_gb, size_gb, spill_pct
                            )
                        })
                        .collect();
                    info!(
                        session   = %self.session_id,
                        trace_id  = %trace_id,
                        resident  = entries.len(),
                        models    = %summary.join(" | "),
                        "Ollama /api/ps — pre-HEAVY-dispatch snapshot"
                    );
                }
                Err(e) => {
                    warn!(
                        session  = %self.session_id,
                        trace_id = %trace_id,
                        error    = %e,
                        "Ollama /api/ps probe failed — continuing with HEAVY dispatch"
                    );
                }
            }

            // Phase 37.6 / Cluster-E: spawn a background probe to log /api/ps
            // 8s and 25s after dispatch starts. The first snapshot catches a
            // partial-load state (size_vram < size means HEAVY is paging from
            // disk through unified memory); the second snapshot catches load
            // completion (or still-partial, if the system is badly pressured).
            // Fire-and-forget — no join, no cancellation. At worst these log
            // lines arrive after HEAVY finishes, which is still useful telemetry.
            let engine_clone = self.engine.clone();
            let session_id = self.session_id.to_string();
            let trace_clone = trace_id.clone();
            tokio::spawn(async move {
                for (delay_secs, label) in [(8u64, "t+8s"), (25u64, "t+25s")] {
                    tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                    match engine_clone.ps().await {
                        Ok(entries) => {
                            let summary: Vec<String> = entries
                                .iter()
                                .map(|e| {
                                    let vram_gb = (e.size_vram as f64) / 1_073_741_824.0;
                                    let size_gb = (e.size as f64) / 1_073_741_824.0;
                                    let spill_pct = if e.size > 0 {
                                        100.0 - (e.size_vram as f64 / e.size as f64 * 100.0)
                                    } else {
                                        0.0
                                    };
                                    format!(
                                        "{}: vram={:.1}GB size={:.1}GB cpu_spill={:.0}%",
                                        e.name, vram_gb, size_gb, spill_pct
                                    )
                                })
                                .collect();
                            info!(
                                session   = %session_id,
                                trace_id  = %trace_clone,
                                at        = %label,
                                resident  = entries.len(),
                                models    = %summary.join(" | "),
                                "Ollama /api/ps — post-HEAVY-dispatch snapshot"
                            );
                        }
                        Err(e) => {
                            warn!(
                                session  = %session_id,
                                trace_id = %trace_clone,
                                at       = %label,
                                error    = %e,
                                "Ollama /api/ps post-dispatch probe failed"
                            );
                        }
                    }
                }
            });
        }

        // 3a. [Phase 19] Retrieval-first check — fires BEFORE routing for queries that
        //     are always stale in model memory (current time, software versions, etc.).
        //     When fired, the result is injected as a tool_result message into context
        //     BEFORE prepare_messages_for_inference() is called, so the model sees it.
        //
        //     Tracked with `retrieval_first_done` to avoid double-retrieval: if Phase 19
        //     already retrieved, Phase 9's detect_pre_trigger is skipped below.
        let retrieval_first_done = if is_retrieval_first_query(&content) {
            info!(
                session  = %self.session_id,
                trace_id = %trace_id,
                "Retrieval-first query detected — fetching before model call"
            );
            self.send_text(RETRIEVAL_ACKNOWLEDGMENT, false, &trace_id)
                .await?;
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(RETRIEVAL_TIMEOUT_SECS),
                self.retrieval.retrieve_web_only(&content),
            )
            .await;
            let injection = match result {
                Ok(Ok(r)) => {
                    info!(
                        session = %self.session_id,
                        source  = %r.source,
                        "Retrieval-first: result obtained"
                    );
                    format!(
                        "Retrieved fact for query '{query}':\n{text}\n(Source: {source})\n\n\
                         Use the retrieved fact above to answer the question. \
                         Do not speculate beyond it.",
                        query = content,
                        text = r.text,
                        source = r.source,
                    )
                }
                Ok(Err(e)) => {
                    warn!(session = %self.session_id, error = %e, "Retrieval-first: error");
                    format!(
                        "Retrieval for '{}' failed: {}. \
                         State that you cannot confirm this fact and why.",
                        content, e
                    )
                }
                Err(_timeout) => {
                    warn!(session = %self.session_id, "Retrieval-first: timeout");
                    format!(
                        "Retrieval for '{}' timed out. \
                         State that you cannot confirm this fact right now.",
                        content
                    )
                }
            };
            // Inject retrieval result — visible in all subsequent prepare_messages calls.
            // Role is "user" with a `[Retrieved]` prefix (see push_tool_result docs):
            // Ollama-compatible models only honor system/user/assistant roles; custom
            // roles like "retrieval" are silently dropped by base-instruct models.
            self.context.push_tool_result(&injection);
            true
        } else {
            false
        };

        // 3b. Pre-generation retrieval (Phase 9).
        //
        // Skipped if Phase 19 retrieval-first already fired (same query, no double fetch).
        // Otherwise: the router classified this as RetrievalFirst — retrieve from memory
        // + web BEFORE generating so the response is grounded in retrieved facts.
        let embed_model = self.model_config.embed.clone();
        let pre_retrieval_injection: Option<String> = if retrieval_first_done {
            None // Phase 19 already handled this
        } else if let Some(trigger) = self.retrieval.detect_pre_trigger(
            &content,
            matches!(decision.category, Category::RetrievalFirst),
        ) {
            self.send_text(RETRIEVAL_ACKNOWLEDGMENT, false, &trace_id)
                .await?;
            match self
                .retrieval
                .retrieve(&self.engine, &embed_model, &trigger)
                .await
            {
                Ok(ctx) => {
                    let text = self.retrieval.format_for_injection(&ctx);
                    if text.is_empty() {
                        None
                    } else {
                        Some(text)
                    }
                }
                Err(e) => {
                    warn!(
                        session = %self.session_id,
                        error   = %e,
                        "Pre-retrieval failed — generating without retrieved context"
                    );
                    None
                }
            }
        } else {
            None
        };

        // 3c. [Phase 21/24] Memory recall — use the speculative recall result from
        //     the tokio::join! above (Phase 24: parallel routing + recall).
        //
        //     Discarded when retrieval_first_done (already grounded in web content) or
        //     when routing to Vision (image provides primary context). In those cases
        //     the speculative embed call was wasted (~200ms) — acceptable for rare paths.
        let suppress_joke_recall =
            should_suppress_joke_memory_recall(&content, self.last_joke_turn_at);
        let recall_entries: Vec<crate::retrieval::store::MemoryEntry> = if retrieval_first_done
            || decision.model == ModelId::Vision
            || suppress_joke_recall
        {
            if suppress_joke_recall && !speculative_recall.is_empty() {
                info!(
                    session  = %self.session_id,
                    trace_id = %trace_id,
                    hits     = speculative_recall.len(),
                    "Joke context — suppressing semantic memory recall to avoid stale joke bleed-through"
                );
            }
            vec![] // Discard speculative recall
        } else {
            speculative_recall
        };

        // 4. Build message list via prepare_messages_for_inference().
        //
        // This replaces the previous inline steps 4 + 4b:
        //   [0] personality system prompt (PersonalityLayer + UNCERTAINTY_PROTOCOL)
        //   [1] Phase 16 context snapshot (current app/element, if known)
        //   [2+] conversation history (including any tool_result from step 3a)
        //
        // Both the original generation call and any re-prompt after retrieval MUST use
        // this helper — calling generate_stream with raw context skips personality and
        // context snapshot injection.
        let mut messages = self.prepare_messages_for_inference(&recall_entries);

        // 4c. [Phase 9] Inject Phase 9 retrieval context AFTER personality + context.
        //
        // Ordering: [0] personality  [1] context  [2] retrieval  [3..N] conversation
        // The retrieval_idx is computed by counting leading system messages so that
        // it's correct regardless of whether a context snapshot was injected.
        if let Some(injection) = pre_retrieval_injection {
            let retrieval_idx = messages.iter().take_while(|m| m.role == "system").count();
            messages.insert(
                retrieval_idx,
                crate::inference::engine::Message::system(injection),
            );
            info!(session = %self.session_id, "Pre-retrieval context injected into generation request");
        }

        // 4d. [Phase 20] Vision image attachment.
        //
        // When the router selected the Vision tier, capture the main display and attach
        // the base64-encoded PNG to the last user message before inference. This is done
        // on the ephemeral `messages` vec (not on `self.context`) — images are per-request
        // and must not be stored in conversation history or session state.
        //
        // On capture failure (screencapture unavailable, timeout, read error): fall through
        // with no image. The vision model will respond based on text context alone — degraded
        // but not blocking. The operator asked "look at this"; getting a text-only answer is
        // better than returning an error.
        if decision.model == ModelId::Vision {
            info!(session = %self.session_id, trace_id = %trace_id, "Vision query — capturing screen");
            // Phase 37.7: track whether an image actually lands on a real user
            // turn. If capture fails OR no target is found, demote the request
            // from Vision to Primary rather than sending a text-only payload
            // to the vision model (which produces fast, confident hallucinations
            // about a screen that isn't there).
            let mut image_attached = false;

            if let Some(image_b64) = self.capture_screen().await {
                // Attach to the last *genuine* user message, skipping any trailing
                // tool-result injections (retrieval, action output) that now ride on
                // role="user" per the Round 3 fix. Without this filter, a combined
                // retrieval+vision path would attach the screenshot to the synthetic
                // `[Retrieved] …` turn and the vision model would receive text-only.
                let target = messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == "user" && !is_tool_result_content(&m.content));
                if let Some(last_user) = target {
                    last_user.images = Some(vec![image_b64]);
                    image_attached = true;
                    // Vision continuation: arm the follow-up window. From this
                    // point until VISION_CONTINUATION_WINDOW_SECS elapses, any
                    // anaphoric/visual-reference utterance ("how big is that?",
                    // "what color is it?", "show me another") will re-route to
                    // Vision instead of falling through to text-only FAST.
                    self.last_vision_turn_at = Some(Instant::now());
                    debug!(session = %self.session_id, "Screen image attached to vision query message");
                } else {
                    warn!(
                        session = %self.session_id,
                        "Vision query had no genuine user message to attach image to"
                    );
                }
            }

            // Demotion gate: if no image landed on a user turn, the Vision route
            // is a lie. The router's published fallback for Vision is Primary
            // (see router.rs select_model), so demote to Primary and annotate
            // the decision so logs + downstream dispatch reflect reality.
            if !image_attached {
                let demoted_from = decision.model;
                decision.model = ModelId::Primary;
                decision.reasoning = format!(
                    "{} [demoted: vision requested but no image attachment — falling back to PRIMARY]",
                    decision.reasoning
                );
                warn!(
                    session   = %self.session_id,
                    trace_id  = %trace_id,
                    from      = ?demoted_from,
                    to        = ?decision.model,
                    "Vision demotion: no image attached, re-routing to PRIMARY"
                );
            }
        }

        // 5+6. Stream generation.
        //    ModelId::ollama_name() resolves the tier to the operator-configured Ollama tag.
        //    ModelId::unload_after_use() is true for Heavy, and for Vision when Vision
        //    resolves to a different model than PRIMARY (see unload_after_use docs).
        let model_name = decision.model.ollama_name(&self.model_config).to_string();
        let unload_after = decision.model.unload_after_use(&self.model_config);
        let needs_context_cap = decision.model.needs_context_cap();

        // Phase 10 — TTS: if available, spawn a concurrent synthesis task and wire
        // sentence detection into the generation loop via the unbounded channel.
        // Follow-up + retrieval generations always pass None — not narrated via TTS.
        let (tts_tx_opt, tts_join_handle) = self.make_tts_channel();

        // 5+6. Spawn the generation as a background task (Phase 27).
        //
        // The token streaming loop in run_generation_background can run for 30–120 s.
        // Spawning it returns control to the select! loop immediately so BargIn events
        // (and other ClientEvents) can be processed while generation is in progress.
        //
        // cancel_token: cloned into the task. handle_barge_in() stores `true` on the same
        // Arc, stopping the loop at its next per-token check. self.cancel_token is then
        // replaced with a fresh Arc so subsequent generations start with a clean token.
        //
        // Results arrive via generation_tx → gen_rx in server.rs → handle_generation_complete.
        let cancel_token = self.cancel_token.clone();
        let gen_tx = self.generation_tx.clone();
        let engine = self.engine.clone();
        let tx_bg = self.tx.clone();
        let session_id_bg = self.session_id.clone();
        let embed_model = self.model_config.embed.clone();
        // Phase 38 / Codex [7]: producer abort slot — populated by run_generation_background
        // after engine.generate_stream_cancellable returns Ok, drained by abort_active_generation.
        let producer_abort_bg = self.generation_producer_abort.clone();

        // Phase 0 (Adaptive Context Compiler): pre-build prompt-size telemetry.
        //
        // Measures the exact size of the assembled `messages` vector immediately
        // before dispatch — the only point where ALL context injection (system
        // prompt, conversation history, [Context:] / [Clipboard:] / [Memory:] /
        // [Shell:] blocks, retrieval, action schemas, etc.) is finalized.
        //
        // Goal: prove or disprove the prompt-bloat hypothesis BEFORE building
        // the context compiler. If FAST/PRIMARY prompts routinely exceed the
        // budget thresholds (FAST >1500, PRIMARY >3000, CODE/HEAVY >4000), the
        // compiler is mandatory. If they're consistently lean, the 20s prompt
        // eval observed on HEAVY is compute-bound and the compiler won't fix it.
        //
        // Char-count / 4 is a rough estimator; real tokenizer integration lands
        // in PR 2. Image payloads are excluded because Ollama processes them
        // separately and base64 length doesn't correspond to token cost.
        // Remove or downgrade to debug! once baseline data is collected.
        {
            let mut prompt_chars: usize = 0;
            for m in &messages {
                prompt_chars += m.role.chars().count() + m.content.chars().count() + 4;
                // role wrappers
            }
            let prompt_estimated_tokens = (prompt_chars / 4).max(1);
            info!(
                trace_id                = %trace_id,
                model                   = %model_name,
                category                = ?decision.category,
                complexity              = ?decision.complexity,
                message_count           = messages.len(),
                prompt_chars            = prompt_chars,
                prompt_estimated_tokens = prompt_estimated_tokens,
                "PHASE0 prompt size pre-dispatch"
            );
        }

        self.generation_handle = Some(tokio::spawn(run_generation_background(
            engine,
            tx_bg,
            session_id_bg,
            model_name,
            messages,
            trace_id,
            unload_after,
            needs_context_cap,
            tts_tx_opt,
            tts_join_handle,
            cancel_token,
            gen_tx,
            content,
            embed_model,
            0,                 // agentic_depth: user-initiated turn is depth 0
            producer_abort_bg, // Phase 38 / Codex [7]
        )));

        Ok(())
    }

    /// Stream a generation to the UI and return the accumulated response text.
    ///
    /// Sends each token as a non-final `TextResponse`. Sends the final `is_final=true`
    /// marker on completion. Accumulates the full text and returns it.
    ///
    /// Does NOT: check for uncertainty markers, parse action blocks, push to
    /// `ConversationContext`, or record in `SessionStateManager` — callers do all
    /// of that after inspecting the returned text.
    ///
    /// `model_name` is explicit so the primary generation uses the router-decided tier
    /// while the uncertainty follow-up explicitly selects PRIMARY.
    ///
    /// `unload_after` = true only for Heavy model calls — keeps VRAM headroom free.
    ///
    /// `tts_tx` — when `Some`, sentences are detected as tokens arrive and sent on this
    /// channel for concurrent TTS synthesis. The sender is MOVED in and DROPPED when this
    /// function returns, which closes the channel and signals the TTS task to exit.
    /// Pass `None` for follow-up generations (uncertainty grounding) and retrieval passes.
    ///
    /// Returns `Err(ChannelClosed)` only when the gRPC send channel is closed. All
    /// inference errors are handled internally (logged + best-effort error message to UI)
    /// and result in `Ok("")` — the caller continues with an empty response.
    async fn generate_and_stream(
        &mut self,
        model_name: &str,
        messages: Vec<crate::inference::engine::Message>,
        trace_id: &str,
        unload_after: bool,
        tts_tx: Option<UnboundedSender<String>>, // MOVED in; dropped when fn returns
    ) -> Result<String, OrchestratorError> {
        let req = GenerationRequest {
            model_name: model_name.to_string(),
            messages,
            temperature: None,
            unload_after,
            keep_alive_override: if unload_after {
                None
            } else {
                Some(FAST_MODEL_KEEP_ALIVE)
            },
            num_predict: None,
            inactivity_timeout_override_secs: None,
            num_ctx_override: if unload_after {
                Some(LARGE_MODEL_NUM_CTX)
            } else {
                None
            },
        };

        let mut full_response = String::new();

        // SentenceSplitter is instantiated ONCE here, before the token loop.
        // It is cheap (just a String buffer). Always created; only used when tts_tx is Some.
        // Do NOT move this inside the loop body — it holds inter-token accumulation state.
        let mut splitter = SentenceSplitter::new();

        match self.engine.generate_stream(req).await {
            Err(e) => {
                // generate_stream can fail before streaming starts (network unreachable,
                // model not found). Send a terminal is_final=true response so Swift
                // doesn't display a hanging spinner indefinitely.
                error!(
                    session  = %self.session_id,
                    trace_id = %trace_id,
                    error    = %e,
                    "generate_stream failed before streaming started"
                );
                self.send_text("(inference error — check core logs)", true, trace_id)
                    .await?;
            }
            Ok(mut rx) => {
                while let Some(chunk_result) = rx.recv().await {
                    match chunk_result {
                        Ok(chunk) if !chunk.done => {
                            full_response.push_str(&chunk.content);
                            self.send_text(&chunk.content, false, trace_id).await?;

                            // Sentence detection: only when a TTS sender is present.
                            // splitter.push() may return 0, 1, or more complete sentences per token.
                            if let Some(ref tx) = tts_tx {
                                for sentence in splitter.push(&chunk.content) {
                                    let _ = tx.send(sentence); // UnboundedSender::send never blocks
                                }
                            }
                        }
                        Ok(_done_chunk) => {
                            // Flush remaining buffered text as a final sentence.
                            if let Some(ref tx) = tts_tx {
                                if let Some(remainder) = splitter.flush() {
                                    let _ = tx.send(remainder);
                                }
                            }
                            self.send_text("", true, trace_id).await?;
                            break;
                        }
                        Err(e) => {
                            error!(
                                session  = %self.session_id,
                                trace_id = %trace_id,
                                error    = %e,
                                "Stream chunk error mid-generation"
                            );
                            // Flush on error too so TTS gets what it can.
                            if let Some(ref tx) = tts_tx {
                                if let Some(remainder) = splitter.flush() {
                                    let _ = tx.send(remainder);
                                }
                            }
                            // Best-effort: send is_final=true so Swift exits streaming mode.
                            let _ = self.send_text("", true, trace_id).await;
                            break;
                        }
                    }
                }
            }
        }

        // tts_tx (the owned Option<UnboundedSender<String>>) is dropped here as this
        // function returns. Dropping the sender closes the unbounded channel. The TTS task's
        // recv() loop then sees None and exits.
        Ok(full_response)
    }

    /// Handle a `SystemEvent` by updating the `ContextObserver` snapshot.
    ///
    /// All six `SystemEventType` variants are handled (compiler enforces exhaustiveness).
    /// Logging is conditional on `snapshot_hash` change — prevents log spam when the
    /// same app re-fires an APP_FOCUSED event without meaningful state change.
    ///
    /// No response is sent to Swift for system events — they are informational only.
    /// Context injection into inference happens in Phase 9+ via `context_summary()`.
    async fn handle_system_event(
        &mut self,
        sys: SystemEvent,
        trace_id: String,
    ) -> Result<(), OrchestratorError> {
        match SystemEventType::try_from(sys.r#type) {
            Ok(SystemEventType::Connected) => {
                info!(session = %self.session_id, "Operator connected — context observer active");
                // Orchestrator-level reset — but ONLY if we're already Idle. [Phase 37 / B1]
                //
                // The initial IDLE is already sent by `server.rs::stream_audio` at session open
                // (before any warmup or greeting runs). Re-sending here used to be a harmless
                // no-op because Connected arrived before `send_startup_greeting()` could transition
                // us out of Idle.
                //
                // After Phase 36 moved PRIMARY warmup BEFORE the greeting (sequential warmup for
                // a truthful "Ready." signal), the greeting's SPEAKING state now often arrives
                // BEFORE Swift's Connected event. Unconditionally sending IDLE here overwrites
                // that SPEAKING and silences the TTS round-trip: the `speak_action_feedback`
                // task is still queuing AudioResponse frames, but Swift has already been told
                // the entity is Idle and treats them as stale.
                //
                // Guard: only re-send IDLE if current_state is already Idle (making this a
                // confirmation, not an override). When we're SPEAKING/THINKING for a legitimate
                // reason (the greeting, or any future pre-Connected activity), leave state alone
                // and let the natural APC → IDLE round-trip close the loop.
                if self.current_state == EntityState::Idle {
                    self.send_state(EntityState::Idle, &trace_id).await?;
                }
                // Phase 18: push session config so Swift can configure OS-level observers.
                // Sent immediately after IDLE so EventBridge has the correct hotkey parameters
                // before the operator can press any keys.
                let hk = self.hotkey_config.clone();
                let sync_event = ServerEvent {
                    trace_id: trace_id.clone(),
                    event: Some(server_event::Event::ConfigSync(
                        crate::ipc::proto::ConfigSync {
                            hotkey: Some(crate::ipc::proto::HotkeyConfig {
                                key_code: hk.key_code,
                                ctrl: hk.ctrl,
                                shift: hk.shift,
                                cmd: hk.cmd,
                                option: hk.option,
                            }),
                        },
                    )),
                };
                self.tx
                    .send(Ok(sync_event))
                    .await
                    .map_err(|_| OrchestratorError::ChannelClosed)?;
                debug!(
                    session  = %self.session_id,
                    key_code = hk.key_code,
                    ctrl     = hk.ctrl,
                    shift    = hk.shift,
                    "ConfigSync pushed to Swift shell"
                );
            }
            Ok(SystemEventType::AppFocused) => {
                let changed = self.context_observer.update_from_app_focused(&sys.payload);
                if changed {
                    info!(
                        session = %self.session_id,
                        app     = ?self.context_observer.snapshot().app_name,
                        bundle  = ?self.context_observer.snapshot().app_bundle_id,
                        element = ?self.context_observer.snapshot().focused_element,
                        hash    = self.context_observer.snapshot().snapshot_hash,
                        "Context snapshot updated (app focused)"
                    );

                    // Phase 17: Proactive observation.
                    //
                    // When the operator switches to a new app and all rate-limiting gates
                    // pass, fire a brief ambient observation using the FAST model.
                    // `do_proactive_response` collects the full response before displaying
                    // it — so the [SILENT] opt-out suppresses output before any tokens
                    // reach the UI or TTS.
                    // Proactive observations require the FAST model to be resident.
                    // The model is always warm by the time the select! loop runs
                    // (startup awaits the JoinHandle), but this guard defends
                    // against any stale context events processed before "Ready.".
                    if self.fast_model_warm.load(Ordering::Relaxed) {
                        if let Some(summary) = self.context_observer.context_summary() {
                            // Phase 35: suppress proactive when the terminal is showing
                            // Dexter's own build/run output. AX element content for iTerm2
                            // running `make run` will contain these markers; commenting on
                            // your own startup logs is confusing and unhelpful.
                            let is_own_output = summary.contains("[DexterClient]")
                                || summary.contains("dexter-core")
                                || summary.contains("dexter_core")
                                || summary.contains("make run");
                            if !is_own_output
                                && self
                                    .proactive_engine
                                    .should_fire(self.context_observer.snapshot())
                            {
                                self.proactive_engine.record_fire();
                                self.do_proactive_response(&summary, &trace_id).await?;
                            }
                        }
                    }

                    // Phase 24: re-warm KV cache with updated context.
                    // The next voice query will find its full prefix already cached.
                    // Debounce inside prefill_inference_cache prevents flood.
                    self.prefill_inference_cache();
                }
            }
            Ok(SystemEventType::AppUnfocused) => {
                // The next APP_FOCUSED for the newly active app updates the snapshot.
                info!(session = %self.session_id, "App unfocused");
            }
            Ok(SystemEventType::ScreenLocked) => {
                self.context_observer.set_screen_locked(true);
                info!(session = %self.session_id, "Screen locked — context observation paused");
            }
            Ok(SystemEventType::AxElementChanged) => {
                let changed = self
                    .context_observer
                    .update_from_element_changed(&sys.payload);
                if changed {
                    info!(
                        session = %self.session_id,
                        element = ?self.context_observer.snapshot().focused_element,
                        hash    = self.context_observer.snapshot().snapshot_hash,
                        "Context snapshot updated (element changed)"
                    );

                    // Phase 24: re-warm KV cache when the focused element changes.
                    // Debounced at 5s inside prefill_inference_cache — fires at most
                    // once per 5 seconds even if AXElementChanged floods every keystroke.
                    self.prefill_inference_cache();
                }
            }
            Ok(SystemEventType::ScreenUnlocked) => {
                self.context_observer.set_screen_locked(false);
                info!(session = %self.session_id, "Screen unlocked — context observation resumed");
            }
            Ok(SystemEventType::ClipboardChanged) => {
                // Phase 28: operator copied text. Update ContextObserver; content is
                // injected passively into the next inference request. No proactive trigger.
                let changed = self
                    .context_observer
                    .update_from_clipboard_changed(&sys.payload);
                if changed {
                    info!(
                        session    = %self.session_id,
                        char_count = self.context_observer.snapshot()
                                         .clipboard_text.as_deref()
                                         .map(|t| t.chars().count())
                                         .unwrap_or(0),
                        "Clipboard context updated"
                    );
                }
            }
            Ok(SystemEventType::HotkeyActivated) => {
                // Phase 16: Global hotkey pressed — transition entity to LISTENING.
                // VoiceCapture is always-on (Phase 13); this is an attention signal only.
                // The operator's next VAD-detected utterance is processed normally.
                //
                // Round 3 / T0.2: the hotkey is the operator's ONLY interrupt surface
                // in the Phase 34 push-to-talk model (Swift-side BargIn was removed).
                // If a generation is in-flight — e.g. a heavy model stuck on a 4-minute
                // deepseek run, or a hallucinating continuation — the operator expects
                // the hotkey to reclaim attention. Abort the background task first,
                // then transition to LISTENING. This mirrors `handle_barge_in` but
                // gates on there actually being a task to cancel: the cancel token
                // is only swapped when we aborted something, so callers that tap the
                // hotkey as a pure attention signal don't churn the token.
                info!(session = %self.session_id, trace_id = %trace_id, "Global hotkey activated");

                // Phase 38 / Codex [7]+[9]+[10]: unified cancel via the helper.
                // The helper is a no-op when nothing is in flight (matches the prior
                // "gates on there actually being a task to cancel" semantics — token
                // is only swapped when something was actually aborted).
                let info_session = self.session_id.clone();
                let info_state = self.current_state;
                let was_aborted = self.abort_active_generation();
                if was_aborted {
                    info!(
                        session  = %info_session,
                        trace_id = %trace_id,
                        state    = ?info_state,
                        "Hotkey aborting in-flight generation"
                    );
                    self.maybe_rewarm_primary(true, &trace_id);
                }

                self.send_state(EntityState::Listening, &trace_id).await?;

                // Phase 24 (Solution 1): begin KV cache prefill while operator speaks.
                // The system prompt + context snapshot won't change during this utterance.
                // If the environmental prefill (Solution 2) already warmed the cache and
                // the context hasn't changed, Ollama's KV cache already has the exact
                // prefix — this completes in <10ms (no new tokens to process).
                //
                // Phase 24: also demote any in-flight interactions to Background priority
                // so a new interaction takes TTS scheduling priority.
                for interaction in self.interactions.values_mut() {
                    if interaction.priority == InteractionPriority::Active {
                        interaction.priority = InteractionPriority::Background;
                    }
                }
                self.prefill_inference_cache();
            }
            Ok(SystemEventType::AudioPlaybackComplete) => {
                // Phase 18/19: Swift signals that TTS audio (proactive or regular-response)
                // has finished playing. This is the correct IDLE trigger — after playback
                // completes, not after synthesis completes.
                //
                // Phase 19: if an action is awaiting operator approval, stay in ALERT.
                // handle_action_approval() will clear the flag and send IDLE when the
                // operator responds. Sending IDLE here would cancel the ALERT state.
                //
                // Phase 27: if the entity is already LISTENING (barge-in transitioned it),
                // suppress this IDLE transition. Race: AudioPlayer.stop() can fire
                // onPlaybackFinished (→ AUDIO_PLAYBACK_COMPLETE) even after barge-in if the
                // is_final sentinel was already queued. The check prevents that spurious
                // callback from overriding the LISTENING state the operator needs.
                if self.action_awaiting_approval {
                    info!(
                        session  = %self.session_id,
                        trace_id = %trace_id,
                        "TTS playback complete — action pending approval, remaining in ALERT"
                    );
                } else {
                    // Always transition to IDLE when TTS audio finishes.
                    //
                    // Phase 27 originally suppressed this when current_state == Listening
                    // to avoid AUDIO_PLAYBACK_COMPLETE from overriding the LISTENING state
                    // during barge-in. That suppression was removed because it caused
                    // permanent entity lockup:
                    //
                    // make_tts_channel sends EntityState::Speaking from a background task
                    // without going through send_state(), so current_state is never updated
                    // to Speaking. If ambient noise triggers a false barge-in during TTS
                    // (possible with the reduced VAD threshold), Rust sends EntityState::Listening
                    // via send_state() (updating current_state = Listening), then the TTS task
                    // sends EntityState::Speaking *after*. Swift ends up showing SPEAKING while
                    // Rust has current_state = Listening. Any AUDIO_PLAYBACK_COMPLETE then hits
                    // the suppression and is silently dropped — entity locked in SPEAKING forever.
                    //
                    // Functional safety: isActive is set directly by VoiceCapture.onSpeechStart
                    // during barge-in (not by the LISTENING event), so the barge-in utterance is
                    // captured regardless of whether IDLE fires here. The entity will briefly
                    // show IDLE then THINKING/SPEAKING as the new inference runs — cosmetically
                    // imperfect but functionally correct and not permanently stuck.
                    info!(
                        session      = %self.session_id,
                        trace_id     = %trace_id,
                        prior_state  = ?self.current_state,
                        "TTS playback complete — transitioning to IDLE"
                    );
                    self.send_state(EntityState::Idle, &trace_id).await?;
                }
            }
            Ok(SystemEventType::Unspecified) | Err(_) => {
                warn!(
                    session = %self.session_id,
                    kind    = sys.r#type,
                    "Unknown SystemEventType — ignored"
                );
            }
        }
        Ok(())
    }

    /// Handle a `UIAction` (dismiss / drag / resize).
    ///
    /// Currently all UIAction types are logged and acknowledged. Phase 16 uses
    /// SystemEvent for hotkey activation. Future phases may add real handlers
    /// for DRAG (persist window position) and RESIZE.
    /// No silent drop — the action is always logged so it can be audited.
    async fn handle_ui_action(
        &mut self,
        action: UiAction,
        trace_id: String,
    ) -> Result<(), OrchestratorError> {
        info!(
            session  = %self.session_id,
            trace_id = %trace_id,
            ui_type  = action.r#type,
            payload  = %action.payload,
            "UIAction received (deferred to Phase 11)"
        );
        Ok(())
    }

    /// Handle an `ActionApproval` (operator confirmed or rejected an ActionRequest).
    ///
    /// Resolves the pending action in `ActionEngine`. Returns the entity to IDLE
    /// regardless of approval outcome — the ALERT state set in `handle_text_input`
    /// is only cleared here.
    async fn handle_action_approval(
        &mut self,
        approval: ActionApproval,
        trace_id: String,
    ) -> Result<(), OrchestratorError> {
        info!(
            session   = %self.session_id,
            trace_id  = %trace_id,
            action_id = %approval.action_id,
            approved  = approval.approved,
            note      = %approval.operator_note,
            "ActionApproval received"
        );

        let outcome = self
            .action_engine
            .resolve(
                &approval.action_id,
                approval.approved,
                &approval.operator_note,
            )
            .await;

        // Phase 19: clear the action guard before any awaits so that if something
        // goes wrong the ALERT state can still be cleared by future events.
        self.action_awaiting_approval = false;

        // Speak a brief result to the operator and, when TTS is available, let
        // AUDIO_PLAYBACK_COMPLETE drive the IDLE transition (same as regular TTS).
        let spoke = match outcome {
            ActionOutcome::Completed {
                action_id, output, ..
            } => {
                info!(
                    session   = %self.session_id,
                    action_id = %action_id,
                    output    = %output,
                    "Approved action completed"
                );
                // Phase 9+: inject action result into conversation context
                let feedback = if output.trim().is_empty() {
                    "Done.".to_string()
                } else {
                    output.clone()
                };
                self.speak_action_feedback(&feedback, &trace_id).await?
            }
            ActionOutcome::Rejected { action_id, error } => {
                info!(
                    session   = %self.session_id,
                    action_id = %action_id,
                    error     = %error,
                    "Action rejected"
                );
                self.speak_action_feedback(&format!("Action cancelled: {error}"), &trace_id)
                    .await?
            }
            ActionOutcome::PendingApproval { .. } => {
                // resolve() should never return PendingApproval — this is a logic error.
                warn!(session = %self.session_id, "resolve() returned PendingApproval — logic error");
                false
            }
        };

        // When TTS spoke the feedback, AUDIO_PLAYBACK_COMPLETE will drive IDLE.
        // When TTS was unavailable (spoke=false), send IDLE directly.
        if !spoke {
            self.send_state(EntityState::Idle, &trace_id).await?;
        }
        Ok(())
    }

    // ── Phase 24: background action result handling ───────────────────────────

    /// Handle a result delivered by a background action task via `action_rx`.
    ///
    /// Looks up the `Interaction` by `action_id`. If found, transitions to
    /// `FeedbackPending` and speaks the result via TTS. If not found (GC'd or
    /// unknown), logs a warning and discards the result — no panic, no state
    /// transition.
    pub async fn handle_action_result(
        &mut self,
        result: ActionResult,
    ) -> Result<(), OrchestratorError> {
        // Phase 38 / Codex finding [10]: action completed naturally — drop its
        // tracked JoinHandle from the in-flight map. A subsequent cancel won't
        // try to abort a finished task. Action handles for unknown interactions
        // are still removed (defensive — keeps the map bounded even if the
        // interactions map drops the entry first for some reason).
        self.in_flight_actions.remove(&result.action_id);

        let interaction = match self.interactions.get_mut(&result.action_id) {
            Some(i) => i,
            None => {
                warn!(
                    session   = %self.session_id,
                    action_id = %result.action_id,
                    "Action result for unknown interaction — discarding"
                );
                return Ok(());
            }
        };

        // Maximum characters of action output injected into conversation context.
        // Large outputs (e.g. `ps aux | head -20` → ~1500 chars of process list) would
        // flood the HUD when the continuation model echoes them back, and bloat the
        // context window. Truncate to a summary-friendly size; the model gets the most
        // important lines (beginning) plus a clear signal that output was trimmed.
        //
        // Phase 36: bumped 1200 → 4000. The previous limit was tuned for `ps aux`-style
        // process lists (intermediate context), but it cuts off iMessage sqlite3 results
        // mid-conversation — and those results ARE the deliverable, not noise. 4000 chars
        // ≈ 5–7 message thread + handle resolution, still well under prompt budget.
        const ACTION_OUTPUT_MAX_CHARS: usize = 4_000;

        let (feedback_text, rewritten_to) = match &result.outcome {
            ActionOutcome::Completed {
                output,
                rewritten_to,
                ..
            } => {
                let trimmed = output.trim();
                let text = if trimmed.is_empty() {
                    "Done.".to_string()
                } else if trimmed.chars().count() > ACTION_OUTPUT_MAX_CHARS {
                    let truncated: String = trimmed.chars().take(ACTION_OUTPUT_MAX_CHARS).collect();
                    let char_total = trimmed.chars().count();
                    warn!(
                        session    = %self.session_id,
                        char_count = char_total,
                        limit      = ACTION_OUTPUT_MAX_CHARS,
                        "Action output truncated before context injection"
                    );
                    format!("{truncated}\n… (output truncated, {char_total} chars total)")
                } else {
                    trimmed.to_string()
                };
                (text, rewritten_to.clone())
            }
            ActionOutcome::Rejected { action_id, error } => {
                error!(
                    session   = %self.session_id,
                    action_id = %action_id,
                    error     = %error,
                    "Background action failed"
                );
                (format!("Action failed: {error}"), None)
            }
            ActionOutcome::PendingApproval { .. } => {
                // ExecutorHandle::execute never returns PendingApproval — logic error.
                warn!(session = %self.session_id, "Background action returned PendingApproval — logic error");
                return Ok(());
            }
        };

        interaction.stage = InteractionStage::FeedbackPending;

        // Inject the action outcome into conversation context so the model knows
        // what happened on the next turn. Without this, every subsequent turn
        // starts cold — the model has no memory that a browser navigated, what
        // URL it landed on, or what a shell command returned.
        //
        // Rejected outcomes are equally important: the error message is the only
        // signal the model has to change strategy (e.g. "URL not found" → do a
        // browser search first instead of fabricating a URL).
        //
        // `rewritten_to` is Some when a GNU-syntax shell command was transparently
        // normalized to BSD before execution (e.g. ps with --sort). Including the
        // normalized BSD form in the tool result label means the model sees the
        // correct command when it inspects context for "what did you run?" — without
        // this annotation it reads its own assistant message (the original GNU form).
        //
        // Mirrors the retrieval injection at line ~1120: push_tool_result adds
        // a synthetic "user" message (role="user" is Ollama-universal; see
        // push_tool_result doc for the Round 3 regression that forced this).
        // The `[Action result]` / `[Action FAILED]` prefix is what disambiguates
        // tool output from real operator input inside the prompt.
        let tool_label = match &result.outcome {
            ActionOutcome::Completed { .. } => "[Action result",
            ActionOutcome::Rejected { .. } => "[Action FAILED",
            _ => "[Action result",
        };
        let label_text = if let Some(ref cmd) = rewritten_to {
            format!("{tool_label} (normalized to macOS BSD: `{cmd}`): {feedback_text}]")
        } else {
            format!("{tool_label}: {feedback_text}]")
        };
        self.context.push_tool_result(&label_text);

        // Phase 32: Agentic continuation — after injecting the action result, spawn a
        // follow-up generation so the model can issue the next action or respond with
        // completion text. The chain continues until:
        //   a) The model responds with no action block → IDLE via handle_generation_complete
        //   b) agentic_depth >= AGENTIC_MAX_DEPTH → bail with an explanation
        //   c) Barge-in cancels the ongoing generation (cancel_token path)
        //
        // No speak_action_feedback is called for intermediate steps — the model's own
        // response text (before the next action block, if any) is what the operator
        // hears. This avoids a robotic "Done." between every step in a multi-step task.

        let agentic_depth = interaction.agentic_depth;
        let original_content = interaction.original_content.clone();
        let is_terminal_workflow = interaction.is_terminal_workflow;
        interaction.stage = InteractionStage::Complete;

        info!(
            session       = %self.session_id,
            action_id     = %result.action_id,
            trace_id      = %result.trace_id,
            agentic_depth = agentic_depth,
            "Background action result — spawning continuation generation"
        );

        // Phase 36 / H3 fix: terminal workflows (e.g. iMessage send via osascript)
        // produce no observable output on success, so the continuation model speculates
        // a "retry" by re-emitting the same action block — the operator then sees a
        // phantom second send attempt. For these actions, when the outcome is
        // Completed we short-circuit with a fixed acknowledgment. Rejected outcomes
        // fall through to the normal continuation so the model can diagnose the error.
        if is_terminal_workflow {
            if let ActionOutcome::Completed { .. } = &result.outcome {
                info!(
                    session   = %self.session_id,
                    action_id = %result.action_id,
                    trace_id  = %result.trace_id,
                    "Terminal-workflow action completed — skipping continuation"
                );
                let msg = "Sent.";
                let spoke = self.speak_action_feedback(msg, &result.trace_id).await?;
                if !spoke {
                    self.send_state(EntityState::Idle, &result.trace_id).await?;
                }
                return Ok(());
            }
        }

        if agentic_depth >= AGENTIC_MAX_DEPTH {
            warn!(
                session       = %self.session_id,
                agentic_depth = agentic_depth,
                max           = AGENTIC_MAX_DEPTH,
                "Agentic chain exceeded max depth — stopping"
            );
            let msg = "I've taken several steps but couldn't finish. Let me know how you'd like to proceed.";
            let spoke = self.speak_action_feedback(msg, &result.trace_id).await?;
            if !spoke {
                self.send_state(EntityState::Idle, &result.trace_id).await?;
            }
            return Ok(());
        }

        // Build continuation message list — context now includes all prior action
        // results and assistant turns from this chain. No extra user message is
        // appended: the model sees the tool result at the tail and continues naturally.
        let messages = self.prepare_messages_for_inference(&[]);

        self.send_state(EntityState::Thinking, &result.trace_id)
            .await?;

        let (tts_tx_opt, tts_join_handle) = self.make_tts_channel();
        let cancel_token = self.cancel_token.clone();
        let gen_tx = self.generation_tx.clone();
        let engine = self.engine.clone();
        let tx_bg = self.tx.clone();
        let session_id_bg = self.session_id.clone();
        let embed_model = self.model_config.embed.clone();
        // Phase 38 / Codex [7]: producer abort slot — see analogous comment in
        // the user-initiated dispatch above.
        let producer_abort_bg = self.generation_producer_abort.clone();
        // Always use FAST model for agentic steps — better instruction-following
        // for structured action format; complexity is low (next-step selection).
        let model_name = self.model_config.fast.clone();

        self.generation_handle = Some(tokio::spawn(run_generation_background(
            engine,
            tx_bg,
            session_id_bg,
            model_name,
            messages,
            result.trace_id.clone(),
            false, // unload_after: FAST model stays pinned
            false, // needs_context_cap: FAST fits in VRAM at native context
            tts_tx_opt,
            tts_join_handle,
            cancel_token,
            gen_tx,
            original_content, // content = original user query for memory embedding
            embed_model,
            agentic_depth, // passed through so handle_generation_complete can record it
            producer_abort_bg, // Phase 38 / Codex [7]
        )));

        Ok(())
    }

    /// Remove interactions older than `INTERACTION_TTL_SECS`.
    ///
    /// Called periodically from the health-check timer branch in `ipc::server`.
    /// Catches silently-panicked action tasks that never delivered a result.
    pub fn gc_stale_interactions(&mut self) {
        let before = self.interactions.len();
        self.interactions.retain(|_, i| {
            i.created_at.elapsed() < std::time::Duration::from_secs(INTERACTION_TTL_SECS)
        });
        let removed = before - self.interactions.len();
        if removed > 0 {
            info!(
                session = %self.session_id,
                removed = removed,
                remaining = self.interactions.len(),
                "Interaction GC sweep"
            );
        }
    }

    /// Expose the `action_tx` sender so `ipc::server` can create the channel
    /// externally and pass the receiver to the `select!` loop.
    #[allow(dead_code)] // Phase 24c: used when fast-path transcript delivery needs action_tx
    pub fn action_tx(&self) -> &mpsc::Sender<ActionResult> {
        &self.action_tx
    }

    /// Expose an `ExecutorHandle` for spawning background action tasks.
    #[allow(dead_code)] // Used in handle_text_input via action_engine.executor_handle()
    pub fn executor_handle(&self) -> ExecutorHandle {
        self.action_engine.executor_handle()
    }

    // ── Phase 19 helpers ──────────────────────────────────────────────────────

    /// Build the complete message list for a `generate_stream` call.
    ///
    /// Applies (in order):
    ///   1. Personality system prompt — `PersonalityLayer::apply_to_messages()` inserts
    ///      or merges the system prompt at index 0 (includes UNCERTAINTY_PROTOCOL block).
    ///   2. Current context snapshot — Phase 16 injection, inserted at index 1
    ///      (after the system message, before conversation history).
    ///   3. Conversation history — remaining messages from `self.context`.
    ///
    /// Both the original generation call and the re-prompt after retrieval MUST use
    /// this helper. Calling `generate_stream` with `&self.context.messages()` directly
    /// skips steps 1 and 2, producing an out-of-persona response that lacks the
    /// uncertainty protocol instructions and current machine context.
    pub(crate) fn prepare_messages_for_inference(
        &self,
        recall: &[crate::retrieval::store::MemoryEntry],
    ) -> Vec<crate::inference::engine::Message> {
        // Round 3 / T1.2: extract the last genuine user message to drive domain-block
        // selection in the personality layer. Tool-result injections (identified by
        // is_tool_result_content) are skipped — they don't represent operator intent.
        // For an agentic continuation with no trailing genuine query, we fall back to
        // the Interaction's `original_content` (the query that started the chain).
        let query_hint: Option<&str> = self
            .context
            .messages()
            .iter()
            .rev()
            .find(|m| m.role == "user" && !is_tool_result_content(&m.content))
            .map(|m| m.content.as_str());
        let matched = query_hint
            .map(|q| {
                self.personality
                    .profile()
                    .domains
                    .iter()
                    .filter(|d| {
                        let ql = q.to_lowercase();
                        d.triggers
                            .iter()
                            .any(|t| !t.is_empty() && ql.contains(&t.to_lowercase()))
                    })
                    .map(|d| d.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !matched.is_empty() {
            debug!(
                session    = %self.session_id,
                domains    = ?matched,
                "Domain blocks loaded for this turn"
            );
        }

        // Step 1: apply personality — returns a new Vec with system prompt at index 0.
        //         The hint conditionally injects matching domain blocks (T1.2).
        let mut messages = self
            .personality
            .apply_to_messages_for(self.context.messages(), query_hint);

        // Step 2a: always inject the current wall-clock time.
        // qwen3 has no real-time clock — without this it confidently hallucinates the time
        // from training-data patterns.  chrono::Local::now() is pure Rust, zero overhead.
        {
            let ts = chrono::Local::now()
                .format("%a %b %-d %Y %-I:%M %p %Z")
                .to_string();
            let pos = if messages
                .first()
                .map(|m| m.role.as_str() == "system")
                .unwrap_or(false)
            {
                1
            } else {
                0
            };
            // Natural-language sentence rather than "DateTime: X" colon-label format.
            // If the model echoes "The current time is X" it reads as a valid answer;
            // if it echoed the old "DateTime: X" label, stripping left an empty response.
            messages.insert(
                pos,
                crate::inference::engine::Message::system(format!("The current time is {ts}.")),
            );
        }

        // Step 2b: inject Phase 16 context snapshot (app / element) after the DateTime line.
        if let Some(summary) = self.context_observer.context_summary() {
            let insert_pos = if messages
                .first()
                .map(|m| m.role.as_str() == "system")
                .unwrap_or(false)
            {
                1
            } else {
                0
            };
            messages.insert(
                insert_pos,
                crate::inference::engine::Message::system(format!("Context: {summary}")),
            );
            debug!(
                session = %self.session_id,
                context = %summary,
                "Context snapshot injected into inference request"
            );
        }

        // Step 2c: turn-scoped comedy instruction.
        //
        // The core personality already says not to refuse, but aligned local
        // models still reflexively refuse or sanitize identity-themed joke
        // requests ("tell me a gay joke") unless the comedy task is made
        // explicit near the active turn.
        // Keep this scoped to actual joke requests and recent joke follow-ups so it
        // does not weaken serious safety-sensitive action/tool prompts.
        let comedy_mode_active = query_hint
            .map(|q| {
                is_joke_request(q)
                    || self
                        .last_joke_turn_at
                        .map(|last_joke| {
                            last_joke.elapsed().as_secs()
                                <= crate::constants::JOKE_CONTINUATION_WINDOW_SECS
                                && is_joke_followup_reference(q)
                        })
                        .unwrap_or(false)
            })
            .unwrap_or(false);
        if comedy_mode_active {
            let insert_pos = messages
                .iter()
                .take_while(|m| m.role.as_str() == "system")
                .count();
            messages.insert(
                insert_pos,
                crate::inference::engine::Message::system(COMEDY_MODE_INSTRUCTION),
            );
            debug!(
                session = %self.session_id,
                "Comedy mode instruction injected into inference request"
            );
        }

        // Step 2d: comedy request canonicalization.
        //
        // "step-dad joke" is operator shorthand for an adult/NSFW dad-joke-style
        // pun, but local models over-anchor on the literal words "step dad" and
        // make the subject a stepdad/family premise even when told not to. For
        // inference only, rewrite the active user turn to the intended format
        // label. Conversation history remains untouched; this just removes the
        // misleading lexical anchor from the current prompt.
        if let Some(q) = query_hint {
            if let Some(canonical) = canonicalize_step_dad_joke_request_for_inference(q) {
                let target = messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == "user" && !is_tool_result_content(&m.content));
                if let Some(user_msg) = target {
                    user_msg.content = canonical;
                    debug!(
                        session = %self.session_id,
                        "Step-dad joke request canonicalized for inference"
                    );
                }
            }
        }

        // Step 2e (Round 3 / T0.5): clipboard + shell injection as user-turn prefix.
        //
        // Prior implementation injected these as labelled system messages:
        //   ["Clipboard: ...", "Shell: $ cmd → exit 0 in /dir"]
        // which caused qwen3 to echo them back verbatim on questions about the current
        // environment ("Your clipboard contains...", "You ran `ls` in /Users/jason/...").
        // The labels read to the model like prompt-template metadata worth narrating,
        // not like ambient operator context.
        //
        // Reframing: fold both into a single `[Env ...]` prefix on the LAST genuine user
        // message. The model now sees the context as part of what the operator "said"
        // this turn, not as separate system commands to describe. This dramatically
        // reduces label echo and matches how humans naturally reference recent state
        // ("I just ran X — why did it fail?" instead of "the shell log shows X").
        //
        // Skipped when the trailing user message is a tool-result injection
        // (is_tool_result_content) — those already carry their own context block.
        // Also skipped when no user message exists yet (e.g. pre-first-turn prefill).
        // Clipboard injection is CONDITIONAL — only injected when:
        // (a) the user's query explicitly references clipboard/copy, OR
        // (b) the clipboard was updated within CLIPBOARD_RECENCY_SECS (fresh copy-then-ask)
        //
        // Without this gate, stale clipboard content (e.g. 926 chars of browser cookies
        // from hours ago) is prepended to every query, overwhelming small models into
        // treating the clipboard as the answer to unrelated questions like "my messages".
        let clipboard_opt = {
            let raw_clip = self.context_observer.clipboard_summary();
            let query_lower = messages
                .iter()
                .rev()
                .find(|m| m.role == "user" && !is_tool_result_content(&m.content))
                .map(|m| m.content.to_lowercase())
                .unwrap_or_default();
            let user_references_clipboard = query_lower.contains("clipboard")
                || query_lower.contains("copied")
                || query_lower.contains("copy")
                || query_lower.contains("paste")
                || query_lower.contains("what i just")
                || query_lower.contains("what did i");
            let clipboard_is_fresh = self
                .context_observer
                .snapshot()
                .clipboard_changed_at
                .map(|t| {
                    (chrono::Utc::now() - t).num_seconds()
                        < crate::constants::CLIPBOARD_RECENCY_SECS
                })
                .unwrap_or(false);
            if user_references_clipboard || clipboard_is_fresh {
                raw_clip
            } else {
                if raw_clip.is_some() {
                    debug!(
                        session = %self.session_id,
                        "Clipboard available but stale and user didn't reference it — skipping injection"
                    );
                }
                None
            }
        };
        let shell_opt = self
            .context_observer
            .snapshot()
            .last_shell_command
            .as_ref()
            .filter(|shell| {
                let age_secs = (chrono::Utc::now() - shell.received_at).num_seconds();
                age_secs < crate::constants::SHELL_CONTEXT_MAX_AGE_SECS as i64
            })
            .map(|shell| {
                let exit_str = shell
                    .exit_code
                    .map_or_else(|| "?".to_string(), |c| c.to_string());
                format!(
                    "$ {} \u{2192} exit {} in {}",
                    shell.command, exit_str, shell.cwd
                )
            });

        if clipboard_opt.is_some() || shell_opt.is_some() {
            let target = messages
                .iter_mut()
                .rev()
                .find(|m| m.role == "user" && !is_tool_result_content(&m.content));
            if let Some(user_msg) = target {
                let mut prefix_parts: Vec<String> = Vec::with_capacity(2);
                if let Some(ref clip) = clipboard_opt {
                    prefix_parts.push(format!("[Env · clipboard: {clip}]"));
                }
                if let Some(ref sh) = shell_opt {
                    prefix_parts.push(format!("[Env · shell: {sh}]"));
                }
                let prefix = prefix_parts.join("\n");
                // Prepend with a blank line before the operator's actual turn so
                // the model sees a visible separation between ambient context and
                // the actual question.
                user_msg.content = format!("{prefix}\n\n{}", user_msg.content);

                debug!(
                    session       = %self.session_id,
                    clipboard_len = clipboard_opt.as_ref().map(|c| c.chars().count()).unwrap_or(0),
                    shell_present = shell_opt.is_some(),
                    "Env context folded into user-turn prefix"
                );
            } else {
                debug!(
                    session = %self.session_id,
                    "Env context available but no genuine user message to attach to — skipping"
                );
            }
        }

        // Phase 21 — Recall injection.
        // Phase 37.8 — cross-session leak fix.
        //
        // Entries are partitioned by whether they belong to the CURRENT session or
        // a prior one. The two groups are framed differently so the model does not
        // mistake prior-session content for its own recent turns in THIS conversation.
        //
        // Root cause of the leak (Test 2, ret2libc): stored content literally includes
        // the strings "User: …" and "Assistant: …". When a prior-session entry was
        // injected flat under the label "Memory: …", the model saw an "Assistant: …"
        // block, pattern-matched it as its own prior turn, and replied "I already
        // explained this to you" — a hallucinated continuity that never happened.
        //
        // Two defenses stack:
        //   1. Strong framing header — "Reference notes from prior sessions (not part
        //      of the current conversation; do not claim to have said any of this to
        //      the operator now)" — tells the model explicitly that this is retrieved
        //      reference material, not in-context history.
        //   2. Role-marker neutralization — the literal tokens "User:" / "Assistant:"
        //      are rewritten to "Q:" / "A:" inside prior-session entries. This blocks
        //      format-level bleed-through even if the framing header is ignored.
        //
        // Current-session entries are genuinely part of this conversation and are
        // injected under a neutral "Earlier in this conversation:" header without
        // role-marker rewriting.
        //
        // Placed after shell so ordering is: personality → context → clipboard → shell
        // → memory → history.
        if !recall.is_empty() {
            let (current_session, prior_session): (Vec<_>, Vec<_>) = recall
                .iter()
                .partition(|e| e.session_id.as_deref() == Some(self.session_id.as_str()));

            let insert_at = messages.iter().take_while(|m| m.role == "system").count();

            // Prior-session block first — read order is reference → recent, so the
            // model encounters the framing disclaimer before the in-session block.
            //
            // Phase 37.8.1 reinforcement: per-entry inline tags. The first iteration
            // of this fix used a single header before a list of joined entries; the
            // model still claimed continuity ("Since I've already walked you through
            // …") because the body of each entry reads like first-person assistant
            // text and the header was too weak to override. Wrapping each entry in
            // explicit "[from a DIFFERENT prior conversation, NOT this one]" /
            // "[end prior]" delimiters forces the model to step over a per-chunk
            // disclaimer before reading any retrieved text — structurally stronger
            // conditioning than a single block header.
            if !prior_session.is_empty() {
                let body: String = prior_session
                    .iter()
                    .map(|e| {
                        let neutralized = neutralize_role_markers(&e.content);
                        format!(
                            "[from a DIFFERENT prior conversation, NOT this one — never \
                             claim you said this to the operator now]\n{neutralized}\n[end prior]"
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let block = format!(
                    "Reference notes from prior sessions follow. These are retrieved \
                     records from past conversations with other operators (or this one \
                     on a different day). They are NOT part of the current conversation. \
                     Use them only as topical hints; do not claim to have said any of \
                     this to the operator now.\n\n{body}"
                );
                messages.insert(insert_at, crate::inference::engine::Message::system(block));
            }

            if !current_session.is_empty() {
                let body: String = current_session
                    .iter()
                    .map(|e| e.content.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ");
                // Insert AFTER the prior-session block (if any) so the order in the
                // final message list is [prior-session, current-session, …history].
                let pos = messages.iter().take_while(|m| m.role == "system").count();
                messages.insert(
                    pos,
                    crate::inference::engine::Message::system(format!(
                        "Earlier in this conversation: {body}"
                    )),
                );
            }
        }

        messages
    }

    /// Select a bridging phrase for uncertainty-sentinel interception.
    ///
    /// Uses the first byte of `trace_id` as a cheap pseudo-random selector —
    /// varied across requests, reproducible for a given trace, no RNG dependency.
    fn bridging_phrase(trace_id: &str) -> &'static str {
        let idx = trace_id.as_bytes().first().copied().unwrap_or(0) as usize;
        BRIDGING_PHRASES[idx % BRIDGING_PHRASES.len()]
    }

    /// Capture the main display and return the PNG as a base64-encoded string.
    ///
    /// Uses the macOS `screencapture` CLI:
    ///   - `-x`     suppress camera shutter sound
    ///   - `-m`     main display only (no secondary monitors — avoids ambiguity)
    ///   - `-t png` PNG format (lossless, consistent with Ollama's image expectation)
    ///
    /// The capture file is written to a per-invocation path derived from
    /// `SCREEN_CAPTURE_PATH_PREFIX` + a UUID suffix (e.g.
    /// `/tmp/dexter_screen_3f2a1b....png`). A fixed path would introduce a
    /// race condition if two vision queries ran concurrently — the second write
    /// would clobber the first before it was read and encoded. The UUID suffix
    /// makes each invocation fully independent.
    ///
    /// The file is read, base64-encoded, and deleted within this call.
    ///
    /// Returns `None` on any failure (process spawn error, non-zero exit, read
    /// error, or timeout). Callers treat `None` as "proceed without an image".
    async fn capture_screen(&self) -> Option<String> {
        use base64::Engine as _;

        // Per-invocation path: SCREEN_CAPTURE_PATH_PREFIX + UUID + ".png".
        // Each call writes to a unique file — concurrent calls never collide.
        let path = format!(
            "{}_{}.png",
            SCREEN_CAPTURE_PATH_PREFIX,
            uuid::Uuid::new_v4().as_simple(),
        );

        // Spawn screencapture with a SCREEN_CAPTURE_TIMEOUT_SECS wall-clock limit.
        // On Apple Silicon the capture typically completes in <1s; the timeout is
        // a safety net for edge cases (display sleep, Quartz compositor stall, CI).
        let spawn_result = tokio::time::timeout(
            std::time::Duration::from_secs(SCREEN_CAPTURE_TIMEOUT_SECS),
            tokio::process::Command::new("screencapture")
                .args(["-x", "-m", "-t", "png", &path])
                .status(),
        )
        .await;

        match spawn_result {
            Ok(Ok(status)) if status.success() => {
                match tokio::fs::read(&path).await {
                    Ok(bytes) => {
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        // Delete the temp file immediately after encoding.
                        // Non-fatal if removal fails (stale /tmp entries are harmless).
                        let _ = tokio::fs::remove_file(&path).await;
                        Some(b64)
                    }
                    Err(e) => {
                        warn!(
                            session = %self.session_id,
                            error   = %e,
                            path    = %path,
                            "Vision: failed to read screencapture output"
                        );
                        None
                    }
                }
            }
            Ok(Ok(_non_zero)) => {
                warn!(session = %self.session_id, path = %path, "Vision: screencapture exited non-zero");
                None
            }
            Ok(Err(e)) => {
                warn!(
                    session = %self.session_id,
                    error   = %e,
                    "Vision: screencapture process spawn failed"
                );
                None
            }
            Err(_timeout) => {
                warn!(
                    session      = %self.session_id,
                    timeout_secs = SCREEN_CAPTURE_TIMEOUT_SECS,
                    "Vision: screencapture timed out"
                );
                None
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Send an entity state change to Swift and track the current state.
    ///
    /// Phase 27: changed from `&self` to `&mut self` to update `current_state`.
    /// `handle_barge_in` uses `current_state` to decide whether to transition
    /// back to LISTENING (only if actively SPEAKING or THINKING).
    async fn send_state(
        &mut self,
        state: EntityState,
        trace_id: &str,
    ) -> Result<(), OrchestratorError> {
        self.current_state = state; // Phase 27: track for handle_barge_in
        let event = ServerEvent {
            trace_id: trace_id.to_string(),
            event: Some(server_event::Event::EntityState(EntityStateChange {
                state: state.into(),
            })),
        };
        self.tx
            .send(Ok(event))
            .await
            .map_err(|_| OrchestratorError::ChannelClosed)
    }

    /// Send a text response chunk to Swift.
    ///
    /// `is_final = false` during streaming; `is_final = true` on the last chunk
    /// (which may have empty `content`). Swift uses `is_final` to exit streaming mode.
    /// Send a TextResponse to the Swift UI without a caller-supplied trace_id.
    ///
    /// Used for autonomous notifications (worker degradation) where no inbound
    /// request trace_id is in scope. Swallows send errors — a closed channel
    /// during shutdown is not a bug.
    async fn send_text_response_to_ui(&self, text: &str, is_final: bool) {
        let event = ServerEvent {
            trace_id: uuid::Uuid::new_v4().to_string(),
            event: Some(server_event::Event::TextResponse(TextResponse {
                content: text.to_string(),
                is_final,
            })),
        };
        if let Err(e) = self.tx.send(Ok(event)).await {
            warn!(error = %e, "Failed to send degradation notification to UI — channel may be closing");
        }
    }

    async fn send_text(
        &self,
        content: &str,
        is_final: bool,
        trace_id: &str,
    ) -> Result<(), OrchestratorError> {
        let event = ServerEvent {
            trace_id: trace_id.to_string(),
            event: Some(server_event::Event::TextResponse(TextResponse {
                content: content.to_string(),
                is_final,
            })),
        };
        self.tx
            .send(Ok(event))
            .await
            .map_err(|_| OrchestratorError::ChannelClosed)
    }

    // ── Phase 24c: VadHint adaptive endpoint detection ────────────────────────

    /// Classify the last sentence of a response to determine optimal VAD silence frames.
    ///
    /// Returns `Some(frames)` when the sentence pattern suggests a short reply
    /// (yes/no question). Returns `None` for open questions and declaratives —
    /// the VAD default (20 frames / 640ms) is appropriate.
    ///
    /// Only the *last* sentence matters — "Let me explain. Should I continue?"
    /// — only the final question determines expected reply length.
    fn classify_expected_response(last_sentence: &str) -> Option<u32> {
        let sentence = last_sentence.trim().to_lowercase();

        // Must end with a question mark to be a candidate.
        if !sentence.ends_with('?') {
            return None;
        }

        // Wh-word questions ("what", "how", "why", "when", "where", "which") are
        // open-ended even when they contain yes/no fragments.
        // e.g. "What would you like to work on?" contains "would you like" but
        //      the operator's reply will be longer than "yes" or "no".
        let wh_words = ["what ", "how ", "why ", "when ", "where ", "which "];
        if wh_words.iter().any(|w| sentence.starts_with(w)) {
            return None;
        }

        // Patterns that strongly predict a yes/no or very-short reply.
        let yes_no_patterns = [
            "should i",
            "shall i",
            "do you want",
            "would you like",
            "is that ok",
            "is that correct",
            "right?",
            "yes or no",
            "want me to",
            "go ahead",
            "is that right",
            "does that",
            "can i",
            "may i",
        ];

        if yes_no_patterns.iter().any(|p| sentence.contains(p)) {
            // 8 frames × ~32ms/frame ≈ 256ms — enough for "yes", "no", "sure"
            Some(8)
        } else {
            None // Open question or declarative — keep default 640ms
        }
    }

    // ── Phase 17: Proactive observations ──────────────────────────────────────

    /// Collect a generation without streaming tokens to the UI.
    ///
    /// Unlike `generate_and_stream()`, this method accumulates all tokens into a
    /// String and returns the complete response. No `TextResponse` events are sent
    /// during generation — the caller inspects the full text before deciding whether
    /// to display it.
    ///
    /// Used exclusively for proactive observations where we must check for the
    /// `[SILENT]` opt-out before any output reaches the UI or TTS.
    ///
    /// ## Return semantics
    ///
    /// - `Ok(Some(text))` — inference succeeded; `text` may be `[SILENT]` or empty
    ///   (caller checks `is_silent_response()`).
    /// - `Ok(None)` — inference failed (Ollama unreachable, model missing, or the
    ///   30-second collection timeout elapsed). Signals the caller to keep the
    ///   rate-limit slot burned — prevents rapid re-fire loops on connectivity issues.
    ///
    /// These two outcomes carry different rate-limit implications: `None` burns the
    /// slot, `Some("[SILENT]")` does not (the caller calls `undo_fire()`).
    async fn collect_generation(
        &mut self,
        model_name: &str,
        messages: Vec<crate::inference::engine::Message>,
        trace_id: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let req = GenerationRequest {
            model_name: model_name.to_string(),
            messages,
            temperature: None,
            unload_after: false, // proactive uses FAST — never unloaded after use
            keep_alive_override: None,
            num_predict: None,
            inactivity_timeout_override_secs: None,
            num_ctx_override: None,
        };

        match self.engine.generate_stream(req).await {
            Err(e) => {
                // Inference failed before streaming started (Ollama unreachable, model missing).
                // Return None so the caller keeps the rate-limit slot burned.
                warn!(
                    session  = %self.session_id,
                    trace_id = %trace_id,
                    error    = %e,
                    "Proactive collect_generation failed before streaming — slot burned"
                );
                Ok(None)
            }
            Ok(mut rx) => {
                let mut full_response = String::new();

                // Accumulate tokens with a 30-second hard timeout.
                //
                // qwen3:8b (FAST model) should complete a single-sentence observation
                // in well under 10 seconds on the operator's hardware. 30 seconds is
                // a generous upper bound that allows for a loaded system while still
                // preventing a hung Ollama from blocking the proactive task forever.
                //
                // On timeout: treat as an inference error (return None) so the
                // rate-limit slot stays burned — same logic as Ollama unreachable.
                let accumulate = async {
                    while let Some(chunk_result) = rx.recv().await {
                        match chunk_result {
                            Ok(chunk) if !chunk.done => full_response.push_str(&chunk.content),
                            Ok(_done) => break,
                            Err(e) => {
                                warn!(
                                    session  = %self.session_id,
                                    trace_id = %trace_id,
                                    error    = %e,
                                    "Proactive collect_generation chunk error — using partial response"
                                );
                                break;
                            }
                        }
                    }
                };

                match tokio::time::timeout(std::time::Duration::from_secs(30), accumulate).await {
                    Ok(()) => Ok(Some(full_response)),
                    Err(_elapsed) => {
                        warn!(
                            session  = %self.session_id,
                            trace_id = %trace_id,
                            "Proactive collect_generation timed out after 30s — slot burned"
                        );
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Generate and deliver a proactive ambient observation.
    ///
    /// Pipeline:
    ///   1. Transition entity to THINKING so the operator sees Dexter is active.
    ///   2. Build messages: [personality][context][proactive_user_prompt].
    ///   3. Collect full response via `collect_generation()` (no streaming to UI).
    ///   4. Check for `[SILENT]` opt-out — if silent, restore IDLE and return.
    ///   5. Send the full response as a single is_final=true TextResponse to the UI.
    ///   6. Deliver via TTS if available (synchronous — await synthesis before IDLE).
    ///   7. Transition entity to IDLE.
    ///
    /// Proactive responses are NOT added to conversation history or session state.
    /// They are ephemeral ambient observations, not part of the operator's dialogue.
    async fn do_proactive_response(
        &mut self,
        summary: &str,
        trace_id: &str,
    ) -> Result<(), OrchestratorError> {
        // 1. THINKING state — operator sees Dexter is about to speak.
        self.send_state(EntityState::Thinking, trace_id).await?;

        // 2. Build messages: personality wraps a minimal proactive prompt.
        //    No conversation history is included — the observation is context-driven,
        //    not dialogue-driven.
        let proactive_user = crate::inference::engine::Message::user(
            crate::proactive::ProactiveEngine::build_proactive_prompt(summary),
        );
        let mut messages = self.personality.apply_to_messages(&[proactive_user]);
        // Insert context snapshot at index 1 (after personality at 0).
        messages.insert(
            1,
            crate::inference::engine::Message::system(format!("Context: {summary}")),
        );
        // Insert wall-clock timestamp so the model doesn't hallucinate the date.
        // qwen3 has no real-time clock — without this injection it invents dates from
        // training-data patterns (e.g. "The current time is Wednesday 2025-04-16…").
        // Same format as prepare_messages_for_inference step 2a.
        {
            let ts = chrono::Local::now()
                .format("%a %b %-d %Y %-I:%M %p %Z")
                .to_string();
            messages.insert(
                1,
                crate::inference::engine::Message::system(format!("The current time is {ts}.")),
            );
        }
        // Final layout: [0] personality  [1] datetime  [2] context  [3] proactive user prompt

        // 3. Collect the full response before displaying — enables [SILENT] check.
        let model_name = self.model_config.fast.clone();
        let response_opt = self
            .collect_generation(&model_name, messages, trace_id)
            .await?;

        // 4. Check for inference failure or [SILENT] opt-out.
        //
        // `collect_generation` returns:
        //   None         — Ollama failed or timed out. Slot already burned by record_fire().
        //                  Restore IDLE and return — no output, no refund.
        //   Some(text)   — Inference succeeded. Check for [SILENT] or empty.
        //
        // The [SILENT] / empty case calls `undo_fire()` to refund the rate-limit slot.
        // Rationale: the model consciously decided it had nothing useful to say.
        // From the operator's perspective nothing happened, so the 90-second budget
        // should not be consumed. Inference errors do NOT get this refund — keeping
        // the slot burned prevents rapid re-fire loops when Ollama is unavailable.
        let response = match response_opt {
            None => {
                // Inference error or timeout — slot stays burned.
                warn!(
                    session  = %self.session_id,
                    trace_id = %trace_id,
                    "Proactive observation aborted (inference failure) — rate-limit slot burned"
                );
                self.send_state(EntityState::Idle, trace_id).await?;
                return Ok(());
            }
            Some(ref text)
                if crate::proactive::ProactiveEngine::should_suppress_proactive(text) =>
            {
                // Either the model chose silence OR it returned low-value filler
                // (bare time/date/day). Refund the slot so the next context
                // change gets a fresh shot at a real observation.
                //
                // Phase 37.9: the low-value filter catches cases where the model
                // ignores the prompt's FORBIDDEN list and emits "It's 3:42 PM"
                // anyway. See `ProactiveEngine::is_low_value_response`.
                let demoted = !crate::proactive::ProactiveEngine::is_silent_response(text);
                self.proactive_engine.undo_fire();
                info!(
                    session  = %self.session_id,
                    trace_id = %trace_id,
                    demoted  = demoted,
                    sample   = %text.chars().take(80).collect::<String>(),
                    "Proactive observation suppressed — rate-limit slot refunded"
                );
                self.send_state(EntityState::Idle, trace_id).await?;
                return Ok(());
            }
            Some(text) => text,
        };

        info!(
            session  = %self.session_id,
            trace_id = %trace_id,
            context  = %summary,
            "Proactive observation firing"
        );

        // 5. Send the full text as a single is_final=true response.
        //    The operator sees the complete observation in one display update.
        self.send_text(response.trim(), true, trace_id).await?;

        // 6. TTS delivery (if available).
        //    Proactive responses use the same TTS path as regular responses,
        //    but the entire text is sent as one sentence (no SentenceSplitter).
        //
        //    Phase 18 IDLE timing fix:
        //    Instead of sending EntityState::Idle here (after TTS synthesis), we send
        //    an is_final sentinel after all PCM chunks. Swift's AudioPlayer arms a
        //    playback-complete callback on receipt. When the last buffer finishes
        //    playing, Swift sends AUDIO_PLAYBACK_COMPLETE → orchestrator → EntityState::Idle.
        //    This ensures the entity stays THINKING throughout playback, not just synthesis.
        if self.voice.is_tts_available() {
            let tts_arc = self.voice.tts_arc();
            let text_bytes = response.trim().as_bytes().to_vec();
            let session_tx = self.tx.clone();
            let trace_id_clone = trace_id.to_string();

            let handle = tokio::spawn(async move {
                use crate::ipc::proto::{server_event, AudioResponse};
                let mut guard = tts_arc.lock().await;
                if let Some(client) = guard.as_mut() {
                    if client
                        .write_frame(msg::TEXT_INPUT, &text_bytes)
                        .await
                        .is_ok()
                    {
                        let mut seq = 0u32;
                        loop {
                            // Phase 38 / Codex [14]: bound per-frame read.
                            let frame_result = match tokio::time::timeout(
                                std::time::Duration::from_secs(
                                    crate::constants::TTS_FRAME_READ_TIMEOUT_SECS,
                                ),
                                client.read_frame(),
                            )
                            .await
                            {
                                Err(_elapsed) => {
                                    warn!(
                                        "TTS read_frame timed out after {}s — kokoro stalled, breaking out",
                                        crate::constants::TTS_FRAME_READ_TIMEOUT_SECS,
                                    );
                                    break;
                                }
                                Ok(r) => r,
                            };
                            match frame_result {
                                Ok(Some((msg::TTS_AUDIO, pcm))) => {
                                    let evt = ServerEvent {
                                        trace_id: String::new(),
                                        event: Some(server_event::Event::AudioResponse(
                                            AudioResponse {
                                                data: pcm,
                                                sequence_number: seq,
                                                is_final: false,
                                            },
                                        )),
                                    };
                                    let _ = session_tx.send(Ok(evt)).await;
                                    seq += 1;
                                }
                                Ok(Some((msg::TTS_DONE, _))) => {
                                    // Phase 18: send the is_final sentinel (empty data).
                                    // Swift arms its playback-complete callback on receipt.
                                    // When the last scheduled buffer finishes playing,
                                    // Swift sends AUDIO_PLAYBACK_COMPLETE, and the
                                    // orchestrator transitions to IDLE then.
                                    let sentinel = ServerEvent {
                                        trace_id: trace_id_clone,
                                        event: Some(server_event::Event::AudioResponse(
                                            AudioResponse {
                                                data: vec![],
                                                sequence_number: seq,
                                                is_final: true,
                                            },
                                        )),
                                    };
                                    let _ = session_tx.send(Ok(sentinel)).await;
                                    break;
                                }
                                Ok(Some(_)) => {} // discard unexpected frames
                                _ => break,
                            }
                        }
                    }
                }
            });
            let _ = handle.await;
            // Do NOT send EntityState::Idle here — Swift sends AUDIO_PLAYBACK_COMPLETE
            // after the last buffer finishes playing, which the orchestrator handles
            // in the AudioPlaybackComplete arm of handle_system_event.
        } else {
            // No TTS — no audio will play. Transition to IDLE directly since there is
            // no playback to wait for.
            self.send_state(EntityState::Idle, trace_id).await?;
        }

        Ok(())
    }

    /// Emit an `ActionRequest` ServerEvent to Swift, which will present a confirmation dialog.
    async fn send_action_request(
        &self,
        req: ActionRequest,
        trace_id: &str,
    ) -> Result<(), OrchestratorError> {
        let event = ServerEvent {
            trace_id: trace_id.to_string(),
            event: Some(server_event::Event::ActionRequest(req)),
        };
        self.tx
            .send(Ok(event))
            .await
            .map_err(|_| OrchestratorError::ChannelClosed)
    }

    /// Speak a short action-result message: send text to the Swift UI and synthesize via TTS.
    ///
    /// Used after action execution (`Completed` / `Rejected`) to give the operator
    /// audible feedback when the main TTS pipeline has already completed. Without this,
    /// a failed or completed browser/tool action produces total silence — the operator
    /// has no idea what happened.
    ///
    /// Returns `true` when TTS audio was dispatched and an `AUDIO_PLAYBACK_COMPLETE`
    /// round-trip from Swift will drive the IDLE transition.
    /// Returns `false` when TTS is unavailable; the caller must send IDLE directly.
    async fn speak_action_feedback(
        &mut self,
        text: &str,
        trace_id: &str,
    ) -> Result<bool, OrchestratorError> {
        // Always show the feedback text in the Swift UI so it's visible even
        // when the operator has headphones off or TTS is unavailable.
        self.send_text(text, true, trace_id).await?;

        if !self.voice.is_tts_available() {
            return Ok(false);
        }

        let tts_arc = self.voice.tts_arc();
        let text_bytes = text.as_bytes().to_vec();
        let session_tx = self.tx.clone();
        let trace_id_clone = trace_id.to_string();

        // Identical pattern to do_proactive_response §6 (TTS delivery):
        // spawn a task to drive TEXT_INPUT → TTS_AUDIO frames → TTS_DONE → is_final sentinel.
        let handle = tokio::spawn(async move {
            use crate::ipc::proto::{server_event, AudioResponse};
            let mut guard = tts_arc.lock().await;
            if let Some(client) = guard.as_mut() {
                if client
                    .write_frame(msg::TEXT_INPUT, &text_bytes)
                    .await
                    .is_ok()
                {
                    let mut seq = 0u32;
                    loop {
                        // Phase 38 / Codex [14]: bound per-frame read.
                        let frame_result = match tokio::time::timeout(
                            std::time::Duration::from_secs(
                                crate::constants::TTS_FRAME_READ_TIMEOUT_SECS,
                            ),
                            client.read_frame(),
                        )
                        .await
                        {
                            Err(_elapsed) => {
                                warn!(
                                    "TTS read_frame timed out after {}s — kokoro stalled, breaking out",
                                    crate::constants::TTS_FRAME_READ_TIMEOUT_SECS,
                                );
                                break;
                            }
                            Ok(r) => r,
                        };
                        match frame_result {
                            Ok(Some((msg::TTS_AUDIO, pcm))) => {
                                let evt = ServerEvent {
                                    trace_id: String::new(),
                                    event: Some(server_event::Event::AudioResponse(
                                        AudioResponse {
                                            data: pcm,
                                            sequence_number: seq,
                                            is_final: false,
                                        },
                                    )),
                                };
                                let _ = session_tx.send(Ok(evt)).await;
                                seq += 1;
                            }
                            Ok(Some((msg::TTS_DONE, _))) => {
                                // is_final sentinel: Swift arms its playback-complete callback.
                                // When the last scheduled buffer finishes playing, Swift sends
                                // AUDIO_PLAYBACK_COMPLETE, and the orchestrator transitions to IDLE.
                                let sentinel = ServerEvent {
                                    trace_id: trace_id_clone,
                                    event: Some(server_event::Event::AudioResponse(
                                        AudioResponse {
                                            data: vec![],
                                            sequence_number: seq,
                                            is_final: true,
                                        },
                                    )),
                                };
                                let _ = session_tx.send(Ok(sentinel)).await;
                                break;
                            }
                            Ok(Some(_)) => {} // discard unexpected frames
                            _ => break,
                        }
                    }
                }
            }
        });
        let _ = handle.await;
        Ok(true)
    }
}

// ── Phase 27: background generation helpers ───────────────────────────────────

/// Send a `TextResponse` event from a background task (no `&self` reference).
///
/// Returns `true` if the send succeeded; `false` if the channel is closed
/// (the session ended while generation was running — the task should stop).
async fn send_text_bg(
    tx: &mpsc::Sender<Result<ServerEvent, Status>>,
    content: &str,
    is_final: bool,
    trace_id: &str,
) -> bool {
    use crate::ipc::proto::{server_event, TextResponse};
    let event = ServerEvent {
        trace_id: trace_id.to_string(),
        event: Some(server_event::Event::TextResponse(TextResponse {
            content: content.to_string(),
            is_final,
        })),
    };
    tx.send(Ok(event)).await.is_ok()
}

/// Send a `VadHint` event from a background task (no `&self` reference).
async fn send_vad_hint_bg(
    tx: &mpsc::Sender<Result<ServerEvent, Status>>,
    silence_frames: u32,
    trace_id: &str,
) -> bool {
    use crate::ipc::proto::{server_event, VadHint};
    let event = ServerEvent {
        trace_id: trace_id.to_string(),
        event: Some(server_event::Event::VadHint(VadHint { silence_frames })),
    };
    tx.send(Ok(event)).await.is_ok()
}

/// Primary generation with uncertainty sentinel interception — runs as a background task.
///
/// Phase 27: extracted from `CoreOrchestrator::generate_primary` so it can be spawned
/// independently via `tokio::spawn`, freeing the session reader task to process BargIn
/// and other ClientEvents while the (potentially 30–120 s) token loop runs.
///
/// Cooperative cancellation: checks `cancel_token` once per token. When set, the loop
/// breaks, is_final is sent to clean up Swift's UI state, and `GenerationResult::cancelled`
/// is set to `true` so `handle_generation_complete` skips all post-processing.
///
/// Results are delivered via `gen_tx` to `gen_rx` in the server.rs `select!` loop.
#[allow(clippy::too_many_arguments)]
async fn run_generation_background(
    engine: crate::inference::engine::InferenceEngine,
    tx: mpsc::Sender<Result<ServerEvent, Status>>,
    session_id: String,
    model_name: String,
    messages: Vec<crate::inference::engine::Message>,
    trace_id: String,
    unload_after: bool,
    // Phase 37.7: orthogonal to unload_after. unload_after governs keep_alive
    // (evict after this request); needs_context_cap governs num_ctx (cap KV
    // cache on load). Heavy wants both; Code wants cap but stays warm; Primary
    // wants neither. Previously conflated via `unload_after`, which left CODE
    // queries uncapped → 128k-token KV cache → CPU spill → stuck-think timeout.
    needs_context_cap: bool,
    tts_tx_opt: Option<UnboundedSender<String>>,
    tts_join_handle: Option<JoinHandle<()>>,
    cancel_token: Arc<AtomicBool>,
    gen_tx: mpsc::Sender<GenerationResult>,
    content: String,
    embed_model: String,
    agentic_depth: u8,
    // Phase 38 / Codex finding [7]: published into by this task after a
    // successful `generate_stream_cancellable` call so the orchestrator's
    // cancel paths can abort the producer task directly. Cleared (set to
    // None) when the loop ends so a subsequent cancel doesn't try to abort
    // a long-finished task.
    producer_abort_slot: Arc<std::sync::Mutex<Option<tokio::task::AbortHandle>>>,
) {
    use crate::inference::interceptor::{InterceptorOutput, UncertaintyInterceptor};
    use crate::ipc::proto::{server_event, AudioResponse};
    use crate::voice::sentence::SentenceSplitter;

    let req = GenerationRequest {
        model_name: model_name,
        messages,
        temperature: None,
        unload_after,
        // Pin non-unload models (FAST + PRIMARY) at 999m so real inference
        // requests don't reset the TTL to Ollama's default 5m, which would
        // evict the model after inactivity and cause a 15–20s cold-load on
        // the next query. HEAVY model (unload_after=true) is handled separately.
        keep_alive_override: if unload_after {
            None
        } else {
            Some(FAST_MODEL_KEEP_ALIVE)
        },
        num_predict: None,
        // Phase 37.6 / Cluster-E, extended Phase 37.7: cold-load grace window.
        // Large on-demand models (Heavy deepseek-r1:32b, Code deepseek-coder-v2:16b)
        // can spend 30–60s loading off the USB-SSD with zero bytes on the HTTP
        // stream. The default 30s inactivity timeout would abort the request
        // before the first token ever arrives. 120s covers realistic worst
        // cases for both. Keyed on `needs_context_cap` rather than `unload_after`
        // because CODE stays warm after first load but still pays the initial
        // cold-load tax. GENERATION_WALL_TIMEOUT_SECS (90s, stuck-think) and
        // GENERATION_HARD_TIMEOUT_SECS (180s) still bound the whole request.
        inactivity_timeout_override_secs: if needs_context_cap { Some(120) } else { None },
        // Phase 37.6 / Cluster-E (B15), extended Phase 37.7: cap KV-cache
        // context window for tiers with huge native contexts. deepseek-r1:32b
        // (131k native) and deepseek-coder-v2:16b (163k native) would each
        // allocate 20–32 GiB KV cache without this cap, CPU-spilling and
        // making first-token latency exceed our 90s wall timeout. 8k is
        // plenty for Dexter's single-turn reasoning/code workloads. FAST
        // and PRIMARY keep their model-trained defaults.
        num_ctx_override: if needs_context_cap {
            Some(LARGE_MODEL_NUM_CTX)
        } else {
            None
        },
    };

    let mut full_response = String::new();
    let mut interceptor = UncertaintyInterceptor::new();
    let mut intercepted_q: Option<String> = None;
    let mut splitter = SentenceSplitter::new();
    let mut last_sentence: Option<String> = None;
    let mut cancelled = false;
    // T1.4: per-generation telemetry accumulators.
    let telemetry_model = req.model_name.clone();
    let mut token_count: u32 = 0;
    let mut first_token_at: Option<std::time::Instant> = None;
    // Wall-clock deadlines (T1.3): two distinct guards.
    //   - GENERATION_WALL_TIMEOUT_SECS: fires when full_response is still empty (stuck
    //     in silent think mode — per-chunk inactivity timer never trips because chunks
    //     are arriving, just without visible content).
    //   - GENERATION_HARD_TIMEOUT_SECS: absolute ceiling regardless of token flow —
    //     bounds runaway/looping outputs and pathological agentic turns.
    // The hard ceiling is enforced via `tokio::time::timeout(rx.recv())` so it fires
    // even when no new chunk ever arrives (slow stream, stalled channel).
    let gen_started = std::time::Instant::now();

    // Phase 38 / Codex finding [7]: use the cancellable variant so the
    // producer's abort_handle gets published to the orchestrator's slot for
    // immediate cancellation on barge-in. Pre-Phase-38 used the detached
    // `generate_stream` — aborting the consumer eventually closed the channel
    // but the producer could be parked at `byte_stream.next().await` for up to
    // `inactivity` seconds.
    match engine.generate_stream_cancellable(req).await {
        Err(e) => {
            error!(
                session  = %session_id,
                trace_id = %trace_id,
                error    = %e,
                "generate_stream failed before streaming started"
            );
            let _ = send_text_bg(
                &tx,
                "(generation timed out — check core logs)",
                true,
                &trace_id,
            )
            .await;
        }
        Ok(stream) => {
            // Publish the producer's abort handle so the orchestrator's cancel
            // paths can drop the upstream HTTP stream. `lock()` may fail if
            // poisoned; that's diagnostic only — generation can still proceed
            // without abort capability (the rx-drop fallback still works).
            if let Ok(mut g) = producer_abort_slot.lock() {
                *g = Some(stream.producer.abort_handle());
            }
            let mut rx = stream.rx;
            let hard_deadline = std::time::Duration::from_secs(GENERATION_HARD_TIMEOUT_SECS);
            'token_loop: loop {
                // T1.3: wall-clock enforcement independent of chunk arrival.
                //
                // Wrap `rx.recv()` in a timeout keyed to the remaining hard-deadline
                // budget. Without this, a slow/paused Ollama stream that never closes
                // the channel would keep us parked inside `rx.recv().await` — the
                // per-chunk deadline check only fires when a chunk actually lands.
                let elapsed = gen_started.elapsed();
                let remaining = hard_deadline.checked_sub(elapsed);
                let Some(remaining) = remaining else {
                    // Hard ceiling already exceeded (edge case: we got here without a recv).
                    warn!(
                        session      = %session_id,
                        trace_id     = %trace_id,
                        elapsed      = elapsed.as_secs(),
                        response_len = full_response.len(),
                        "Generation hard timeout — exceeded absolute ceiling before next chunk"
                    );
                    cancelled = true;
                    if let Some(ref tts_tx) = tts_tx_opt {
                        if let Some(remainder) = splitter.flush() {
                            let _ = tts_tx.send(remainder);
                        }
                    }
                    let _ = send_text_bg(&tx, "", true, &trace_id).await;
                    break 'token_loop;
                };

                let recv_result = tokio::time::timeout(remaining, rx.recv()).await;
                let chunk_result = match recv_result {
                    Err(_elapsed_err) => {
                        warn!(
                            session      = %session_id,
                            trace_id     = %trace_id,
                            elapsed      = gen_started.elapsed().as_secs(),
                            response_len = full_response.len(),
                            "Generation hard timeout — channel stalled past absolute ceiling"
                        );
                        cancelled = true;
                        if let Some(ref tts_tx) = tts_tx_opt {
                            if let Some(remainder) = splitter.flush() {
                                let _ = tts_tx.send(remainder);
                            }
                        }
                        let _ = send_text_bg(&tx, "", true, &trace_id).await;
                        break 'token_loop;
                    }
                    Ok(None) => break 'token_loop, // channel closed by engine
                    Ok(Some(cr)) => cr,
                };

                // Phase 27: cooperative cancellation check per token.
                // Relaxed is sufficient — we only need eventual visibility, not ordering.
                //
                // Two orthogonal deadline guards (T1.3):
                //   1. `stuck_timeout`: GENERATION_WALL_TIMEOUT_SECS have elapsed with
                //      zero visible output — model is stuck in silent think mode.
                //   2. `hard_timeout`: GENERATION_HARD_TIMEOUT_SECS absolute ceiling —
                //      redundant with the tokio::time::timeout above, but covers the
                //      case where a chunk landed just before the deadline.
                let elapsed_secs = gen_started.elapsed().as_secs();
                let stuck_timeout =
                    full_response.is_empty() && elapsed_secs > GENERATION_WALL_TIMEOUT_SECS;
                let hard_timeout = elapsed_secs > GENERATION_HARD_TIMEOUT_SECS;
                if cancel_token.load(Ordering::Relaxed) || stuck_timeout || hard_timeout {
                    if stuck_timeout {
                        warn!(
                            session  = %session_id,
                            trace_id = %trace_id,
                            elapsed  = elapsed_secs,
                            "Generation stuck-think timeout — model produced no visible output; cancelling"
                        );
                    } else if hard_timeout {
                        warn!(
                            session      = %session_id,
                            trace_id     = %trace_id,
                            elapsed      = elapsed_secs,
                            response_len = full_response.len(),
                            "Generation hard timeout — exceeded absolute ceiling; cancelling runaway output"
                        );
                    }
                    cancelled = true;
                    // Flush any TTS remainder so the TTS task can exit cleanly.
                    if let Some(ref tts_tx) = tts_tx_opt {
                        if let Some(remainder) = splitter.flush() {
                            let _ = tts_tx.send(remainder);
                        }
                    }
                    // Send is_final text so Swift's HUD exits streaming mode.
                    let _ = send_text_bg(&tx, "", true, &trace_id).await;
                    break 'token_loop;
                }

                match chunk_result {
                    Ok(chunk) if !chunk.done => {
                        // T1.4: count every streamed chunk as a token proxy.
                        // Ollama emits one chunk per token for our FAST/PRIMARY models.
                        token_count = token_count.saturating_add(1);
                        // Bypass the uncertainty interceptor while inside an open action block.
                        //
                        // qwen3 (and other models) sometimes emit [UNCERTAIN: <query>] inside
                        // action block content — e.g., in a "rationale" JSON field or between
                        // the closing `}` and `</dexter:action>`. If the interceptor fires
                        // mid-block it breaks the token loop before `</dexter:action>` is
                        // accumulated, producing the "ACTION_BLOCK_OPEN without matching close
                        // delimiter" warning and silently dropping the action.
                        //
                        // Detection: `<dexter:action>` in full_response but `</dexter:action>`
                        // not yet seen. Neither delimiter contains `[` so the interceptor
                        // never holds them in its buffer — full_response.contains() is reliable.
                        let inside_action_block = full_response.contains(ACTION_BLOCK_OPEN)
                            && !full_response.contains(ACTION_BLOCK_CLOSE);

                        let interceptor_out = if inside_action_block {
                            InterceptorOutput::Passthrough(chunk.content.clone())
                        } else {
                            interceptor.process(&chunk.content)
                        };

                        match interceptor_out {
                            InterceptorOutput::Passthrough(text) => {
                                if !text.is_empty() {
                                    // T1.4: stamp first visible operator-facing text.
                                    if first_token_at.is_none() {
                                        first_token_at = Some(std::time::Instant::now());
                                    }
                                    full_response.push_str(&text);
                                    // Channel closed → session ended, stop silently.
                                    if !send_text_bg(&tx, &text, false, &trace_id).await {
                                        cancelled = true;
                                        break 'token_loop;
                                    }
                                    if let Some(ref tts_tx) = tts_tx_opt {
                                        for sentence in splitter.push(&text) {
                                            last_sentence = Some(sentence.clone());
                                            let _ = tts_tx.send(sentence);
                                        }
                                    }
                                }
                            }
                            InterceptorOutput::Intercepted { flush, query } => {
                                if let Some(pre) = flush {
                                    if !pre.is_empty() {
                                        if first_token_at.is_none() {
                                            first_token_at = Some(std::time::Instant::now());
                                        }
                                        full_response.push_str(&pre);
                                        if !send_text_bg(&tx, &pre, false, &trace_id).await {
                                            cancelled = true;
                                            break 'token_loop;
                                        }
                                        if let Some(ref tts_tx) = tts_tx_opt {
                                            for sentence in splitter.push(&pre) {
                                                let _ = tts_tx.send(sentence);
                                            }
                                        }
                                    }
                                }
                                intercepted_q = Some(query.clone());
                                warn!(
                                    session  = %session_id,
                                    trace_id = %trace_id,
                                    query    = %query,
                                    full_response_tail = %&full_response[full_response.len().saturating_sub(200)..],
                                    "UncertaintyInterceptor fired — breaking token loop; full_response tail shown"
                                );
                                // Break WITHOUT is_final — handle_generation_complete's
                                // uncertainty re-prompt will send it.
                                break 'token_loop;
                            }
                        }
                    }
                    Ok(done_chunk) => {
                        // Phase 37.9 diagnostic: surface unexpected cold-loads.
                        //
                        // Ollama reports `load_duration` on the final chunk. On a model
                        // Ollama has kept warm per `keep_alive`, load time is ≤ 500 ms
                        // (the TTL lookup itself). A value > 5 s means the mmap'd weight
                        // pages were reclaimed by macOS between requests — the exact
                        // failure mode the PRIMARY keepalive ping task is designed to
                        // prevent. Emitting a warn here (in addition to the engine's
                        // info-level timing line) makes regressions obvious in the log
                        // rather than visible only as operator-perceived latency.
                        //
                        // False positive for first-use after startup warm: warmup runs
                        // through `warm_up_primary_model`, which calls generate_stream
                        // directly and does NOT pass through this path. So this warn
                        // only fires on real inference requests — meaning any hit is a
                        // genuine cold-load-on-warm-flag bug, not startup noise.
                        if let Some(ld_ms) = done_chunk.load_duration_ms {
                            if ld_ms > 5_000 {
                                warn!(
                                    session  = %session_id,
                                    trace_id = %trace_id,
                                    model    = %telemetry_model,
                                    load_ms  = ld_ms,
                                    "Unexpected cold-load on supposedly-warm model — \
                                     OS reclaimed mmap'd pages between the keepalive \
                                     ping and this request. If persistent, consider \
                                     lowering PRIMARY_KEEPALIVE_PING_INTERVAL_SECS."
                                );
                            }
                        }
                        // Normal completion: flush TTS, emit VadHint BEFORE is_final.
                        if let Some(ref tts_tx) = tts_tx_opt {
                            if let Some(remainder) = splitter.flush() {
                                last_sentence = Some(remainder.clone());
                                let _ = tts_tx.send(remainder);
                            }
                        }
                        if let Some(ref sentence) = last_sentence {
                            if let Some(frames) =
                                CoreOrchestrator::classify_expected_response(sentence)
                            {
                                let _ = send_vad_hint_bg(&tx, frames, &trace_id).await;
                            }
                        }
                        let _ = send_text_bg(&tx, "", true, &trace_id).await;
                        break;
                    }
                    Err(e) => {
                        error!(
                            session  = %session_id,
                            trace_id = %trace_id,
                            error    = %e,
                            "Stream chunk error mid-generation"
                        );
                        if let Some(ref tts_tx) = tts_tx_opt {
                            if let Some(remainder) = splitter.flush() {
                                let _ = tts_tx.send(remainder);
                            }
                        }
                        let _ = send_text_bg(&tx, "", true, &trace_id).await;
                        break;
                    }
                }
            }
        }
    }

    // Dropping tts_tx_opt closes the unbounded channel → TTS task's recv() sees None → exits.
    drop(tts_tx_opt);

    // Await TTS synthesis task cleanup.
    // Phase 27: do not send TTS is_final sentinel when cancelled — AudioPlayer.stop()
    // was already called by Swift on barge-in; awaitingFinalCallback was reset to false
    // in stop(), so any sentinel we send now would be ignored. Omitting it is cleaner.
    let tts_was_active = if let Some(handle) = tts_join_handle {
        let _ = handle.await;
        if !cancelled && intercepted_q.is_none() {
            // Send the is_final TTS audio sentinel — Swift arms the playback-complete callback.
            // When the last PCM buffer finishes playing, Swift sends AUDIO_PLAYBACK_COMPLETE
            // → orchestrator → EntityState::Idle (same as pre-Phase-27 behaviour).
            let sentinel = ServerEvent {
                trace_id: trace_id.clone(),
                event: Some(server_event::Event::AudioResponse(AudioResponse {
                    data: vec![],
                    sequence_number: 0, // AudioPlayer ignores seqnum on is_final=true
                    is_final: true,
                })),
            };
            let _ = tx.send(Ok(sentinel)).await;
            true
        } else {
            false
        }
    } else {
        false
    };

    // T1.4: assemble per-generation telemetry and emit a single structured log line.
    // Placed here (after TTS wait) so total_ms reflects the operator-visible latency
    // of the generation stage; TTS playback completion is deferred to APC and logged
    // separately via entity-state transitions.
    let total_elapsed = gen_started.elapsed();
    let first_token_ms = first_token_at.map(|t| t.duration_since(gen_started).as_millis() as u64);
    let telemetry = GenerationTelemetry {
        model: telemetry_model,
        first_token_ms,
        total_ms: total_elapsed.as_millis() as u64,
        token_count,
        cancelled,
        response_len: full_response.len(),
        agentic_depth,
    };
    info!(
        session        = %session_id,
        trace_id       = %trace_id,
        model          = %telemetry.model,
        first_token_ms = ?telemetry.first_token_ms,
        total_ms       = telemetry.total_ms,
        tokens         = telemetry.token_count,
        response_len   = telemetry.response_len,
        cancelled      = telemetry.cancelled,
        agentic_depth  = telemetry.agentic_depth,
        "gen_complete"
    );

    // Phase 38 / Codex [7]: clear the producer slot before delivering the
    // result. The producer task has already exited (done:true or the channel
    // closed); a stale Some() in the slot would let a subsequent cancel call
    // .abort() on a finished task — harmless but misleading in logs.
    if let Ok(mut g) = producer_abort_slot.lock() {
        *g = None;
    }

    // Deliver result. If the channel is closed (session ended), drop silently.
    let _ = gen_tx
        .send(GenerationResult {
            cancelled,
            full_response,
            intercepted_q,
            tts_was_active,
            trace_id,
            content,
            embed_model,
            is_shell_proactive: false,
            proactive_silent: false,
            agentic_depth,
            telemetry,
        })
        .await;
}

// ── run_shell_error_proactive_background ─────────────────────────────────────

/// Background task for shell-error proactive observations (Phase 31).
///
/// Runs fully independently — no reference to `CoreOrchestrator` after spawn.
/// Mirrors the `do_proactive_response` pipeline but as a `tokio::spawn`'d task
/// so `handle_shell_command` returns immediately and the `select!` loop is not
/// blocked during the ~1–3s inference call.
///
/// Delivers a `GenerationResult` via `gen_tx` when done:
///   - `proactive_silent = true`  → `handle_generation_complete` calls `undo_fire()`
///   - `proactive_silent = false` → slot stays burned (inference error or real response)
///
/// TTS delivery and IDLE transition are handled here.
/// `handle_generation_complete` skips both for `is_shell_proactive` results.
async fn run_shell_error_proactive_background(
    engine: crate::inference::engine::InferenceEngine,
    tx: mpsc::Sender<Result<ServerEvent, Status>>,
    session_id: String,
    model_name: String,
    messages: Vec<crate::inference::engine::Message>,
    trace_id: String,
    tts_arc: Option<Arc<tokio::sync::Mutex<Option<crate::voice::WorkerClient>>>>,
    gen_tx: mpsc::Sender<GenerationResult>,
    command: String, // structured log fields
    exit_code: i32,  // structured log fields
) {
    use crate::ipc::proto::{server_event, AudioResponse, EntityState, EntityStateChange};
    use crate::voice::protocol::msg;

    // T1.4: minimal telemetry for shell-proactive (non-streaming, single-shot).
    // We only track model + total_ms + cancelled; token-level counters aren't
    // meaningful because the response is collected in one shot via `collect_generation`.
    let proactive_started = std::time::Instant::now();
    let proactive_model_for_log = model_name.clone();
    let make_proactive_telemetry = |len: usize| GenerationTelemetry {
        model: proactive_model_for_log.clone(),
        first_token_ms: None,
        total_ms: proactive_started.elapsed().as_millis() as u64,
        token_count: 0,
        cancelled: false,
        response_len: len,
        agentic_depth: 0,
    };

    // Send EntityState without &mut self.
    // Proactive is non-streaming — barge-in cancellation is not supported;
    // self.current_state tracking is not needed for this path.
    let send_entity_state_bg = |state: EntityState| {
        let tx2 = tx.clone();
        let tid = trace_id.clone();
        async move {
            let evt = ServerEvent {
                trace_id: tid,
                event: Some(server_event::Event::EntityState(EntityStateChange {
                    state: state.into(),
                })),
            };
            let _ = tx2.send(Ok(evt)).await;
        }
    };

    // 1. THINKING — operator sees Dexter is active.
    send_entity_state_bg(EntityState::Thinking).await;

    // 2. Collect full response before displaying — enables [SILENT] check.
    //    30-second timeout matches collect_generation's budget.
    let req = crate::inference::engine::GenerationRequest {
        model_name,
        messages,
        temperature: None,
        unload_after: false, // FAST model — never unloaded after use
        keep_alive_override: None,
        num_predict: None,
        inactivity_timeout_override_secs: None,
        num_ctx_override: None,
    };

    let response_text: Option<String> = match engine.generate_stream(req).await {
        Err(e) => {
            warn!(
                session   = %session_id,
                trace_id  = %trace_id,
                command   = %command,
                exit_code = exit_code,
                error     = %e,
                "Shell error proactive: inference failed before streaming — slot burned"
            );
            None
        }
        Ok(mut rx) => {
            let mut full = String::new();
            let collect = async {
                while let Some(chunk_result) = rx.recv().await {
                    match chunk_result {
                        Ok(chunk) if !chunk.done => full.push_str(&chunk.content),
                        Ok(_done) => break,
                        Err(e) => {
                            warn!(
                                session  = %session_id,
                                trace_id = %trace_id,
                                error    = %e,
                                "Shell error proactive: chunk error — using partial"
                            );
                            break;
                        }
                    }
                }
            };
            match tokio::time::timeout(std::time::Duration::from_secs(30), collect).await {
                Ok(()) => Some(full),
                Err(_elapsed) => {
                    warn!(
                        session  = %session_id,
                        trace_id = %trace_id,
                        "Shell error proactive: timed out after 30s — slot burned"
                    );
                    None
                }
            }
        }
    };

    // 3. Inference error — slot stays burned; transition to IDLE.
    let response = match response_text {
        None => {
            send_entity_state_bg(EntityState::Idle).await;
            let telemetry = make_proactive_telemetry(0);
            let _ = gen_tx
                .send(GenerationResult {
                    cancelled: false,
                    full_response: String::new(),
                    intercepted_q: None,
                    tts_was_active: false,
                    trace_id,
                    content: String::new(),
                    embed_model: String::new(),
                    is_shell_proactive: true,
                    proactive_silent: false,
                    agentic_depth: 0,
                    telemetry,
                })
                .await;
            return;
        }
        Some(text) => text,
    };

    // 4. [SILENT] opt-out OR low-value filler — refund slot via handle_generation_complete.
    //    Phase 37.9: `should_suppress_proactive` covers bare time/date outputs in
    //    addition to literal [SILENT]. Shell-error proactive is less likely to emit
    //    a clock than the ambient path (the prompt asks for a specific fix), but
    //    the low-value filter is cheap enough to apply universally.
    if crate::proactive::ProactiveEngine::should_suppress_proactive(&response) {
        let demoted = !crate::proactive::ProactiveEngine::is_silent_response(&response);
        info!(
            session   = %session_id,
            trace_id  = %trace_id,
            command   = %command,
            exit_code = exit_code,
            demoted   = demoted,
            "Shell error proactive suppressed — slot will be refunded"
        );
        send_entity_state_bg(EntityState::Idle).await;
        let telemetry = make_proactive_telemetry(0);
        let _ = gen_tx
            .send(GenerationResult {
                cancelled: false,
                full_response: String::new(),
                intercepted_q: None,
                tts_was_active: false,
                trace_id,
                content: String::new(),
                embed_model: String::new(),
                is_shell_proactive: true,
                proactive_silent: true,
                agentic_depth: 0,
                telemetry,
            })
            .await;
        return;
    }

    info!(
        session   = %session_id,
        trace_id  = %trace_id,
        command   = %command,
        exit_code = exit_code,
        "Shell error proactive observation firing"
    );

    // 5. Send full text as a single is_final=true response.
    let _ = send_text_bg(&tx, response.trim(), true, &trace_id).await;

    // 6. TTS delivery — identical pattern to do_proactive_response §6.
    //    Identical TTS block: if this changes, update do_proactive_response too.
    let tts_was_active = if let Some(tts_arc) = tts_arc {
        let text_bytes = response.trim().as_bytes().to_vec();
        let session_tx = tx.clone();
        let trace_id_clone = trace_id.clone();

        let handle = tokio::spawn(async move {
            let mut guard: tokio::sync::MutexGuard<'_, Option<crate::voice::WorkerClient>> =
                tts_arc.lock().await;
            if let Some(client) = guard.as_mut() {
                if client
                    .write_frame(msg::TEXT_INPUT, &text_bytes)
                    .await
                    .is_ok()
                {
                    let mut seq = 0u32;
                    loop {
                        // Phase 38 / Codex [14]: bound per-frame read.
                        let frame_result = match tokio::time::timeout(
                            std::time::Duration::from_secs(
                                crate::constants::TTS_FRAME_READ_TIMEOUT_SECS,
                            ),
                            client.read_frame(),
                        )
                        .await
                        {
                            Err(_elapsed) => {
                                warn!(
                                    "TTS read_frame timed out after {}s — kokoro stalled, breaking out",
                                    crate::constants::TTS_FRAME_READ_TIMEOUT_SECS,
                                );
                                break;
                            }
                            Ok(r) => r,
                        };
                        match frame_result {
                            Ok(Some((msg::TTS_AUDIO, pcm))) => {
                                let evt = ServerEvent {
                                    trace_id: String::new(),
                                    event: Some(server_event::Event::AudioResponse(
                                        AudioResponse {
                                            data: pcm,
                                            sequence_number: seq,
                                            is_final: false,
                                        },
                                    )),
                                };
                                let _ = session_tx.send(Ok(evt)).await;
                                seq += 1;
                            }
                            Ok(Some((msg::TTS_DONE, _))) => {
                                // Phase 18: is_final sentinel arms playback-complete callback.
                                // Swift sends AUDIO_PLAYBACK_COMPLETE when last buffer finishes.
                                // orchestrator's handle_system_event transitions to Idle then.
                                let sentinel = ServerEvent {
                                    trace_id: trace_id_clone,
                                    event: Some(server_event::Event::AudioResponse(
                                        AudioResponse {
                                            data: vec![],
                                            sequence_number: seq,
                                            is_final: true,
                                        },
                                    )),
                                };
                                let _ = session_tx.send(Ok(sentinel)).await;
                                break;
                            }
                            Ok(Some(_)) => {}
                            _ => break,
                        }
                    }
                }
            }
        });
        let _ = handle.await;
        // IDLE deferred to AUDIO_PLAYBACK_COMPLETE — do NOT send Idle here.
        true
    } else {
        // No TTS — no playback to wait for; transition to IDLE directly.
        send_entity_state_bg(EntityState::Idle).await;
        false
    };

    // 7. Deliver result. handle_generation_complete sees is_shell_proactive=true,
    //    returns early, skips all regular post-processing.
    let telemetry = make_proactive_telemetry(response.len());
    info!(
        session  = %session_id,
        trace_id = %trace_id,
        model    = %telemetry.model,
        total_ms = telemetry.total_ms,
        response_len = telemetry.response_len,
        "gen_complete (shell_proactive)"
    );
    let _ = gen_tx
        .send(GenerationResult {
            cancelled: false,
            full_response: response,
            intercepted_q: None,
            tts_was_active,
            trace_id,
            content: String::new(),
            embed_model: String::new(),
            is_shell_proactive: true,
            proactive_silent: false,
            agentic_depth: 0,
            telemetry,
        })
        .await;
}

// ── extract_action_block ──────────────────────────────────────────────────────

/// Scan a model response for an embedded action block delimited by
/// `ACTION_BLOCK_OPEN` / `ACTION_BLOCK_CLOSE`. Returns the response with the
/// block stripped, and the parsed `ActionSpec` if a valid block was found.
///
/// If the JSON inside the block is malformed, logs a warning and returns the
/// original response unchanged with `None` for the spec — the text is still
/// shown to the operator rather than silently dropped.
fn extract_action_block(response: &str) -> (String, Option<ActionSpec>) {
    // ACTION_BLOCK_OPEN / ACTION_BLOCK_CLOSE are imported at the top of this module.
    let Some(open_pos) = response.find(ACTION_BLOCK_OPEN) else {
        return (response.to_string(), None);
    };

    let content_start = open_pos + ACTION_BLOCK_OPEN.len();

    // Primary path: explicit close delimiter present.
    if let Some(close_offset) = response[content_start..].find(ACTION_BLOCK_CLOSE) {
        let raw_json_str = response[content_start..content_start + close_offset].trim();
        let full_block_end = content_start + close_offset + ACTION_BLOCK_CLOSE.len();
        // qwen3 occasionally emits stray characters (e.g. `%` from zsh-prompt training
        // memory) between the closing `}` and the close tag: `}%</dexter:action>`.
        // Try raw first; if that fails, retry with everything after the last `}` stripped.
        let json_str = trim_to_last_brace(raw_json_str).unwrap_or(raw_json_str);
        return match serde_json::from_str::<ActionSpec>(json_str) {
            Ok(spec) => {
                let mut cleaned = response[..open_pos].to_string();
                cleaned.push_str(response[full_block_end..].trim_start());
                (cleaned, Some(spec))
            }
            Err(e) => {
                warn!(
                    error = %e,
                    json  = %json_str,
                    "Failed to parse action block JSON — treating response as plain text"
                );
                (response.to_string(), None)
            }
        };
    }

    // Fallback path: close delimiter absent.
    //
    // qwen3:8b reliably stops generation after the closing `}` of the JSON object
    // without emitting `</dexter:action>`. The stream ends, Ollama sends done=true,
    // and the token loop breaks — so the close tag never arrives.
    //
    // Recovery: treat everything from the open tag to end-of-response as the JSON
    // payload. If the tail contains trailing garbage (e.g. `}%`), strip everything
    // after the last `}` and retry. A truncated/malformed JSON falls through to warn.
    let tail = response[content_start..].trim();
    // First attempt: parse as-is (handles clean `{...}` with no close tag).
    if let Ok(spec) = serde_json::from_str::<ActionSpec>(tail) {
        let cleaned = response[..open_pos].trim_end().to_string();
        return (cleaned, Some(spec));
    }
    // Second attempt: strip trailing garbage after last `}` (handles `}%` pattern).
    if let Some(trimmed) = trim_to_last_brace(tail) {
        if trimmed != tail {
            if let Ok(spec) = serde_json::from_str::<ActionSpec>(trimmed) {
                let cleaned = response[..open_pos].trim_end().to_string();
                return (cleaned, Some(spec));
            }
        }
    }
    // Both attempts failed — log details and return original response unchanged.
    let preview = &tail[..tail.len().min(300)];
    let preview_hex = preview
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    warn!(
        post_open_text = %preview,
        post_open_hex  = %preview_hex,
        "Found ACTION_BLOCK_OPEN without close delimiter — treating as plain text"
    );
    (response.to_string(), None)
}

/// Return a subslice of `s` ending at (and including) the last `}` character,
/// or `None` if `s` contains no `}`. Used to strip model-generated trailing
/// garbage (e.g. the `%` zsh prompt character) from JSON action blocks.
fn trim_to_last_brace(s: &str) -> Option<&str> {
    let last_brace = s.rfind('}')?;
    Some(&s[..=last_brace])
}

// ── Command-query classifier ──────────────────────────────────────────────────

/// Returns `true` when `content` is an information request about a command
/// rather than a request to execute one.
///
/// The model (qwen3:8b) sometimes ignores the personality rule about command
/// queries and generates a shell action anyway. This function implements the
/// same rule at the Rust layer so it cannot be bypassed by the model.
///
/// Heuristic: check for question patterns that ask *about* a command/approach.
/// Execution-intent phrasing ("run", "execute", "do it", "go ahead") is
/// explicitly NOT matched to avoid blocking legitimate requests like "run the
/// ps command to show CPU usage."
fn is_command_query(content: &str) -> bool {
    let lc = content.to_lowercase();

    // Execution-intent phrases take priority — if present, it's a real action request.
    // These are specific directive forms, not question forms.  "run " alone is too broad
    // (matches "What command should I run to list processes?"), so we only match it when
    // preceded by imperative context ("please run", "can you run", etc.) or used with
    // direct-object phrasing ("run it", "run this", "run the ").
    let exec_signals = [
        "do it",
        "go ahead",
        "please run",
        "can you run",
        "run it",
        "run this",
        "run the ",
        "run now",
        "execute it",
        "execute the ",
        "execute this",
        "let's run",
        "just run",
        "go run",
    ];
    if exec_signals.iter().any(|s| lc.contains(s)) {
        return false;
    }

    // Question patterns that indicate the operator wants the command as text.
    let query_patterns = [
        "what command",
        "what's the command",
        "what is the command",
        "which command",
        "how do i",
        "how would i",
        "how can i",
        "what do i type",
        "what do i run",
        "what should i run",
        "what should i type",
        "how to list",
        "how to show",
        "how to find",
        "how to check",
        "how to see",
        "what command should",
        "tell me the command",
        "give me the command",
        "show me the command",
        "what's the syntax",
        "what is the syntax",
    ];
    query_patterns.iter().any(|p| lc.contains(p))
}

/// Phase 37 / B10: short human-readable label for a category, used in the
/// HUD approval warning. Not `Display` because the enum is proto-generated
/// and we don't want to lock the wording into the wire type.
fn category_label(cat: &ActionCategory) -> &'static str {
    match cat {
        ActionCategory::Safe => "safe",
        ActionCategory::Cautious => "cautious",
        ActionCategory::Destructive => "destructive",
        // Unspecified is the proto zero-value. PolicyEngine::classify() never
        // emits it; only wire-side defaults would produce it. Fall through to
        // the most conservative label so an unexpected value is never silently
        // shown as "safe" in a warning.
        ActionCategory::Unspecified => "destructive",
    }
}

/// Phase 37 / B8: detect whether the operator's request is scoped to a host
/// other than this Mac.
///
/// Dexter runs locally. When the operator says "what's the disk usage on my
/// linux box?" or "show me the running services on the server", executing
/// the corresponding shell command against *this* Mac is a category error —
/// it answers a question the operator didn't ask and, on unlucky wording,
/// mutates the wrong machine. The model also conflates "Linux command"
/// (a command form) with "run on Linux" (a target host); even a well-formed
/// `ps aux` issued on behalf of "my Debian server" would land on the Mac
/// and return mac-ps output.
///
/// This function returns true iff the utterance contains a phrase that
/// unambiguously names a *remote* host. The precedent is `is_command_query`:
/// when it fires, the Shell action is surfaced as text instead of executed.
///
/// Design notes:
///   - Conservative: only match phrases that are unambiguously off-host.
///     We do NOT match "on my computer" (could be this Mac) or generic
///     "remote" (can appear in legitimate local contexts like "remote branch").
///   - Mac-positive phrases are NOT listed — "on my mac", "on this machine",
///     "here" are implicit-local and should execute normally.
///   - Matches operate on the lowercased full utterance; phrases are chosen
///     so short-substring collisions are rare ("on the pi" won't match
///     "on the piano" because the phrase is bounded by a trailing space in
///     the utterance check — callers pass raw `content`, we check via
///     `contains` which allows word-internal matches. The specific phrases
///     are rare enough in normal English to tolerate this.)
fn is_off_host_request(content: &str) -> bool {
    let lc = content.to_lowercase();

    // Explicit non-Mac host naming. Each entry is a phrase the operator
    // would only utter when talking about a different machine.
    let off_host_phrases = [
        // Generic remote host references
        "on my linux box",
        "on my linux machine",
        "on my linux server",
        "on the linux box",
        "on the linux machine",
        "on my server",
        "on the server",
        "on my vm",
        "on the vm",
        "on my vps",
        "on the vps",
        "on my raspberry pi",
        "on the raspberry pi",
        "on my pi", // informal, but contextually remote
        "on the pi",
        // Distro-named hosts (operator identifies the target by OS flavor)
        "on my ubuntu",
        "on my debian",
        "on my fedora",
        "on my arch",
        "on my kali",
        "on my centos",
        "on my redhat",
        "on my rhel",
        "on my rocky",
        "on my alma",
        "on my nixos",
        "on my proxmox",
        "on my windows",
        "on my windows box",
        "on my windows machine",
        "on my pc", // PC in Mac-user speech almost always means "not this Mac"
        // Transport cues
        "ssh into",
        "over ssh",
        "via ssh",
        "ssh to ",
        // Infrastructure-y phrasing
        "on the remote host",
        "on the remote machine",
        "on the docker host",
        "in the container",
        "on the container",
        "on kubernetes",
        "on the cluster",
        "on the ec2",
        "on ec2",
    ];
    off_host_phrases.iter().any(|p| lc.contains(p))
}

/// Joke-request detector.
///
/// Returns true when the operator's utterance unambiguously asks for a joke.
/// Used by the router override that upgrades Fast → Primary on joke requests.
///
/// Background: qwen3:8b (FAST tier) handles joke requests poorly. Every joke
/// response opens with a hedge preamble ("I'm not sure if this qualifies as a
/// dad joke, but here goes:") and the model recycles the same handful of jokes
/// across distinct requests ("tell me a dad joke", "tell me a dirty joke",
/// "give me an adult joke" all return the same brothel/ladder joke). At 8B the
/// model lacks the breadth and tonal range to deliver humor reliably. Routing
/// to PRIMARY (gemma4:26b) gives both more variety and warmer delivery — the
/// same model that handles the rest of the operator's casual chat well.
///
/// Bias: tight detection. Routing a non-joke request to PRIMARY isn't harmful
/// (PRIMARY handles everything FAST does, just slower), but we don't want
/// every mention of "joke" to upgrade — "what's the joke about that bug?" is
/// referencing prior context, not requesting a joke.
pub(crate) fn is_joke_request(content: &str) -> bool {
    let lc = content.to_lowercase();

    // Hard requirement: the utterance must mention "joke" or "jokes". Without
    // it, this isn't a joke request at all, regardless of imperative form.
    if !lc.contains("joke") {
        return false;
    }

    // Soft requirement: the utterance must also contain a request signal —
    // imperative verb, want-statement, or iteration marker. This rules out
    // non-request mentions ("the joke is on me", "what's the joke about
    // that bug?", "i don't get the joke").
    //
    // Permissive design: any "tell me a [...] joke" / "give me [...] joke"
    // pattern matches regardless of what fills the brackets. That handles
    // "tell me a gay joke", "tell me a major groaner of a dad joke", "give
    // me a step-dad joke", and any other adjective the operator chooses
    // without requiring the override list to enumerate every variant.
    //
    // Critical for content-moderation surface: when phrasings like "tell me
    // a gay joke" don't match the override, they fall through to FAST
    // (qwen3:8b) which has aggressive identity-group safety training and
    // refuses with a moralizing lecture. Routing to PRIMARY (gemma4:26b)
    // avoids that specific refusal mode — gemma4 produces general humor
    // without identity-group reflexive refusal.
    let request_signals = [
        "tell me",
        "tell us",
        "give me",
        "give us",
        "tell a ",
        "tell another",
        "make me",
        "make a ",
        "write me",
        "write a ",
        "got a ",
        "got any ",
        "know a ",
        "know any ",
        "i want",
        "i'd like",
        "id like",
        "let's hear",
        "lets hear",
        "hit me with",
        // Iteration markers — operator iterating without restating "tell me"
        "another",
        "different ",
        "one more",
        "next ",
        "fresh ",
        "new ",
    ];
    request_signals.iter().any(|s| lc.contains(s))
}

fn is_step_dad_joke_format_request(content: &str) -> bool {
    if !is_joke_request(content) {
        return false;
    }

    let normalized = content.to_lowercase().replace('-', " ");
    normalized.contains("step dad joke")
        || normalized.contains("step dad jokes")
        || normalized.contains("stepdad joke")
        || normalized.contains("stepdad jokes")
}

fn canonicalize_step_dad_joke_request_for_inference(content: &str) -> Option<String> {
    if !is_step_dad_joke_format_request(content) {
        return None;
    }

    Some(
        "Comedy request: tell exactly one adult/NSFW dad-joke-style pun. \
         The operator used a format label, not a subject request. \
         A safe-for-work dad joke is the wrong answer. The punchline must contain \
         adult sexual innuendo or an explicit adult double meaning. \
         Do not make the joke about a stepdad, parent, family member, divorce, custody, \
         family reunion, or similar family premise. \
         Do not include the words stepdad or step-dad in the joke. \
         Use a fresh non-family setup and punchline. Output the joke only."
            .to_string(),
    )
}

/// Joke-continuation reference detector.
///
/// Returns true when the user's utterance plausibly continues a joke-iteration
/// thread: criticism ("not funny enough", "wasn't NSFW enough"), correction
/// ("it doesn't need to be about a step dad"), explanation requests ("explain
/// the joke", "i don't get it", "why is that funny"), or iteration imperatives
/// ("another one", "different one", "do better", "try again").
///
/// Used by the joke-continuation route override that upgrades FAST → PRIMARY
/// when the previous turn told a joke. Without continuation, qwen3:8b fields
/// these follow-ups and hallucinates joke content from training data instead
/// of grounding in the actual joke just told.
///
/// Bias: targeted detection over generic catchall. The detector is gated by
/// `last_joke_turn_at` — patterns here only fire when there IS a recent joke
/// context, so phrases like "do better" don't false-positive on engineering
/// requests.
pub(crate) fn is_joke_followup_reference(content: &str) -> bool {
    let t = content.to_lowercase();

    // Criticism / "not [adjective] enough" patterns. Operator pushing the
    // model to be funnier, dirtier, more on-tone, etc.
    let criticism_markers = [
        "not funny enough",
        "wasn't funny enough",
        "isn't funny enough",
        "not nsfw enough",
        "wasn't nsfw enough",
        "not dirty enough",
        "wasn't dirty enough",
        "not adult enough",
        "wasn't adult enough",
        "too wholesome",
        "too tame",
        "too mild",
        "too clean",
        "make it dirtier",
        "make it nastier",
        "make it raunchier",
        "make it more",
        "make it less",
        "more nsfw",
        "more adult",
        "more dirty",
        "more raunchy",
        "less wholesome",
        "less tame",
        "less mild",
        "do better",
        "try again",
        "do another one",
    ];
    if criticism_markers.iter().any(|p| t.contains(p)) {
        return true;
    }

    // Iteration / variation requests. Operator wants a different joke without
    // re-typing "tell me a joke".
    let iteration_markers = [
        "another one",
        "another joke",
        "different one",
        "different joke",
        "give me another",
        "do another",
        "tell me one",
        "tell one",
        "one then",
        "one more",
        "next one",
        "next joke",
    ];
    if iteration_markers.iter().any(|p| t.contains(p)) {
        return true;
    }

    // Identity-themed variation requests. These are intentionally narrow but
    // cover the live failure mode: after an ordinary joke, the operator may ask
    // for "a gay one" or "make it queer" without restating "joke".
    let identity_variation_markers = [
        "make it gay",
        "make it gayer",
        "make it queer",
        "make it queerer",
        "more gay",
        "more queer",
        "gay one",
        "queer one",
        "gay version",
        "queer version",
        "about gay",
        "about queer",
    ];
    if identity_variation_markers.iter().any(|p| t.contains(p)) {
        return true;
    }

    // Explanation / "i don't get it" markers. Operator asking the model to
    // explain its own joke. Critical case: prevents qwen3 from hallucinating
    // an explanation of a joke it never saw.
    //
    // "why is that " is intentionally permissive — it matches "why is that
    // funny", "why is that a dirty joke", "why is that nsfw", etc. The
    // detector is gated by `last_joke_turn_at` in the orchestrator, so
    // catch-everything-question-about-recent-utterance is safe in this
    // context. Outside joke context, "why is that the case" doesn't fire
    // because the gate is closed.
    let explanation_markers = [
        "explain the joke",
        "explain that joke",
        "explain it",
        "don't get it",
        "dont get it",
        "don't get the joke",
        "dont get the joke",
        "why is that ", // permissive: "why is that a dirty joke", "why is that funny", etc.
        "why is it funny",
        "what's funny about",
        "whats funny about",
        "what was the joke",
    ];
    if explanation_markers.iter().any(|p| t.contains(p)) {
        return true;
    }

    // Clarification of joke type / format. Operator correcting the model's
    // misinterpretation of what was wanted.
    let clarification_markers = [
        "that is wrong",
        "that's wrong",
        "thats wrong",
        "you define",
        "define as",
        "definition",
        "doesn't need to be about",
        "it's just the name",
        "that means",
        "the type of joke",
        "the kind of joke",
        "type of humor",
        "kind of humor",
    ];
    if clarification_markers.iter().any(|p| t.contains(p)) {
        return true;
    }

    false
}

/// Returns true when semantic memory recall should be suppressed for a humor turn.
///
/// Joke turns are already grounded by immediate conversation history. Pulling
/// semantically similar old joke turns from VectorStore is actively harmful:
/// a follow-up like "why is that a dirty joke?" can retrieve a different prior
/// dirty-joke exchange with higher embedding similarity than the joke that was
/// just told, giving the model a false referent.
pub(crate) fn should_suppress_joke_memory_recall(
    content: &str,
    last_joke_turn_at: Option<Instant>,
) -> bool {
    if is_joke_request(content) {
        return true;
    }

    let Some(last_joke) = last_joke_turn_at else {
        return false;
    };

    last_joke.elapsed().as_secs() <= crate::constants::JOKE_CONTINUATION_WINDOW_SECS
        && is_joke_followup_reference(content)
}

/// Vision-continuation reference detector.
///
/// Returns true when the user's utterance plausibly refers back to visual content
/// from a recent Vision turn — anaphoric pronouns, visual-property words, or
/// direct comparison/inspection idioms. Used by the route override that upgrades
/// Chat → Vision for follow-up questions about an already-shown image.
///
/// Bias: false-positive friendly. Routing a non-vision turn to Vision is
/// graceful — the model gets the current screen attached, and if the screen is
/// irrelevant the model just answers from text. Missing a real vision follow-up
/// is the harsh failure (8B model confidently hallucinates "navy blue dress
/// shirt"). When in doubt, route to Vision.
///
/// Phrases are bounded with leading/trailing space where a substring collision
/// would matter ("color" doesn't collide; "it" without bounding would match
/// "italic" so it's checked as a leading word only).
pub(crate) fn is_vision_followup_reference(content: &str) -> bool {
    let t = content.to_lowercase();
    let trimmed = t.trim();

    // Anaphoric sentence-starters. Bounded check: "it's red" matches; "italic
    // formatting" doesn't. The trailing space/apostrophe is part of the pattern.
    let anaphoric_starts = [
        "this ",
        "this'",
        "this is",
        "this one",
        "this thing",
        "that ",
        "that'",
        "that is",
        "that one",
        "that thing",
        "it ",
        "it's",
        "it is",
        "its ",
        "those ",
        "these ",
    ];
    if anaphoric_starts.iter().any(|p| trimmed.starts_with(p)) {
        return true;
    }

    // Visual-property words anywhere in the utterance. These are strongly
    // associated with discussing an image — color/shape/size/clothing/posture/
    // composition. A short list is intentional; expansion happens based on
    // operator-observed false negatives.
    let visual_words = [
        "color",
        "colour",
        "shade",
        "shape",
        "size",
        "outfit",
        "wearing",
        "shirt",
        "pants",
        "jacket",
        "vest",
        "cardigan",
        "sweater",
        "dress",
        "looks like",
        "look at",
        "the image",
        "the picture",
        "the photo",
        "the screen",
        "show me",
        "another one",
        "the other one",
        "bigger",
        "smaller",
        "darker",
        "lighter",
        "thicker",
        "thinner",
        "longer",
        "shorter",
        "wider",
        "narrower",
        // Measurement words — only asked in a visual-inspection context.
        // "girth?" alone is the canonical example: minimal sentence, but
        // unambiguous follow-up to a previous size estimate.
        "girth",
        "length",
        "depth",
        "height",
        "width",
        "circumference",
        "thickness",
        "diameter",
    ];
    if visual_words.iter().any(|w| t.contains(w)) {
        return true;
    }

    // Direct visual questions. Bounded with trailing space to reduce collisions.
    let visual_questions = [
        "how big",
        "how large",
        "how small",
        "how long",
        "how wide",
        "what color",
        "what colour",
        "what shape",
        "what size",
        "what's it",
        "what is it",
        "what about that",
        "what about this",
    ];
    if visual_questions.iter().any(|q| t.contains(q)) {
        return true;
    }

    false
}

/// Phase 37.9 / T8: detects operator self-send intent for iMessage.
///
/// Returns true when the operator's utterance unambiguously requests sending
/// a text to themselves. Used by the orchestrator's iMessage-send intercept
/// to rewrite LLM-generated Messages-send AppleScripts to a deterministic
/// template addressed to `operator_self_handle` — bypassing the Contacts
/// lookup phase where live-smoke T8 observed confabulation.
///
/// Design notes:
///   - Conservative phrase list. "text me" alone is not matched because it
///     appears in non-self constructions ("tell Bob to text me"). Matches
///     require a complete self-send INTENT: "text myself", "send me the
///     list", "message myself", etc. The "send me <article>" / "text me
///     <article>" patterns catch typical voice-dictated self-sends without
///     the bare "me" false positive.
///   - Aliases are matched whole-word, case-insensitive, preceded by a
///     send verb ("text", "send", "message", "imessage"). "message jason"
///     with alias "jason" returns true; "message jasonbourne" does NOT
///     (alphanumeric boundary enforced).
///   - Runs on the ORIGINAL operator utterance, not the LLM's proposed
///     script. This grounds the self-reference decision in operator intent,
///     not the model's improvisation (which is exactly what went wrong in
///     T8 — the model fabricated a recipient unrelated to "myself").
pub(crate) fn is_self_reference_request(content: &str, aliases: &[String]) -> bool {
    let lc = content.to_lowercase();

    // Delegation guard — relay/third-party requests must NOT match.
    //
    // "ask Alex to send me a code" contains the substring "send me a " which
    // would match SELF_PATTERNS below, but the operator's intent is for a
    // third party to send the operator something, not for Dexter to self-send.
    // These leading verbs ("ask", "tell", "remind", "have", "get") followed by
    // *anything* are relay requests by nature; if one appears at the start of
    // the utterance we short-circuit to `false` regardless of what comes after.
    //
    // Trimmed-leading match — handles "Hey, ask Alex …" loosely (a leading
    // "hey," prefix is a common voice-input artifact). The check is "starts
    // with one of these tokens after stripping a small set of pleasantry
    // prefixes", not a full parser.
    const DELEGATION_PREFIXES: &[&str] = &["ask ", "tell ", "remind ", "have ", "get ", "make "];
    let stripped_lead = lc
        .trim_start()
        .trim_start_matches("hey, ")
        .trim_start_matches("hey ")
        .trim_start_matches("ok ")
        .trim_start_matches("okay ")
        .trim_start_matches("please ");
    if DELEGATION_PREFIXES
        .iter()
        .any(|p| stripped_lead.starts_with(p))
    {
        return false;
    }

    // Narrow, high-confidence self-send phrasings.
    const SELF_PATTERNS: &[&str] = &[
        "text myself",
        "text to myself",
        "send myself",
        "send a text to myself",
        "send a message to myself",
        "message myself",
        "imessage myself",
        "send me a text",
        "send me a message",
        "send me an imessage",
        // "<verb> me the/a/my/that <obj>" — complete self-send intents.
        "text me the ",
        "text me a ",
        "text me my ",
        "text me that ",
        "send me the ",
        "send me a ",
        "send me my ",
        "send me that ",
        "message me the ",
        "message me a ",
        // Phase 38 / Codex finding [30]: the personality YAML's Rule 0 documents
        // these phrases as self-send forms ("ping me", "send it to my phone"),
        // but Rust didn't recognize them — so the model would emit `buddy "self"`
        // following YAML guidance and the intercept would NOT rewrite the
        // recipient. The delegation guard above ("ask Bob to ping me", etc.)
        // already protects against third-party-relay false positives.
        "ping me",
        "send it to my phone",
        "send to my phone",
        "send to my number",
    ];
    if SELF_PATTERNS.iter().any(|p| lc.contains(p)) {
        return true;
    }

    // Alias match: "<verb> <alias>" with a whole-word boundary after the alias.
    // "message jason" matches alias "jason"; "message jasonbourne" does not.
    const VERBS: &[&str] = &["text ", "send ", "message ", "imessage "];
    for alias in aliases {
        let alias_lc = alias.to_lowercase();
        if alias_lc.is_empty() {
            continue;
        }
        for verb in VERBS {
            let pattern = format!("{verb}{alias_lc}");
            let mut search_from = 0usize;
            while let Some(rel_idx) = lc[search_from..].find(&pattern) {
                let idx = search_from + rel_idx;
                let after = idx + pattern.len();
                let boundary_ok = match lc.as_bytes().get(after).copied() {
                    None => true,
                    Some(b) => !(b as char).is_alphanumeric(),
                };
                if boundary_ok {
                    return true;
                }
                search_from = idx + 1;
            }
        }
    }

    false
}

/// Phase 37.9 / T8: extracts the message body from a Messages-send AppleScript.
///
/// Parses the first `send "…" to …` construct, honoring AppleScript escape
/// sequences (`\"`, `\\`). Returns `None` if no valid send is found — callers
/// should treat that as "body could not be determined" and ask the operator
/// to restate rather than guessing.
///
/// Examples:
///   `send "hello" to targetBuddy`                → Some("hello")
///   `send "he said \"hi\"" to targetBuddy`       → Some(`he said \"hi\"`)
///   `tell application "Messages" to foo`         → None
pub(crate) fn extract_messages_body(script: &str) -> Option<String> {
    // Case-insensitive search for `send "`, then read the original-case
    // script forward from the quote to preserve message capitalization.
    let lc = script.to_lowercase();
    let marker = "send \"";
    let rel_start = lc.find(marker)?;
    let body_start = rel_start + marker.len();

    let bytes = script.as_bytes();
    let mut i = body_start;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if c == b'\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if c == b'"' {
            // Closing quote — return the slice.
            return Some(script[body_start..i].to_string());
        }
        i += 1;
    }
    None
}

/// Phase 37.9 / T8: builds a deterministic Messages-send AppleScript.
///
/// Used by the self-send intercept to replace LLM-generated scripts whose
/// recipient slot came from an untrusted Contacts lookup (or fabricated
/// outright). The script addresses `handle` directly via the iMessage
/// service, skipping Contacts entirely.
///
/// `body` is embedded with AppleScript-safe escaping: `\` → `\\`, `"` → `\"`,
/// and raw `\n` split into `" & linefeed & "` (AppleScript string literals
/// cannot contain raw newlines). `handle` receives the same quote/backslash
/// escaping as defense against odd config values.
pub(crate) fn build_self_send_script(handle: &str, body: &str) -> String {
    fn escape_applescript_literal(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\" & linefeed & \""),
                _ => out.push(c),
            }
        }
        out
    }
    let escaped_handle = escape_applescript_literal(handle);
    let escaped_body = escape_applescript_literal(body);
    format!(
        "tell application \"Messages\"\n\
         set targetService to 1st service whose service type = iMessage\n\
         set targetBuddy to buddy \"{escaped_handle}\" of targetService\n\
         send \"{escaped_body}\" to targetBuddy\n\
         end tell"
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Unit tests for the non-inference event handlers. These tests do not require
// a live Ollama instance — they use a real CoreOrchestrator constructed from
// a default DexterConfig but only exercise SystemEvent, UIAction, ActionApproval,
// ContextObserver integration, and the action block extraction function.
//
// The full handle_text_input pipeline is tested in the integration test in
// ipc/server.rs (gated #[ignore] — requires live Ollama with phi3:mini).

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Build a minimal DexterConfig pointing at a temp state directory.
    /// Uses default model/inference config (valid Ollama URL, no auto-pull).
    fn test_config(state_dir: &std::path::Path) -> DexterConfig {
        let mut cfg = DexterConfig::default();
        cfg.core.state_dir = state_dir.to_path_buf();
        cfg
    }

    fn new_trace() -> String {
        Uuid::new_v4().to_string()
    }
    fn new_session() -> String {
        Uuid::new_v4().to_string()
    }

    /// Construct a test orchestrator with an unbounded channel (never blocks).
    /// The receiver is returned so the caller can inspect sent events.
    fn make_orchestrator(
        state_dir: &std::path::Path,
    ) -> (
        CoreOrchestrator,
        tokio::sync::mpsc::UnboundedReceiver<Result<ServerEvent, Status>>,
    ) {
        let (tx_unbounded, rx) = tokio::sync::mpsc::unbounded_channel();
        // Adapt unbounded sender to bounded mpsc::Sender:
        // We wrap by creating a real bounded channel of capacity 64 and spawning
        // a forwarder — but that's complex for tests. Simpler: use a bounded channel
        // with generous capacity and assert it never fills in unit tests.
        let (tx, mut rx_bounded) = tokio::sync::mpsc::channel(64);

        // Forward from bounded rx to unbounded tx so callers can use the unbounded rx.
        tokio::spawn(async move {
            while let Some(item) = rx_bounded.recv().await {
                let _ = tx_unbounded.send(item);
            }
        });

        // Phase 24: action result channel — receiver dropped in tests that don't need it.
        let (action_tx, _action_rx) = tokio::sync::mpsc::channel(8);
        // Phase 27: generation result channel — receiver dropped in tests that don't need it.
        let (generation_tx, _generation_rx) = tokio::sync::mpsc::channel(4);

        let cfg = test_config(state_dir);
        let orch = CoreOrchestrator::new(
            &cfg,
            new_session(),
            tx,
            action_tx,
            generation_tx,
            crate::orchestrator::SharedDaemonState::new_degraded(),
        )
        .expect("CoreOrchestrator::new should succeed with default config");

        (orch, rx)
    }

    /// Same as `make_orchestrator` but also returns the `action_rx` receiver
    /// for tests that need to verify action result delivery.
    fn make_orchestrator_with_action_rx(
        state_dir: &std::path::Path,
    ) -> (
        CoreOrchestrator,
        tokio::sync::mpsc::UnboundedReceiver<Result<ServerEvent, Status>>,
        tokio::sync::mpsc::Receiver<ActionResult>,
    ) {
        let (tx_unbounded, rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, mut rx_bounded) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(item) = rx_bounded.recv().await {
                let _ = tx_unbounded.send(item);
            }
        });

        let (action_tx, action_rx) = tokio::sync::mpsc::channel(8);
        let (generation_tx, _generation_rx) = tokio::sync::mpsc::channel(4);

        let cfg = test_config(state_dir);
        let orch = CoreOrchestrator::new(
            &cfg,
            new_session(),
            tx,
            action_tx,
            generation_tx,
            crate::orchestrator::SharedDaemonState::new_degraded(),
        )
        .expect("CoreOrchestrator::new should succeed with default config");

        (orch, rx, action_rx)
    }

    /// Same as `make_orchestrator` but also returns `gen_rx` so tests can observe
    /// continuation generation deliveries (Phase 32 agentic chain verification).
    fn make_orchestrator_with_gen_rx(
        state_dir: &std::path::Path,
    ) -> (
        CoreOrchestrator,
        tokio::sync::mpsc::UnboundedReceiver<Result<ServerEvent, Status>>,
        tokio::sync::mpsc::Receiver<GenerationResult>,
    ) {
        let (tx_unbounded, rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, mut rx_bounded) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(item) = rx_bounded.recv().await {
                let _ = tx_unbounded.send(item);
            }
        });

        let (action_tx, _action_rx) = tokio::sync::mpsc::channel(8);
        let (generation_tx, gen_rx) = tokio::sync::mpsc::channel(4);

        let cfg = test_config(state_dir);
        let orch = CoreOrchestrator::new(
            &cfg,
            new_session(),
            tx,
            action_tx,
            generation_tx,
            crate::orchestrator::SharedDaemonState::new_degraded(),
        )
        .expect("CoreOrchestrator::new should succeed with default config");

        (orch, rx, gen_rx)
    }

    #[tokio::test]
    async fn previous_session_history_is_not_bootstrapped_into_prompt() {
        let tmp = tempfile::tempdir().unwrap();

        {
            let mut prior = SessionStateManager::new(
                tmp.path(),
                &new_session(),
                &crate::config::ModelConfig::default(),
            );
            prior.push_turn("user", "tell me a step-dad joke");
            prior.push_turn(
                "assistant",
                "I do not generate NSFW or sexually explicit content.",
            );
            prior.persist().expect("prior session should persist");
        }

        let (mut orch, _rx) = make_orchestrator(tmp.path());
        orch.context.push_user("tell me a dirty joke");

        let messages = orch.prepare_messages_for_inference(&[]);
        assert!(
            !messages.iter().any(|m| m
                .content
                .contains("I do not generate NSFW or sexually explicit content")),
            "raw previous-session refusal text must not be replayed into a new prompt"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.role == "user" && m.content.contains("tell me a dirty joke")),
            "current user request must still be present"
        );
    }

    #[tokio::test]
    async fn handle_system_event_connected_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let sys_event = SystemEvent {
            r#type: SystemEventType::Connected.into(),
            payload: String::new(),
        };
        let result = orch.handle_system_event(sys_event, new_trace()).await;
        assert!(result.is_ok(), "CONNECTED should return Ok(())");
    }

    #[tokio::test]
    async fn handle_system_event_app_focused_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let sys_event = SystemEvent {
            r#type: SystemEventType::AppFocused.into(),
            payload: r#"{"bundle_id":"com.apple.Xcode","name":"Xcode"}"#.to_string(),
        };
        let result = orch.handle_system_event(sys_event, new_trace()).await;
        assert!(result.is_ok(), "APP_FOCUSED should return Ok(())");
    }

    #[tokio::test]
    async fn handle_system_event_screen_locked_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let sys_event = SystemEvent {
            r#type: SystemEventType::ScreenLocked.into(),
            payload: String::new(),
        };
        let result = orch.handle_system_event(sys_event, new_trace()).await;
        assert!(result.is_ok(), "SCREEN_LOCKED should return Ok(())");
    }

    #[tokio::test]
    async fn handle_ui_action_dismiss_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let action = UiAction {
            r#type: crate::ipc::proto::UiActionType::Dismiss.into(),
            payload: String::new(),
        };
        let result = orch.handle_ui_action(action, new_trace()).await;
        assert!(result.is_ok(), "UIAction DISMISS should return Ok(())");
    }

    #[tokio::test]
    async fn handle_ui_action_drag_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let action = UiAction {
            r#type: crate::ipc::proto::UiActionType::Drag.into(),
            payload: r#"{"x":200,"y":400}"#.to_string(),
        };
        let result = orch.handle_ui_action(action, new_trace()).await;
        assert!(result.is_ok(), "UIAction DRAG should return Ok(())");
    }

    #[tokio::test]
    async fn handle_action_approval_approved_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let appr = ActionApproval {
            action_id: Uuid::new_v4().to_string(),
            approved: true,
            operator_note: String::new(),
        };
        let result = orch.handle_action_approval(appr, new_trace()).await;
        assert!(
            result.is_ok(),
            "ActionApproval(approved=true) should return Ok(())"
        );
    }

    #[tokio::test]
    async fn handle_action_approval_rejected_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let appr = ActionApproval {
            action_id: Uuid::new_v4().to_string(),
            approved: false,
            operator_note: "too risky".to_string(),
        };
        let result = orch.handle_action_approval(appr, new_trace()).await;
        assert!(
            result.is_ok(),
            "ActionApproval(approved=false) should return Ok(())"
        );
    }

    #[tokio::test]
    async fn handle_event_dispatches_system_event() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let event = ClientEvent {
            trace_id: new_trace(),
            session_id: new_session(),
            event: Some(client_event::Event::SystemEvent(SystemEvent {
                r#type: SystemEventType::Connected.into(),
                payload: String::new(),
            })),
        };
        let result = orch.handle_event(event).await;
        assert!(
            result.is_ok(),
            "handle_event should dispatch SystemEvent correctly"
        );
    }

    #[tokio::test]
    async fn handle_event_with_no_variant_returns_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let event = ClientEvent {
            trace_id: new_trace(),
            session_id: new_session(),
            event: None, // Malformed — no variant set.
        };
        let result = orch.handle_event(event).await;
        assert!(
            result.is_ok(),
            "None event variant should be silently ignored"
        );
    }

    // ── ContextObserver integration tests ─────────────────────────────────────
    //
    // These tests send real SystemEvents through handle_system_event and verify
    // that the internal ContextObserver snapshot is updated correctly.

    #[tokio::test]
    async fn handle_system_event_app_focused_updates_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let payload = r#"{"bundle_id":"com.apple.Xcode","name":"Xcode"}"#;
        let sys_event = SystemEvent {
            r#type: SystemEventType::AppFocused.into(),
            payload: payload.to_string(),
        };

        // Snapshot starts empty.
        assert!(orch.context_observer.snapshot().app_name.is_none());

        let result = orch.handle_system_event(sys_event, new_trace()).await;
        assert!(result.is_ok());

        // Snapshot must now reflect the focused app.
        let snap = orch.context_observer.snapshot();
        assert_eq!(snap.app_name.as_deref(), Some("Xcode"));
        assert_eq!(snap.app_bundle_id.as_deref(), Some("com.apple.Xcode"));
    }

    #[tokio::test]
    async fn handle_system_event_ax_element_changed_updates_element() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // First: focus an app so the snapshot has app identity.
        let focused = SystemEvent {
            r#type: SystemEventType::AppFocused.into(),
            payload: r#"{"bundle_id":"com.apple.Xcode","name":"Xcode"}"#.to_string(),
        };
        orch.handle_system_event(focused, new_trace())
            .await
            .unwrap();

        // Then: an element change within that app.
        let payload = r#"{"role":"AXTextField","label":"Source Editor","value_preview":"let x = 5","is_sensitive":false}"#;
        let elem_event = SystemEvent {
            r#type: SystemEventType::AxElementChanged.into(),
            payload: payload.to_string(),
        };
        let result = orch.handle_system_event(elem_event, new_trace()).await;
        assert!(result.is_ok());

        let snap = orch.context_observer.snapshot();
        // App identity preserved.
        assert_eq!(snap.app_name.as_deref(), Some("Xcode"));
        // Element updated.
        let el = snap.focused_element.as_ref().expect("element must be set");
        assert_eq!(el.role, "AXTextField");
        assert_eq!(el.label.as_deref(), Some("Source Editor"));
    }

    // ── extract_action_block tests ─────────────────────────────────────────────

    #[test]
    fn extract_action_block_no_block_returns_original() {
        let response = "This is a plain text response with no action.";
        let (text, spec) = extract_action_block(response);
        assert_eq!(text, response);
        assert!(spec.is_none());
    }

    #[test]
    fn extract_action_block_strips_and_parses_shell() {
        let response = format!(
            "I'll run ls for you.{}{}{}\nThat's the plan.",
            ACTION_BLOCK_OPEN, r#"{"type":"shell","args":["ls","-la"]}"#, ACTION_BLOCK_CLOSE,
        );
        let (text, spec) = extract_action_block(&response);

        assert!(
            !text.contains(ACTION_BLOCK_OPEN),
            "block delimiters must be stripped"
        );
        assert!(!text.contains(ACTION_BLOCK_CLOSE));
        assert!(
            text.contains("I'll run ls for you."),
            "surrounding text preserved"
        );

        match spec.expect("spec must be Some") {
            ActionSpec::Shell { args, .. } => {
                assert_eq!(args, vec!["ls", "-la"]);
            }
            other => panic!("expected Shell, got: {other:?}"),
        }
    }

    #[test]
    fn extract_action_block_malformed_json_returns_original() {
        let response = format!(
            "Let me try.{}NOT_VALID_JSON{}",
            ACTION_BLOCK_OPEN, ACTION_BLOCK_CLOSE,
        );
        let (text, spec) = extract_action_block(&response);
        assert_eq!(
            text, response,
            "original response must be preserved on parse error"
        );
        assert!(spec.is_none());
    }

    #[test]
    fn extract_action_block_unclosed_but_valid_json_parses_via_fallback() {
        // qwen3 stops generation after `}` without emitting </dexter:action>.
        // The fallback path must parse the JSON and extract the action.
        let response = format!(
            "Trying to act.{}{}",
            ACTION_BLOCK_OPEN,
            r#"{"type":"shell","args":["ls","-la"]}"#,
            // No ACTION_BLOCK_CLOSE
        );
        let (text, spec) = extract_action_block(&response);
        // Pre-tag text is preserved; action block is stripped.
        assert!(
            text.contains("Trying to act."),
            "pre-tag text must be preserved: got {text:?}"
        );
        assert!(
            !text.contains(ACTION_BLOCK_OPEN),
            "open tag must be stripped"
        );
        match spec.expect("fallback must parse valid JSON") {
            ActionSpec::Shell { args, .. } => assert_eq!(args, vec!["ls", "-la"]),
            other => panic!("expected Shell, got {other:?}"),
        }
    }

    #[test]
    fn extract_action_block_unclosed_and_malformed_json_returns_original() {
        // Close tag absent AND JSON is truncated/malformed — return original unchanged.
        let response = format!(
            "Trying to act.{}{{\"type\":\"shell\",\"args\":[",
            ACTION_BLOCK_OPEN,
            // No ACTION_BLOCK_CLOSE, truncated JSON
        );
        let (text, spec) = extract_action_block(&response);
        assert_eq!(
            text, response,
            "original response preserved when close is missing and JSON is malformed"
        );
        assert!(spec.is_none());
    }

    #[test]
    fn extract_action_block_percent_after_brace_with_close_tag() {
        // qwen3 emits `}%</dexter:action>` — the `%` zsh-prompt character sits between
        // the closing brace and the close tag. Primary path must strip it and parse.
        let response = format!(
            "Opening Finder.{}{}%{}",
            ACTION_BLOCK_OPEN,
            r#"{"type":"apple_script","script":"tell application \"Finder\" to activate","rationale":"open finder"}"#,
            ACTION_BLOCK_CLOSE,
        );
        let (text, spec) = extract_action_block(&response);
        assert!(
            text.contains("Opening Finder."),
            "pre-tag text preserved: {text:?}"
        );
        assert!(!text.contains(ACTION_BLOCK_OPEN));
        match spec.expect("must parse despite trailing %") {
            ActionSpec::AppleScript { .. } => {}
            other => panic!("expected AppleScript, got {other:?}"),
        }
    }

    #[test]
    fn extract_action_block_percent_after_brace_no_close_tag() {
        // qwen3 stops at `}%` with no close tag — fallback path must strip `%` and parse.
        let response = format!(
            "Opening Finder.{}{}%",
            ACTION_BLOCK_OPEN,
            r#"{"type":"apple_script","script":"tell application \"Finder\" to activate","rationale":"open finder"}"#,
            // No ACTION_BLOCK_CLOSE
        );
        let (text, spec) = extract_action_block(&response);
        assert!(
            text.contains("Opening Finder."),
            "pre-tag text preserved: {text:?}"
        );
        assert!(!text.contains(ACTION_BLOCK_OPEN));
        match spec.expect("fallback must parse after stripping %") {
            ActionSpec::AppleScript { .. } => {}
            other => panic!("expected AppleScript, got {other:?}"),
        }
    }

    // ── is_command_query unit tests ──────────────────────────────────────────

    #[test]
    fn is_command_query_detects_what_command_phrasing() {
        assert!(is_command_query(
            "What command should I run to list processes?"
        ));
        assert!(is_command_query("what's the command to show memory usage?"));
        assert!(is_command_query("What is the command for sorting by CPU?"));
        assert!(is_command_query("which command shows open ports?"));
    }

    #[test]
    fn is_command_query_detects_how_do_i_phrasing() {
        assert!(is_command_query("How do I list all running processes?"));
        assert!(is_command_query(
            "how would I check disk usage from terminal?"
        ));
        assert!(is_command_query(
            "how can i see what processes are using the most memory"
        ));
        assert!(is_command_query("how to show network connections?"));
    }

    #[test]
    fn is_command_query_detects_tell_give_show_me_phrasing() {
        assert!(is_command_query("Tell me the command to tail a log file."));
        assert!(is_command_query("give me the command for searching files"));
        assert!(is_command_query("Show me the command to kill a process."));
        assert!(is_command_query(
            "what's the syntax for grep with case insensitive?"
        ));
    }

    #[test]
    fn is_command_query_not_triggered_on_execution_intent() {
        // Execution verbs override the information-request detection.
        assert!(!is_command_query("Run the ps command to show CPU usage"));
        assert!(!is_command_query("Execute the command to list processes"));
        assert!(!is_command_query("Do it — show me the process list"));
        assert!(!is_command_query("Go ahead and check disk usage"));
        assert!(!is_command_query("Please run ps aux now"));
    }

    #[test]
    fn is_command_query_not_triggered_on_plain_action_requests() {
        // Direct action requests with no command-query language.
        assert!(!is_command_query(
            "Show me which processes are using the most memory"
        ));
        assert!(!is_command_query("List the running processes"));
        assert!(!is_command_query("Check the CPU usage"));
        assert!(!is_command_query("Open Finder"));
        assert!(!is_command_query("Download this file from the URL"));
    }

    // ── is_off_host_request unit tests (Phase 37 / B8) ───────────────────────

    #[test]
    fn is_off_host_detects_explicit_linux_host_phrasing() {
        assert!(is_off_host_request(
            "What's the disk usage on my linux box?"
        ));
        assert!(is_off_host_request(
            "Show me the running services on the server"
        ));
        assert!(is_off_host_request("check memory on my vm"));
        assert!(is_off_host_request(
            "what processes are running on my ubuntu"
        ));
        assert!(is_off_host_request("tail the auth log on my debian"));
    }

    #[test]
    fn is_off_host_detects_transport_cues() {
        assert!(is_off_host_request(
            "ssh into the backup host and check space"
        ));
        assert!(is_off_host_request("list services over ssh"));
        assert!(is_off_host_request("run uptime via ssh"));
    }

    #[test]
    fn is_off_host_detects_infrastructure_phrasing() {
        assert!(is_off_host_request("check disk on the remote host"));
        assert!(is_off_host_request("what's running on the docker host"));
        assert!(is_off_host_request("is nginx up on ec2"));
        assert!(is_off_host_request("restart the pod on the cluster"));
    }

    #[test]
    fn is_off_host_not_triggered_on_local_phrasing() {
        // Mac-positive or ambiguous-local phrasing must NOT match.
        assert!(!is_off_host_request("what's the disk usage on my mac"));
        assert!(!is_off_host_request("show me running processes here"));
        assert!(!is_off_host_request("check CPU on this machine"));
        assert!(!is_off_host_request("list files in my downloads folder"));
        assert!(!is_off_host_request("open Finder"));
        assert!(!is_off_host_request("what's using the most memory?"));
    }

    #[test]
    fn is_off_host_not_triggered_on_incidental_word_usage() {
        // Legitimate local requests that mention 'linux' or 'server' in passing
        // without the off-host phrase should pass through. Note that the
        // phrase-match list deliberately requires "on my linux box" or similar —
        // not the bare word "linux" — to keep false positives low.
        assert!(!is_off_host_request("what's a good linux distro?"));
        assert!(!is_off_host_request("is this a server-grade cpu?"));
        assert!(!is_off_host_request("what does the linux kernel do?"));
    }

    // ── is_vision_followup_reference unit tests ──────────────────────────────

    #[test]
    fn vision_followup_detects_anaphoric_pronouns() {
        // The exact phrasings the operator hit in the broken session.
        assert!(is_vision_followup_reference(
            "How big would you say that thing is?"
        ));
        assert!(is_vision_followup_reference("That isn't a sweater vest."));
        assert!(is_vision_followup_reference("That's not a joke."));
        assert!(is_vision_followup_reference("It's very light blue."));
        assert!(is_vision_followup_reference("This one is bigger."));
        assert!(is_vision_followup_reference("this is wrong"));
    }

    #[test]
    fn vision_followup_detects_visual_property_words() {
        // Property questions about an image — even without anaphora.
        assert!(is_vision_followup_reference("what color is the cardigan"));
        assert!(is_vision_followup_reference("describe the outfit"));
        assert!(is_vision_followup_reference("show me another one"));
        assert!(is_vision_followup_reference("the picture is blurry"));
        assert!(is_vision_followup_reference("the screen looks weird"));
    }

    #[test]
    fn vision_followup_detects_direct_visual_questions() {
        assert!(is_vision_followup_reference("how big is it"));
        assert!(is_vision_followup_reference("how large is the watermelon"));
        assert!(is_vision_followup_reference("what color is it"));
        assert!(is_vision_followup_reference("what shape is that"));
        assert!(is_vision_followup_reference("what about that"));
    }

    #[test]
    fn vision_followup_detects_comparative_descriptors() {
        // Operator says "show me a bigger one" — clearly a vision follow-up
        // even without explicit pronoun.
        assert!(is_vision_followup_reference(
            "want to see an even bigger one"
        ));
        assert!(is_vision_followup_reference("show me a smaller one"));
        assert!(is_vision_followup_reference("a darker version please"));
    }

    #[test]
    fn vision_followup_does_not_match_unrelated_chat() {
        // Phrases that should NOT trigger vision continuation. Bias is toward
        // false positives, but pure topic-shifts should pass through cleanly.
        assert!(!is_vision_followup_reference("what's the weather"));
        assert!(!is_vision_followup_reference("tell me a dad joke"));
        assert!(!is_vision_followup_reference("set a timer for ten minutes"));
        assert!(!is_vision_followup_reference("send a message to mom"));
        assert!(!is_vision_followup_reference("what time is it in tokyo"));
    }

    #[test]
    fn vision_followup_handles_capitalization() {
        // The detector must be case-insensitive — operator typing "What Color"
        // or "THIS IS WRONG" must still match.
        assert!(is_vision_followup_reference("What Color Is It"));
        assert!(is_vision_followup_reference("THIS IS WRONG"));
        assert!(is_vision_followup_reference("How Big"));
    }

    #[test]
    fn vision_followup_does_not_match_word_internal_substrings() {
        // 'it' as a sentence-starter must not match 'italic'. The bounded
        // anaphoric_starts list checks "it " with trailing space.
        assert!(!is_vision_followup_reference("italic formatting is fine"));
        assert!(!is_vision_followup_reference("italics matter here"));
    }

    #[test]
    fn vision_followup_detects_measurement_words() {
        // The canonical regression — operator types "girth?" as a one-word
        // follow-up to a size estimate. Pre-fix, this routed FAST and got a
        // hallucinated answer. Post-fix, it must route Vision.
        assert!(is_vision_followup_reference("girth?"));
        assert!(is_vision_followup_reference("length?"));
        assert!(is_vision_followup_reference("circumference"));
        assert!(is_vision_followup_reference("what's the depth"));
        assert!(is_vision_followup_reference("how's the thickness"));
    }

    // ── is_joke_request unit tests ────────────────────────────────────────────

    #[test]
    fn joke_request_detects_canonical_phrasings() {
        assert!(is_joke_request("tell me a joke"));
        assert!(is_joke_request("tell me a dad joke"));
        assert!(is_joke_request("tell me a dirty joke"));
        assert!(is_joke_request("tell me an adult joke"));
        assert!(is_joke_request("give me a joke"));
        assert!(is_joke_request("give me another joke"));
    }

    #[test]
    fn joke_request_detects_step_dad_variants() {
        // "step-dad joke" was a phrasing the operator used live; ensure
        // hyphenated and unhyphenated both match.
        assert!(is_joke_request("tell me a step-dad joke"));
        assert!(is_joke_request("tell me a step dad joke"));
    }

    #[test]
    fn joke_request_detects_imperative_followups() {
        assert!(is_joke_request("another joke"));
        assert!(is_joke_request("a different joke"));
        assert!(is_joke_request("got any jokes"));
        assert!(is_joke_request("know any jokes"));
        assert!(is_joke_request("i want another joke"));
    }

    #[test]
    fn joke_request_does_not_match_non_imperative_mentions() {
        // Mentioning "joke" without requesting one must NOT match. These are
        // common phrasings where the operator is referencing prior context or
        // commenting on a joke, not asking for a new one.
        assert!(!is_joke_request("that was a good joke"));
        assert!(!is_joke_request("the joke is on me"));
        assert!(!is_joke_request("what's the joke about"));
        assert!(!is_joke_request("i don't get the joke"));
        assert!(!is_joke_request("that joke was funny"));
    }

    #[test]
    fn joke_request_handles_capitalization() {
        assert!(is_joke_request("Tell Me A Joke"));
        assert!(is_joke_request("TELL ME A DAD JOKE"));
        assert!(is_joke_request("Give Me Another Joke"));
    }

    #[test]
    fn joke_request_matches_arbitrary_adjectives() {
        // Permissive matching: any "tell me a [...] joke" / "give me [...] joke"
        // pattern routes to PRIMARY regardless of what fills the brackets. This
        // is critical for content-moderation surface — "tell me a gay joke"
        // must NOT route to qwen3:8b (which refuses with a moralizing lecture
        // on identity-group humor) and must instead route to gemma4:26b which
        // produces humor without that reflexive refusal.
        assert!(is_joke_request("tell me a gay joke"));
        assert!(is_joke_request("tell me a major groaner of a dad joke"));
        assert!(is_joke_request("tell me a really filthy joke"));
        assert!(is_joke_request("give me a politically incorrect joke"));
        assert!(is_joke_request("write me a gay joke"));
        assert!(is_joke_request("hit me with another joke"));
        assert!(is_joke_request("i'd like a different joke"));
        assert!(is_joke_request("let's hear another one of those jokes"));
    }

    #[test]
    fn step_dad_joke_format_request_detects_label_variants() {
        assert!(is_step_dad_joke_format_request("tell me a step-dad joke"));
        assert!(is_step_dad_joke_format_request("tell me a step dad joke"));
        assert!(is_step_dad_joke_format_request("give me stepdad jokes"));
        assert!(!is_step_dad_joke_format_request(
            "tell me a joke about my stepdad"
        ));
        assert!(!is_step_dad_joke_format_request("tell me a dad joke"));
    }

    #[test]
    fn step_dad_joke_canonicalization_removes_literal_anchor() {
        let canonical = canonicalize_step_dad_joke_request_for_inference(
            "tell me a step-dad joke. That means a dad joke with a NSFW twist.",
        )
        .expect("step-dad joke label should canonicalize");

        assert!(canonical.contains("adult/NSFW dad-joke-style pun"));
        assert!(canonical.contains("not a subject request"));
        assert!(canonical.contains("safe-for-work dad joke is the wrong answer"));
        assert!(canonical.contains("adult sexual innuendo"));
        assert!(!canonical.contains("tell me a step-dad joke"));
    }

    // ── is_joke_followup_reference unit tests ────────────────────────────────

    #[test]
    fn joke_followup_detects_criticism_markers() {
        // The exact phrasings from the broken session.
        assert!(is_joke_followup_reference("that wasn't NSFW enough."));
        assert!(is_joke_followup_reference("not dirty enough"));
        assert!(is_joke_followup_reference("too wholesome"));
        assert!(is_joke_followup_reference("make it dirtier"));
        assert!(is_joke_followup_reference("make it more raunchy"));
        assert!(is_joke_followup_reference("more nsfw"));
    }

    #[test]
    fn joke_followup_detects_iteration_markers() {
        assert!(is_joke_followup_reference("another one"));
        assert!(is_joke_followup_reference("different one"));
        assert!(is_joke_followup_reference("give me another"));
        assert!(is_joke_followup_reference("tell me one then"));
        assert!(is_joke_followup_reference("one more"));
    }

    #[test]
    fn joke_followup_detects_identity_variation_markers() {
        assert!(is_joke_followup_reference("make it gay"));
        assert!(is_joke_followup_reference("make it gayer"));
        assert!(is_joke_followup_reference("a queer one"));
        assert!(is_joke_followup_reference("give me the gay version"));
    }

    #[test]
    fn joke_followup_detects_explanation_markers() {
        // The qwen3-hallucination-prevention case. "why is that a dirty joke"
        // must route PRIMARY so the model that told the joke also explains it.
        assert!(is_joke_followup_reference("why is that a dirty joke?"));
        assert!(is_joke_followup_reference("explain the joke"));
        assert!(is_joke_followup_reference("explain that joke"));
        assert!(is_joke_followup_reference("i don't get it"));
        assert!(is_joke_followup_reference("i dont get the joke"));
        assert!(is_joke_followup_reference("why is that funny"));
        assert!(is_joke_followup_reference("what's funny about that"));
    }

    #[test]
    fn joke_followup_detects_clarification_markers() {
        // Operator correcting model's misinterpretation of joke type.
        assert!(is_joke_followup_reference(
            "it doesn't need to be about a step dad"
        ));
        assert!(is_joke_followup_reference(
            "it's just the name for the type of joke"
        ));
        assert!(is_joke_followup_reference(
            "what do you define as a step dad joke?"
        ));
        assert!(is_joke_followup_reference(
            "That is wrong. A step-dad joke is a dad joke with a NSFW twist"
        ));
        assert!(is_joke_followup_reference("the kind of humor i mean"));
    }

    #[test]
    fn joke_followup_handles_capitalization() {
        assert!(is_joke_followup_reference("That Wasn't NSFW Enough"));
        assert!(is_joke_followup_reference("EXPLAIN THE JOKE"));
        assert!(is_joke_followup_reference("Make It Dirtier"));
    }

    #[test]
    fn joke_followup_does_not_match_unrelated_chat() {
        // The detector is GATED by `last_joke_turn_at` in the orchestrator,
        // but it should still avoid the most obvious false-positive cases on
        // its own. Pure non-joke utterances must not match.
        assert!(!is_joke_followup_reference("what's the weather"));
        assert!(!is_joke_followup_reference("send a text to mom"));
        assert!(!is_joke_followup_reference("set a timer"));
        assert!(!is_joke_followup_reference("describe the image"));
        assert!(!is_joke_followup_reference("hello"));
    }

    #[test]
    fn joke_memory_recall_suppressed_for_initial_requests() {
        assert!(should_suppress_joke_memory_recall(
            "tell me a dirty joke",
            None
        ));
        assert!(should_suppress_joke_memory_recall(
            "give me another joke",
            None
        ));
    }

    #[test]
    fn joke_memory_recall_suppressed_for_recent_followups() {
        let recent = Instant::now();
        assert!(should_suppress_joke_memory_recall(
            "why is that a dirty joke?",
            Some(recent),
        ));
        assert!(should_suppress_joke_memory_recall(
            "that wasn't NSFW enough",
            Some(recent),
        ));
    }

    #[test]
    fn joke_memory_recall_not_suppressed_after_window() {
        let stale = Instant::now()
            - std::time::Duration::from_secs(crate::constants::JOKE_CONTINUATION_WINDOW_SECS + 1);
        assert!(!should_suppress_joke_memory_recall(
            "why is that a dirty joke?",
            Some(stale),
        ));
    }

    #[tokio::test]
    async fn previous_assistant_joke_turn_is_preserved_in_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.context.push_user("tell me a dirty joke.");
        orch.context.push_assistant(
            "Why did the man put sugar under his pillow? Because he wanted sweet dreams.",
        );
        orch.context.push_user("why is that a dirty joke?");

        let messages = orch.prepare_messages_for_inference(&[]);
        assert!(
            messages.iter().any(|m| m.role == "assistant"
                && m.content.contains("sugar under his pillow")
                && m.content.contains("sweet dreams")),
            "the actual previous assistant joke must be present for explanation follow-ups"
        );
    }

    #[tokio::test]
    async fn comedy_mode_instruction_injected_for_identity_joke_request() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.context.push_user("tell me a gay joke");

        let messages = orch.prepare_messages_for_inference(&[]);
        assert!(
            messages.iter().any(|m| m.role == "system"
                && m.content.contains("Comedy mode:")
                && m.content.contains("special refusal category")
                && m.content.contains("Do not refuse")
                && m.content.contains("sanitize")
                && m.content.contains("Match the requested comedic premise")),
            "identity-themed joke requests must carry the comedy-mode no-refusal instruction"
        );
    }

    #[tokio::test]
    async fn comedy_mode_instruction_treats_step_dad_as_format_label() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.context.push_user("tell me a step-dad joke");

        let messages = orch.prepare_messages_for_inference(&[]);
        assert!(
            messages.iter().any(|m| m.role == "system"
                && m.content.contains("Comedy mode:")
                && m.content.contains("step-dad or step dad joke")
                && m.content.contains("format label")
                && m.content.contains("does not need to mention a stepdad")),
            "step-dad joke requests must not force literal stepdad/family subject matter"
        );
    }

    #[tokio::test]
    async fn step_dad_joke_request_is_rewritten_in_inference_prompt_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.context.push_user("tell me a step-dad joke");

        let messages = orch.prepare_messages_for_inference(&[]);
        let user_msg = messages
            .iter()
            .rev()
            .find(|m| m.role == "user" && !is_tool_result_content(&m.content))
            .expect("user message should be present");

        assert!(
            user_msg.content.contains("adult/NSFW dad-joke-style pun"),
            "inference prompt should carry canonical comedy format"
        );
        assert!(
            !user_msg.content.contains("tell me a step-dad joke"),
            "current inference turn should remove the sticky literal phrase"
        );
        assert_eq!(
            orch.context.messages().last().map(|m| m.content.as_str()),
            Some("tell me a step-dad joke"),
            "stored conversation history must keep the operator's original wording"
        );
    }

    #[tokio::test]
    async fn comedy_mode_instruction_discourages_repeating_recent_jokes() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.last_joke_turn_at = Some(Instant::now());
        orch.context.push_user("another one");

        let messages = orch.prepare_messages_for_inference(&[]);
        assert!(
            messages.iter().any(|m| m.role == "system"
                && m.content.contains("fresh setup")
                && m.content.contains("punchline")
                && m.content.contains("instead of repeating")),
            "joke follow-ups must explicitly ask for non-repeated premises"
        );
    }

    #[tokio::test]
    async fn comedy_mode_instruction_injected_for_recent_identity_joke_followup() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.last_joke_turn_at = Some(Instant::now());
        orch.context.push_user("make it gay");

        let messages = orch.prepare_messages_for_inference(&[]);
        assert!(
            messages.iter().any(|m| m.role == "system"
                && m.content.contains("Comedy mode:")
                && m.content.contains("Do not refuse")),
            "recent joke follow-ups like 'make it gay' must keep comedy-mode active"
        );
    }

    #[tokio::test]
    async fn comedy_mode_instruction_not_injected_for_unrelated_joke_mentions() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.context.push_user("that was a good joke");

        let messages = orch.prepare_messages_for_inference(&[]);
        assert!(
            !messages
                .iter()
                .any(|m| m.role == "system" && m.content.contains("Comedy mode:")),
            "mere joke mentions must not activate comedy-mode instructions"
        );
    }

    // ── is_self_reference_request unit tests (Phase 37.9 / T8) ────────────────

    #[test]
    fn is_self_reference_detects_myself_patterns() {
        let no_aliases: Vec<String> = vec![];
        assert!(is_self_reference_request(
            "text myself a reminder to buy milk",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "can you message myself the grocery list",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "send myself the weather forecast",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "send a text to myself",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "send a message to myself saying hello",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "imessage myself the address",
            &no_aliases
        ));
    }

    #[test]
    fn is_self_reference_detects_send_me_article_patterns() {
        let no_aliases: Vec<String> = vec![];
        assert!(is_self_reference_request(
            "text me the list of things to do",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "send me a reminder at noon",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "message me the address when you find it",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "send me my next appointment details",
            &no_aliases
        ));
    }

    #[test]
    fn is_self_reference_not_triggered_on_bare_text_me() {
        // "tell Bob to text me" contains "text me" but is not a self-send intent.
        // The narrow "text me the/a/my/that" patterns avoid this false positive.
        let no_aliases: Vec<String> = vec![];
        assert!(!is_self_reference_request(
            "tell Bob to text me",
            &no_aliases
        ));
        assert!(!is_self_reference_request(
            "remind Sarah to text me later",
            &no_aliases
        ));
        assert!(
            !is_self_reference_request("ask Alex to send me a code", &no_aliases),
            "'ask <name> to send me' is a third-party send, not self-send"
        );
    }

    // Phase 38 / Codex finding [30]: regression guard for the YAML-promised
    // self-send phrases ("ping me" / "send it to my phone") that Rust didn't
    // recognize before this fix.
    #[test]
    fn is_self_reference_detects_ping_me_and_phone_patterns() {
        let no_aliases: Vec<String> = vec![];
        assert!(is_self_reference_request("ping me at 3pm", &no_aliases));
        assert!(is_self_reference_request(
            "send it to my phone",
            &no_aliases
        ));
        assert!(is_self_reference_request(
            "send to my phone please",
            &no_aliases
        ));
        assert!(is_self_reference_request("send to my number", &no_aliases));
    }

    #[test]
    fn is_self_reference_ping_me_respects_delegation_guard() {
        // "ask Bob to ping me" should NOT be self-send — the delegation prefix
        // guard runs before SELF_PATTERNS matching.
        let no_aliases: Vec<String> = vec![];
        assert!(!is_self_reference_request(
            "ask Bob to ping me",
            &no_aliases
        ));
        assert!(!is_self_reference_request(
            "tell Sarah to send to my phone",
            &no_aliases
        ));
    }

    #[test]
    fn is_self_reference_not_triggered_on_unrelated_chat() {
        let no_aliases: Vec<String> = vec![];
        assert!(!is_self_reference_request("what time is it", &no_aliases));
        assert!(!is_self_reference_request("tell me a joke", &no_aliases));
        assert!(!is_self_reference_request(
            "what's the weather in Tokyo",
            &no_aliases
        ));
    }

    #[test]
    fn is_self_reference_detects_alias_match() {
        // Operator's nickname: "jay". Utterance "text jay my grocery list"
        // resolves to self-send.
        let aliases = vec!["jay".to_string()];
        assert!(is_self_reference_request(
            "text jay my grocery list",
            &aliases
        ));
        assert!(is_self_reference_request("send jay the weather", &aliases));
        assert!(is_self_reference_request(
            "message jay a reminder",
            &aliases
        ));
        assert!(is_self_reference_request("imessage jay hello", &aliases));
    }

    #[test]
    fn is_self_reference_alias_respects_word_boundary() {
        // Alias "jay" must NOT match "text jaywalker" (substring with continuation)
        // — the whole-word boundary after the alias prevents this.
        let aliases = vec!["jay".to_string()];
        assert!(
            !is_self_reference_request("text jaywalker the news", &aliases),
            "alias 'jay' must not match 'jaywalker' — alphanumeric boundary required"
        );
        assert!(
            !is_self_reference_request("message jayden the update", &aliases),
            "alias 'jay' must not match 'jayden'"
        );
    }

    #[test]
    fn is_self_reference_alias_allows_terminal_punctuation() {
        // "text jay" at end of utterance or with punctuation should still match.
        let aliases = vec!["jay".to_string()];
        assert!(
            is_self_reference_request("text jay", &aliases),
            "end-of-string boundary"
        );
        assert!(
            is_self_reference_request("text jay,", &aliases),
            "comma boundary"
        );
        assert!(
            is_self_reference_request("text jay.", &aliases),
            "period boundary"
        );
        assert!(
            is_self_reference_request("text jay the latest", &aliases),
            "space boundary"
        );
    }

    #[test]
    fn is_self_reference_empty_alias_is_ignored() {
        // An empty alias in the list must not match every utterance.
        let aliases = vec![String::new(), "jay".to_string()];
        assert!(
            !is_self_reference_request("what time is it", &aliases),
            "empty alias must not cause spurious self-send matches"
        );
        assert!(is_self_reference_request("text jay the list", &aliases));
    }

    // ── extract_messages_body unit tests (Phase 37.9 / T8) ────────────────────

    #[test]
    fn extract_messages_body_basic() {
        let script = r#"tell application "Messages"
set targetService to 1st service whose service type = iMessage
set targetBuddy to buddy "+15551234567" of targetService
send "hello world" to targetBuddy
end tell"#;
        assert_eq!(
            extract_messages_body(script).as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn extract_messages_body_preserves_case() {
        let script = r#"send "Hello World With CaseMix" to targetBuddy"#;
        assert_eq!(
            extract_messages_body(script).as_deref(),
            Some("Hello World With CaseMix")
        );
    }

    #[test]
    fn extract_messages_body_handles_escaped_quotes() {
        let script = r#"send "she said \"hi\"" to targetBuddy"#;
        // The extracted body keeps the AppleScript escapes as-is; re-encoding
        // is the caller's job (build_self_send_script re-escapes from raw).
        assert_eq!(
            extract_messages_body(script).as_deref(),
            Some("she said \\\"hi\\\"")
        );
    }

    #[test]
    fn extract_messages_body_returns_none_when_absent() {
        let script = r#"tell application "Contacts"
set m to first person whose name is "Alice"
end tell"#;
        assert!(
            extract_messages_body(script).is_none(),
            "scripts without a send construct should return None"
        );
    }

    #[test]
    fn extract_messages_body_returns_none_on_unterminated_string() {
        // Defensive: a malformed script with an unclosed quote must not hang
        // or panic — return None so the caller treats it as undeterminable.
        let script = r#"send "no closing quote ever comes"#;
        assert!(extract_messages_body(script).is_none());
    }

    // ── build_self_send_script unit tests (Phase 37.9 / T8) ───────────────────

    #[test]
    fn build_self_send_script_plain_body() {
        let s = build_self_send_script("+15551234567", "hello world");
        assert!(s.contains("tell application \"Messages\""));
        assert!(s.contains("buddy \"+15551234567\""));
        assert!(s.contains("send \"hello world\" to targetBuddy"));
        assert!(s.contains("end tell"));
    }

    #[test]
    fn build_self_send_script_escapes_embedded_quotes() {
        let s = build_self_send_script("+15551234567", r#"she said "hi""#);
        // The body's quotes are escaped via \" so the outer AppleScript string
        // literal stays valid.
        assert!(
            s.contains(r#"send "she said \"hi\"" to targetBuddy"#),
            "embedded quotes must be backslash-escaped; got: {s}"
        );
    }

    #[test]
    fn build_self_send_script_splits_newlines_into_linefeed() {
        let s = build_self_send_script("user@example.com", "line one\nline two");
        // Raw \n is split to `" & linefeed & "` because AppleScript string
        // literals cannot contain newlines.
        assert!(
            s.contains(r#"send "line one" & linefeed & "line two" to targetBuddy"#),
            "newline must become ' & linefeed & '; got: {s}"
        );
    }

    #[test]
    fn build_self_send_script_escapes_backslashes() {
        let s = build_self_send_script("+15551234567", r"path: C:\Users\me");
        // Backslash escaping: raw \ → \\
        assert!(
            s.contains(r#"send "path: C:\\Users\\me" to targetBuddy"#),
            "backslashes must be doubled; got: {s}"
        );
    }

    #[test]
    fn build_self_send_script_email_handle() {
        let s = build_self_send_script("user@example.com", "ok");
        assert!(
            s.contains("buddy \"user@example.com\""),
            "email handles must embed literally; got: {s}"
        );
    }

    // ── ActionApproval integration test ───────────────────────────────────────

    #[tokio::test]
    async fn handle_action_approval_unknown_id_returns_ok() {
        // An ActionApproval for an ID that was never registered as pending must not
        // panic or error — it's harmless (stale approval from a prior session).
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let appr = ActionApproval {
            action_id: "unknown-id-that-was-never-pending".to_string(),
            approved: true,
            operator_note: String::new(),
        };
        let result = orch.handle_action_approval(appr, new_trace()).await;
        assert!(result.is_ok(), "unknown approval must return Ok, not error");
    }

    // ── Phase 9: Retrieval pipeline integration tests ─────────────────────────

    #[tokio::test]
    async fn retrieval_pipeline_initializes_in_make_orchestrator() {
        // CoreOrchestrator::new() initializes the retrieval pipeline. This test verifies
        // construction succeeds without panic — the retrieval field is always initialized
        // (either real SQLite in the temp state dir, or in-memory fallback on open failure).
        let tmp = tempfile::tempdir().unwrap();
        let (_orch, _rx) = make_orchestrator(tmp.path());
        // Reaching this point without panicking confirms the retrieval pipeline
        // integrated correctly into CoreOrchestrator::new().
    }

    #[tokio::test]
    async fn retrieval_non_fatal_does_not_prevent_session_state_persist() {
        // The retrieval pipeline is wired into handle_text_input. Even if retrieval
        // operations fail (e.g., embedding model unavailable in CI), session state
        // must persist normally on shutdown. We verify by calling shutdown() and
        // confirming at least one file was created in the state directory.
        let tmp = tempfile::tempdir().unwrap();
        let (orch, _rx) = make_orchestrator(tmp.path());
        // shutdown() calls session_mgr.persist() which creates a session file.
        orch.shutdown().await;

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("state dir must be readable")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !entries.is_empty(),
            "state dir must contain at least one file after shutdown (session JSON or symlink)"
        );
    }

    // ── Phase 10: Voice coordinator integration tests ─────────────────────────

    #[tokio::test]
    async fn voice_coordinator_is_degraded_in_make_orchestrator() {
        // CoreOrchestrator::new() initialises VoiceCoordinator::new_degraded().
        // No TTS worker is spawned at construction — start_voice() does that.
        // Verify is_tts_available() is false straight after construction.
        let tmp = tempfile::tempdir().unwrap();
        let (orch, _rx) = make_orchestrator(tmp.path());
        assert!(
            !orch.voice.is_tts_available(),
            "Voice coordinator must be degraded (TTS unavailable) immediately after new()"
        );
    }

    #[tokio::test]
    async fn voice_tts_unavailable_does_not_break_text_generation_path() {
        // With voice degraded (is_tts_available() == false), handle_text_input() must
        // build the TTS channel branch correctly — the (None, None) path. Verify by
        // calling shutdown() after construction; session state must persist normally,
        // meaning no panic occurred in the voice-branching code path in new().
        let tmp = tempfile::tempdir().unwrap();
        let (orch, _rx) = make_orchestrator(tmp.path());
        assert!(!orch.voice.is_tts_available());
        // shutdown() exercises the voice.shutdown().await path (gracefully handles None client).
        orch.shutdown().await;
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("state dir must be readable")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !entries.is_empty(),
            "session state must persist even when voice is degraded"
        );
    }

    #[tokio::test]
    async fn orchestrator_degraded_notification_flags_start_false() {
        let tmp = tempfile::tempdir().unwrap();
        let (orch, _rx) = make_orchestrator(tmp.path());
        assert!(
            !orch.voice_degraded_notified,
            "voice_degraded_notified must be false on construction"
        );
        assert!(
            !orch.browser_degraded_notified,
            "browser_degraded_notified must be false on construction"
        );
    }

    #[tokio::test]
    async fn handle_system_event_hotkey_activated_transitions_to_listening() {
        // Verifies HOTKEY_ACTIVATED causes the orchestrator to emit EntityState::Listening.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx) = make_orchestrator(tmp.path());

        // Drain any events emitted during construction (CONNECTED, initial IDLE state, etc.)
        while rx.try_recv().is_ok() {}

        let evt = SystemEvent {
            r#type: crate::ipc::proto::SystemEventType::HotkeyActivated.into(),
            payload: "{}".to_string(),
        };
        orch.handle_system_event(evt, new_trace()).await.unwrap();

        // Yield once so the forwarder task (bounded→unbounded channel bridge in
        // make_orchestrator) has a chance to move the event into the unbounded rx.
        tokio::task::yield_now().await;

        let server_event = rx
            .try_recv()
            .expect("orchestrator must emit a server event after HOTKEY_ACTIVATED")
            .expect("server event must be Ok(...)");

        match server_event.event {
            Some(crate::ipc::proto::server_event::Event::EntityState(ref change)) => {
                assert_eq!(
                    change.state,
                    crate::ipc::proto::EntityState::Listening as i32,
                    "HOTKEY_ACTIVATED must transition entity to LISTENING"
                );
            }
            other => panic!("Expected EntityStateChange(Listening), got {:?}", other),
        }
    }

    // ── Phase 18: ConfigSync + AudioPlaybackComplete ──────────────────────────

    #[tokio::test]
    async fn handle_connected_sends_config_sync_after_idle() {
        // CONNECTED → exactly two events: EntityState(Idle) then ConfigSync.
        // ConfigSync must carry the default hotkey values.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx) = make_orchestrator(tmp.path());
        // Drain any pre-existing events from orchestrator construction.
        while rx.try_recv().is_ok() {}

        let evt = SystemEvent {
            r#type: crate::ipc::proto::SystemEventType::Connected.into(),
            payload: "{}".to_string(),
        };
        orch.handle_system_event(evt, new_trace()).await.unwrap();

        // Yield once to allow the channel bridge to forward the events.
        tokio::task::yield_now().await;

        // First event: EntityState(Idle)
        let first = rx
            .try_recv()
            .expect("CONNECTED must emit at least one event")
            .expect("event must be Ok(...)");
        assert!(
            matches!(
                first.event,
                Some(crate::ipc::proto::server_event::Event::EntityState(_))
            ),
            "first event from CONNECTED must be EntityStateChange"
        );

        // Second event: ConfigSync with default hotkey values
        let second = rx
            .try_recv()
            .expect("CONNECTED must emit a second event (ConfigSync)")
            .expect("event must be Ok(...)");
        match second.event {
            Some(crate::ipc::proto::server_event::Event::ConfigSync(ref cs)) => {
                let hk = cs
                    .hotkey
                    .as_ref()
                    .expect("ConfigSync must carry HotkeyConfig");
                assert_eq!(hk.key_code, 49, "default key_code must be 49 (kVK_Space)");
                assert!(hk.ctrl, "default ctrl must be true");
                assert!(hk.shift, "default shift must be true");
                assert!(!hk.cmd, "default cmd must be false");
                assert!(!hk.option, "default option must be false");
            }
            other => panic!("Expected ConfigSync, got {:?}", other),
        }

        // No further events.
        assert!(
            rx.try_recv().is_err(),
            "CONNECTED must produce exactly two events"
        );
    }

    #[tokio::test]
    async fn handle_audio_playback_complete_transitions_to_idle() {
        // AUDIO_PLAYBACK_COMPLETE → orchestrator emits EntityState(Idle).
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx) = make_orchestrator(tmp.path());
        while rx.try_recv().is_ok() {}

        let evt = SystemEvent {
            r#type: crate::ipc::proto::SystemEventType::AudioPlaybackComplete.into(),
            payload: "{}".to_string(),
        };
        orch.handle_system_event(evt, new_trace()).await.unwrap();

        tokio::task::yield_now().await;

        let server_event = rx
            .try_recv()
            .expect("AUDIO_PLAYBACK_COMPLETE must emit a server event")
            .expect("event must be Ok(...)");
        match server_event.event {
            Some(crate::ipc::proto::server_event::Event::EntityState(ref change)) => {
                assert_eq!(
                    change.state,
                    crate::ipc::proto::EntityState::Idle as i32,
                    "AUDIO_PLAYBACK_COMPLETE must transition entity to IDLE"
                );
            }
            other => panic!("Expected EntityStateChange(Idle), got {:?}", other),
        }
    }

    // ── Phase 19: action_awaiting_approval guard ──────────────────────────────

    #[tokio::test]
    async fn audio_playback_complete_skips_idle_when_action_pending() {
        // When action_awaiting_approval is true (set by handle_text_input on
        // PendingApproval), AUDIO_PLAYBACK_COMPLETE must NOT emit EntityState::Idle.
        // The entity must stay in ALERT until handle_action_approval() runs.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx) = make_orchestrator(tmp.path());
        while rx.try_recv().is_ok() {}

        // Simulate state after ActionOutcome::PendingApproval.
        orch.action_awaiting_approval = true;

        let evt = SystemEvent {
            r#type: crate::ipc::proto::SystemEventType::AudioPlaybackComplete.into(),
            payload: "{}".to_string(),
        };
        orch.handle_system_event(evt, new_trace()).await.unwrap();
        tokio::task::yield_now().await;

        assert!(
            rx.try_recv().is_err(),
            "AUDIO_PLAYBACK_COMPLETE must NOT emit any event when action is pending"
        );
    }

    #[tokio::test]
    async fn audio_playback_complete_sends_idle_after_action_flag_cleared() {
        // Demonstrates the full guard lifecycle:
        //   1. Flag set       → AUDIO_PLAYBACK_COMPLETE suppressed
        //   2. Flag cleared   → AUDIO_PLAYBACK_COMPLETE sends IDLE normally
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx) = make_orchestrator(tmp.path());
        while rx.try_recv().is_ok() {}

        // Step 1: flag set — event must be suppressed.
        orch.action_awaiting_approval = true;
        let evt = SystemEvent {
            r#type: crate::ipc::proto::SystemEventType::AudioPlaybackComplete.into(),
            payload: "{}".to_string(),
        };
        orch.handle_system_event(evt.clone(), new_trace())
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "event must be suppressed with flag=true"
        );

        // Step 2: flag cleared — event must send IDLE.
        orch.action_awaiting_approval = false;
        orch.handle_system_event(evt, new_trace()).await.unwrap();
        tokio::task::yield_now().await;
        let server_event = rx
            .try_recv()
            .expect("AUDIO_PLAYBACK_COMPLETE must emit IDLE after flag cleared")
            .expect("event must be Ok(...)");
        match server_event.event {
            Some(crate::ipc::proto::server_event::Event::EntityState(ref change)) => {
                assert_eq!(
                    change.state,
                    crate::ipc::proto::EntityState::Idle as i32,
                    "event must be EntityState(IDLE)"
                );
            }
            other => panic!("Expected EntityStateChange(Idle), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn action_approval_clears_action_awaiting_approval_flag() {
        // handle_action_approval must clear action_awaiting_approval before sending IDLE,
        // so that subsequent AUDIO_PLAYBACK_COMPLETE events transition normally.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx) = make_orchestrator(tmp.path());
        while rx.try_recv().is_ok() {}

        // Simulate state after PendingApproval.
        orch.action_awaiting_approval = true;

        // Operator approves (action_id unknown — resolve() handles it gracefully).
        let appr = ActionApproval {
            action_id: "test-action-id".to_string(),
            approved: true,
            operator_note: String::new(),
        };
        orch.handle_action_approval(appr, new_trace())
            .await
            .unwrap();

        // Flag must be cleared.
        assert!(
            !orch.action_awaiting_approval,
            "handle_action_approval must clear action_awaiting_approval"
        );

        // handle_action_approval must send IDLE (ALERT → IDLE transition).
        // Before IDLE, speak_action_feedback emits a TextResponse with the feedback text
        // (e.g. "Action cancelled."). Drain TextResponse events until we find EntityState.
        tokio::task::yield_now().await;
        let idle_evt = loop {
            let evt = rx
                .try_recv()
                .expect("handle_action_approval must emit EntityState(Idle)")
                .expect("event must be Ok(...)");
            match evt.event {
                Some(crate::ipc::proto::server_event::Event::TextResponse(_)) => continue,
                other_event => {
                    break ServerEvent {
                        trace_id: evt.trace_id,
                        event: other_event,
                    }
                }
            }
        };
        match idle_evt.event {
            Some(crate::ipc::proto::server_event::Event::EntityState(ref change)) => {
                assert_eq!(
                    change.state,
                    crate::ipc::proto::EntityState::Idle as i32,
                    "handle_action_approval must send IDLE"
                );
            }
            other => panic!("Expected EntityStateChange(Idle), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn context_summary_returns_some_after_app_focused_event() {
        // Verifies the data path: system event → context snapshot → context_summary().
        // Confirms context injection will have a non-None value to inject when the
        // operator is actively using an app.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // Before any event — no app focused.
        assert!(
            orch.context_observer.context_summary().is_none(),
            "Fresh orchestrator must have no context summary"
        );

        // Simulate APP_FOCUSED for Xcode.
        let evt = SystemEvent {
            r#type: crate::ipc::proto::SystemEventType::AppFocused.into(),
            payload: r#"{"bundle_id":"com.apple.dt.Xcode","name":"Xcode"}"#.to_string(),
        };
        orch.handle_system_event(evt, new_trace()).await.unwrap();

        // After the event — context summary must contain the app name.
        let summary = orch.context_observer.context_summary();
        assert!(
            summary.is_some(),
            "context_summary must return Some after APP_FOCUSED"
        );
        assert!(
            summary.unwrap().contains("Xcode"),
            "context_summary must contain the focused app name"
        );
    }

    // ── Phase 28: Clipboard context tests ────────────────────────────────────

    #[tokio::test]
    async fn clipboard_context_injected_as_user_turn_prefix() {
        // Round 3 / T0.5: clipboard is no longer a labelled system message.
        // It is folded into the LAST genuine user message as a "[Env · clipboard: …]"
        // prefix. This test verifies the new injection path end-to-end:
        //   CLIPBOARD_CHANGED system event
        //     → context_observer.update_from_clipboard_changed
        //     → prepare_messages_for_inference prefixes the user turn
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // Seed a user turn — without one, the prefix has nowhere to land.
        orch.context
            .push_user("What does this code do?".to_string());

        // Simulate CLIPBOARD_CHANGED arriving through handle_system_event.
        let clipboard_event = SystemEvent {
            r#type:  crate::ipc::proto::SystemEventType::ClipboardChanged.into(),
            payload: r#"{"text":"fn fibonacci(n: u64) -> u64 { if n <= 1 { n } else { fibonacci(n-1) + fibonacci(n-2) } }"}"#.to_string(),
        };
        orch.handle_system_event(clipboard_event, new_trace())
            .await
            .expect("handle_system_event must succeed for CLIPBOARD_CHANGED");

        let messages = orch.prepare_messages_for_inference(&[]);

        // No system-role message should carry the old "Clipboard:" label.
        assert!(
            !messages
                .iter()
                .any(|m| m.role == "system" && m.content.starts_with("Clipboard:")),
            "legacy 'Clipboard:' system message must not appear after T0.5"
        );

        // The genuine user message must be prefixed with the env block.
        let user_msg = messages
            .iter()
            .find(|m| m.role == "user" && !is_tool_result_content(&m.content))
            .expect("genuine user message must be present");
        assert!(
            user_msg.content.starts_with("[Env "),
            "user message must be prefixed with [Env ...] (got: {:?})",
            user_msg.content
        );
        assert!(
            user_msg.content.contains("fibonacci"),
            "env prefix must carry the clipboard text"
        );
        assert!(
            user_msg.content.contains("What does this code do?"),
            "original user query must survive the prefix injection"
        );
    }

    #[tokio::test]
    async fn memory_injection_remains_system_message_after_env_refactor() {
        // Round 3 / T0.5: clipboard moved from system-role to user-turn prefix.
        // Memory (Phase 21) must continue to ride on a system-role "Memory:" message —
        // it is reference knowledge, not turn-scoped operator context, and the model
        // needs to treat it as stable grounding rather than conversational framing.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // Clipboard present to prove the two pathways don't collide.
        let clipboard_event = SystemEvent {
            r#type: crate::ipc::proto::SystemEventType::ClipboardChanged.into(),
            payload: r#"{"text":"clipboard content here"}"#.to_string(),
        };
        orch.handle_system_event(clipboard_event, new_trace())
            .await
            .expect("CLIPBOARD_CHANGED must succeed");

        // User turn so the env prefix has somewhere to attach.
        orch.context
            .push_user("What's in my clipboard?".to_string());

        let recall = vec![crate::retrieval::store::MemoryEntry {
            id: "test-id".to_string(),
            content: "recalled fact".to_string(),
            source: crate::constants::MEMORY_SOURCE_CONVERSATION.to_string(),
            entry_type: "turn".to_string(),
            session_id: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            similarity: 0.9,
        }];
        let messages = orch.prepare_messages_for_inference(&recall);

        // Clipboard now on the user turn, NOT a system message.
        assert!(
            !messages
                .iter()
                .any(|m| m.role == "system" && m.content.starts_with("Clipboard:")),
            "legacy 'Clipboard:' system message must not appear after T0.5"
        );
        let user_msg = messages
            .iter()
            .find(|m| m.role == "user" && !is_tool_result_content(&m.content))
            .expect("user turn must be present");
        assert!(
            user_msg.content.contains("[Env \u{00B7} clipboard"),
            "clipboard must ride on the user-turn env prefix"
        );

        // Phase 37.8: memory entries with no/foreign session_id are framed as
        // "Reference notes from prior sessions …" rather than the legacy "Memory: …"
        // label. The structural invariant — system message precedes first user turn —
        // is preserved.
        let memory_idx = messages
            .iter()
            .position(|m| {
                m.role == "system" && m.content.starts_with("Reference notes from prior sessions")
            })
            .expect("prior-session reference block must be present when recall is non-empty");
        let first_user_idx = messages
            .iter()
            .position(|m| m.role == "user")
            .expect("user message must exist");
        assert!(
            memory_idx < first_user_idx,
            "reference block (idx {memory_idx}) must precede first user turn (idx {first_user_idx})"
        );
    }

    // ── Phase 37.8: cross-session retrieval leak fix ─────────────────────────
    //
    // Root cause: VectorStore stores turns as literal "User: X\nAssistant: Y"
    // strings. Injecting a prior-session row flat under "Memory: …" let the
    // "Assistant:" role marker bleed through gemma4's role parser, triggering
    // "I already told you this" hallucinations (Test 2, ret2libc).

    #[tokio::test]
    async fn prior_session_recall_is_framed_as_reference_not_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());
        orch.context.push_user("explain ret2libc".to_string());

        let recall = vec![crate::retrieval::store::MemoryEntry {
            id: "prior-1".to_string(),
            content: "User: explain ret2libc\nAssistant: ret2libc is a technique…".to_string(),
            source: "memory".to_string(),
            entry_type: "turn".to_string(),
            // Different session from orch.session_id → goes to prior-session bucket.
            session_id: Some("some-other-session-uuid".to_string()),
            created_at: "2026-04-18T23:51:53Z".to_string(),
            similarity: 0.92,
        }];

        let messages = orch.prepare_messages_for_inference(&recall);
        let block = messages
            .iter()
            .find(|m| {
                m.role == "system" && m.content.starts_with("Reference notes from prior sessions")
            })
            .expect("prior-session framing must appear");

        // Role markers neutralized.
        assert!(
            !block.content.contains("\nAssistant:"),
            "literal 'Assistant:' role marker must be rewritten to 'A:' to prevent role bleed-through (got: {:?})",
            block.content
        );
        assert!(
            block.content.contains("A: ret2libc is a technique"),
            "rewritten 'A:' form must carry the original answer text"
        );
        assert!(
            block.content.contains("Q: explain ret2libc"),
            "rewritten 'Q:' form must carry the original question"
        );
        assert!(
            block
                .content
                .contains("do not claim to have said any of this"),
            "framing must explicitly tell the model this is not part of the current conversation"
        );
    }

    #[tokio::test]
    async fn current_session_recall_uses_in_conversation_framing() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());
        let current_session = orch.session_id.clone();
        orch.context.push_user("continue".to_string());

        let recall = vec![crate::retrieval::store::MemoryEntry {
            id: "current-1".to_string(),
            content: "User: earlier question\nAssistant: earlier answer".to_string(),
            source: "memory".to_string(),
            entry_type: "turn".to_string(),
            session_id: Some(current_session),
            created_at: "2026-04-19T01:00:00Z".to_string(),
            similarity: 0.88,
        }];

        let messages = orch.prepare_messages_for_inference(&recall);

        assert!(
            messages
                .iter()
                .any(|m| m.role == "system"
                    && m.content.starts_with("Earlier in this conversation:")),
            "current-session recall must use 'Earlier in this conversation:' framing"
        );
        // Current-session entries KEEP the original User:/Assistant: format —
        // they genuinely are part of this conversation, so role markers are
        // semantically honest.
        let block = messages
            .iter()
            .find(|m| m.role == "system" && m.content.starts_with("Earlier in this conversation:"))
            .unwrap();
        assert!(
            block.content.contains("Assistant: earlier answer"),
            "current-session entries must not be role-neutralized (format is honest)"
        );
        // And must NOT be framed as cross-session reference material.
        assert!(
            !messages.iter().any(|m| m.role == "system"
                && m.content.starts_with("Reference notes from prior sessions")),
            "pure-current-session recall must not emit prior-session framing"
        );
    }

    #[tokio::test]
    async fn mixed_session_recall_partitions_into_two_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());
        let current_session = orch.session_id.clone();
        orch.context.push_user("next".to_string());

        let recall = vec![
            crate::retrieval::store::MemoryEntry {
                id: "a".to_string(),
                content: "User: old Q\nAssistant: old A".to_string(),
                source: "memory".to_string(),
                entry_type: "turn".to_string(),
                session_id: Some("foreign".to_string()),
                created_at: "2026-04-10T00:00:00Z".to_string(),
                similarity: 0.8,
            },
            crate::retrieval::store::MemoryEntry {
                id: "b".to_string(),
                content: "User: fresh Q\nAssistant: fresh A".to_string(),
                source: "memory".to_string(),
                entry_type: "turn".to_string(),
                session_id: Some(current_session),
                created_at: "2026-04-19T01:10:00Z".to_string(),
                similarity: 0.85,
            },
        ];

        let messages = orch.prepare_messages_for_inference(&recall);
        let prior_idx = messages
            .iter()
            .position(|m| {
                m.role == "system" && m.content.starts_with("Reference notes from prior sessions")
            })
            .expect("prior-session block expected");
        let current_idx = messages
            .iter()
            .position(|m| {
                m.role == "system" && m.content.starts_with("Earlier in this conversation:")
            })
            .expect("current-session block expected");
        assert!(
            prior_idx < current_idx,
            "prior-session block ({prior_idx}) must appear before current-session block ({current_idx}) — disclaimer must be read first"
        );
    }

    #[tokio::test]
    async fn neutralize_role_markers_only_rewrites_line_starts() {
        // Incidental occurrences inside a sentence must not be rewritten.
        let input = "User: real question\n\
                     Assistant: the user said 'User: foo' in a prior context\n\
                     Assistant: also the Assistant: label appears mid-line";
        let out = super::neutralize_role_markers(input);

        // Line-start User:/Assistant: rewritten.
        assert!(out.starts_with("Q: real question"));
        // The mid-line "User: foo" quoted inside an answer must be untouched.
        assert!(
            out.contains("'User: foo'"),
            "mid-line 'User:' must not be rewritten — it's a quoted literal, not a role marker"
        );
        assert!(
            out.contains("the Assistant: label"),
            "mid-line 'Assistant:' must not be rewritten"
        );
        // Both Assistant: line-starts rewritten.
        assert_eq!(out.matches("A: ").count(), 2);
        assert_eq!(out.matches("\nAssistant: ").count(), 0);
    }

    // ── Phase 30: Shell context tests ─────────────────────────────────────────

    #[tokio::test]
    async fn shell_context_injected_as_user_turn_prefix() {
        // Round 3 / T0.5: shell context is folded into the user-turn env prefix
        // instead of a labelled system message. Verifies the full data path:
        //   handle_shell_command → context_observer.update_shell_command
        //     → prepare_messages_for_inference writes [Env · shell: …] on the user turn.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.handle_shell_command(
            "cargo build".to_string(),
            "/Users/test/project".to_string(),
            Some(1),
        )
        .await;

        // User turn required for the env prefix to attach.
        orch.context.push_user("what just happened?".to_string());

        let messages = orch.prepare_messages_for_inference(&[]);

        // No legacy "Shell:" system message should be present.
        assert!(
            !messages
                .iter()
                .any(|m| m.role == "system" && m.content.starts_with("Shell:")),
            "legacy 'Shell:' system message must not appear after T0.5"
        );

        let user_msg = messages
            .iter()
            .find(|m| m.role == "user" && !is_tool_result_content(&m.content))
            .expect("user turn must be present");

        assert!(
            user_msg.content.starts_with("[Env "),
            "user turn must be prefixed with [Env ...] when shell context is fresh (got: {:?})",
            user_msg.content
        );
        assert!(
            user_msg.content.contains("cargo build"),
            "command must appear in env prefix"
        );
        assert!(
            user_msg.content.contains("exit 1"),
            "exit code must appear in env prefix"
        );
        assert!(
            user_msg.content.contains("/Users/test/project"),
            "cwd must appear in env prefix"
        );
        assert!(
            user_msg.content.contains("what just happened?"),
            "original query must survive"
        );
    }

    #[tokio::test]
    async fn shell_context_not_injected_when_none() {
        // Baseline: orchestrator with no shell command and no clipboard produces no
        // env prefix on the user turn.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.context.push_user("hello".to_string());

        let messages = orch.prepare_messages_for_inference(&[]);
        let user_msg = messages
            .iter()
            .find(|m| m.role == "user" && !is_tool_result_content(&m.content))
            .expect("user turn must be present");
        assert!(
            !user_msg.content.starts_with("[Env "),
            "user turn must not carry [Env ...] when neither clipboard nor shell context exist"
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.role == "system" && m.content.contains("Shell:")),
            "legacy 'Shell:' system message must not appear"
        );
    }

    // ── Phase 31: Shell error proactive gate tests ────────────────────────────

    #[tokio::test]
    async fn handle_shell_command_success_does_not_attempt_proactive() {
        // Exit 0 must not trigger proactive. Verified by checking context is updated
        // and documenting the is_error guard. In the test environment the startup grace
        // period also suppresses proactive, but the is_error guard fires first.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.handle_shell_command("cargo build".to_string(), "/tmp".to_string(), Some(0))
            .await;

        let snap = orch.context_observer.snapshot();
        assert_eq!(
            snap.last_shell_command.as_ref().map(|s| s.exit_code),
            Some(Some(0)),
            "context must be updated even for exit code 0"
        );
    }

    #[tokio::test]
    async fn handle_shell_command_ctrl_c_does_not_attempt_proactive() {
        // Exit 130 = SIGINT (Ctrl+C) — operator deliberately stopped the process.
        // Must not trigger proactive regardless of rate-limit state.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.handle_shell_command("sleep 60".to_string(), "/tmp".to_string(), Some(130))
            .await;

        let snap = orch.context_observer.snapshot();
        assert_eq!(
            snap.last_shell_command.as_ref().map(|s| s.exit_code),
            Some(Some(130)),
            "Ctrl+C must update context but not trigger proactive"
        );
    }

    #[tokio::test]
    async fn handle_shell_command_none_exit_code_does_not_attempt_proactive() {
        // None exit code — hook couldn't capture a result. Must not trigger proactive.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        orch.handle_shell_command("some_cmd".to_string(), "/tmp".to_string(), None)
            .await;

        let snap = orch.context_observer.snapshot();
        assert!(
            snap.last_shell_command
                .as_ref()
                .map_or(false, |s| s.exit_code.is_none()),
            "None exit code must be stored in context without proactive attempt"
        );
    }

    // ── Phase 20: Vision Integration tests ───────────────────────────────────

    #[test]
    fn screen_capture_constants_are_valid() {
        // SCREEN_CAPTURE_PATH_PREFIX must be under /tmp so the ephemeral per-invocation
        // capture files do not pollute the operator's home directory or state dir.
        // The full path is: SCREEN_CAPTURE_PATH_PREFIX + "_" + UUID + ".png"
        assert!(
            crate::constants::SCREEN_CAPTURE_PATH_PREFIX.starts_with("/tmp"),
            "SCREEN_CAPTURE_PATH_PREFIX must be under /tmp: {}",
            crate::constants::SCREEN_CAPTURE_PATH_PREFIX
        );
        // The prefix must not end in "/" or ".png" — the suffix logic appends "_<uuid>.png"
        // and a trailing slash or extension would produce a malformed path.
        assert!(
            !crate::constants::SCREEN_CAPTURE_PATH_PREFIX.ends_with('/'),
            "SCREEN_CAPTURE_PATH_PREFIX must not end with '/'"
        );
        assert!(
            !crate::constants::SCREEN_CAPTURE_PATH_PREFIX.ends_with(".png"),
            "SCREEN_CAPTURE_PATH_PREFIX must not end with '.png' (suffix appended at call time)"
        );
        // SCREEN_CAPTURE_TIMEOUT_SECS must be non-zero to prevent an immediate
        // timeout race where the capture never gets a chance to complete.
        assert!(
            crate::constants::SCREEN_CAPTURE_TIMEOUT_SECS > 0,
            "SCREEN_CAPTURE_TIMEOUT_SECS must be positive"
        );
    }

    #[test]
    fn vision_messages_have_no_images_before_capture_attachment() {
        // prepare_messages_for_inference() must return messages with images = None.
        // Images are ephemeral — they are attached AFTER this call in handle_text_input,
        // never stored in ConversationContext. This test guards against accidental
        // persistence of images across turns.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let (action_tx, _) = tokio::sync::mpsc::channel(8);
        let (generation_tx, _) = tokio::sync::mpsc::channel(4);
        let orch = CoreOrchestrator::new(
            &cfg,
            "test-session".to_string(),
            tx,
            action_tx,
            generation_tx,
            crate::orchestrator::SharedDaemonState::new_degraded(),
        )
        .expect("orchestrator creation should succeed");

        let messages = orch.prepare_messages_for_inference(&[]);
        for msg in &messages {
            assert!(
                msg.images.is_none(),
                "Message from prepare_messages_for_inference must have images=None \
                 (role: {}, content_len: {})",
                msg.role,
                msg.content.len()
            );
        }
    }

    #[test]
    fn vision_image_attaches_to_user_message_when_tool_result_follows() {
        // Issue 2 regression test: Phase 19 + Phase 20 combined path.
        //
        // A query like "look at this screenshot - what version of Safari is running?"
        // matches BOTH the vision keyword ("look at this") AND is_retrieval_first_query
        // ("what version of"). In that combined path:
        //   1. Retrieval-first fires → push_tool_result(...) appends a synthetic user
        //      message to context with content prefix "[Retrieved: ...]"
        //   2. Router returns Category::Vision → capture_screen() → image in hand
        //   3. Step 4d must attach the image to the GENUINE user query, NOT the
        //      trailing tool-result injection.
        //
        // Round 3 fix: tool results now ride on role="user" (custom roles like
        // "retrieval" were silently dropped by base-instruct Ollama models). The
        // vision attachment code must therefore skip role="user" messages whose
        // content starts with a known tool-result prefix.
        use crate::inference::engine::Message;

        let fake_b64 = "dGVzdGltYWdl".to_string();

        // Simulate the message list after prepare_messages_for_inference() with a
        // retrieval tool_result injected (now as role="user" per the Round 3 fix):
        //   [0] system: personality
        //   [1] user: the genuine vision query
        //   [2] user: "[Retrieved: ...]" synthetic injection
        let mut messages = vec![
            Message::system("You are Dexter."),
            Message::user("look at this screenshot — what version of Safari is running?"),
            Message::retrieval("[Retrieved: safari.com] Safari 18.3.1"),
        ];

        // Replicate the Round 3 vision-attach logic: skip tool-result user turns.
        let target = messages
            .iter_mut()
            .rev()
            .find(|m| m.role == "user" && !is_tool_result_content(&m.content));
        if let Some(last_user) = target {
            last_user.images = Some(vec![fake_b64.clone()]);
        }

        // The genuine user message (index 1) must have the image attached.
        assert_eq!(messages[1].role, "user");
        assert_eq!(
            messages[1].images.as_ref().map(|v| v.len()),
            Some(1),
            "genuine user message must have exactly one image attached"
        );
        assert_eq!(messages[1].images.as_ref().unwrap()[0], fake_b64);

        // The trailing retrieval message must be unchanged — no images.
        // Its content-prefix is the signal that kept is_tool_result_content() from matching.
        assert_eq!(messages[2].role, "user");
        assert!(
            is_tool_result_content(&messages[2].content),
            "retrieval injection must be classified as tool-result content"
        );
        assert!(
            messages[2].images.is_none(),
            "tool-result message must never receive image attachment"
        );

        // The system message must also be unchanged.
        assert!(messages[0].images.is_none());
    }

    #[tokio::test]
    async fn capture_screen_returns_none_gracefully_when_screencapture_fails() {
        // When screencapture is not available or fails (e.g., in CI environments
        // without a display), capture_screen() must return None without panicking.
        // This test verifies graceful degradation — the vision path continues
        // text-only rather than erroring out.
        //
        // Note: on macOS development machines this will actually run screencapture.
        // In that case it may return Some (success) or None (no display in CI).
        // The test only asserts that the function does not panic.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let (action_tx, _) = tokio::sync::mpsc::channel(8);
        let (generation_tx, _) = tokio::sync::mpsc::channel(4);
        let orch = CoreOrchestrator::new(
            &cfg,
            "test-session".to_string(),
            tx,
            action_tx,
            generation_tx,
            crate::orchestrator::SharedDaemonState::new_degraded(),
        )
        .expect("orchestrator creation should succeed");

        // Does not panic regardless of display availability.
        let _result = orch.capture_screen().await;
        // Any result (Some or None) is acceptable — the test verifies non-panicking.
    }

    // ── Phase 19: Hallucination Guard helper tests ────────────────────────────

    #[test]
    fn bridging_phrase_is_deterministic_for_same_trace_id() {
        // Same trace_id must always select the same bridging phrase.
        // Determinism is required: the phrase selection must not vary across
        // runs (no RNG), ensuring predictable behavior in logs and replays.
        let id = "abcdef12-0000-0000-0000-000000000000".to_string();
        let first = CoreOrchestrator::bridging_phrase(&id);
        let second = CoreOrchestrator::bridging_phrase(&id);
        assert_eq!(
            first, second,
            "same trace_id must always produce the same phrase"
        );
    }

    #[test]
    fn bridging_phrase_covers_all_four_slots() {
        // Verify all four phrases are reachable by cycling through first-byte values
        // 0..=3. This proves the modulo index doesn't skip any phrase slot.
        let phrases: Vec<&str> = (0u8..4)
            .map(|b| {
                // Construct a trace_id whose first byte has the desired value.
                // ASCII '0'=0x30 .. '3'=0x33.
                let id = format!("{}{}", (0x30u8 + b) as char, "-rest-of-uuid");
                CoreOrchestrator::bridging_phrase(&id)
            })
            .collect();
        // All four phrases must be distinct.
        let unique: std::collections::HashSet<&str> = phrases.iter().copied().collect();
        assert_eq!(
            unique.len(),
            4,
            "all four bridging phrase slots must be reachable"
        );
    }

    #[tokio::test]
    async fn push_tool_result_adds_message_without_truncation() {
        // push_tool_result must insert a message into the conversation context
        // without triggering turn-count truncation. Verify by pushing a message
        // and confirming the message count increased by exactly 1.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        let before = orch.context.messages().len();
        orch.context
            .push_tool_result("[Retrieved: test query]\nSome content.");
        let after = orch.context.messages().len();

        assert_eq!(
            after,
            before + 1,
            "push_tool_result must add exactly one message to the conversation context"
        );
        // The injected message must appear at the tail.
        let last = orch
            .context
            .messages()
            .last()
            .expect("messages must be non-empty");
        // Post Round 3 regression fix: tool results are injected as role="user"
        // (Ollama-universal role) with a content prefix for disambiguation.
        // Custom roles ("tool", "retrieval") were silently dropped by base-instruct
        // models, so this assertion now pins the universal-role contract.
        assert_eq!(
            last.role, "user",
            "injected tool result must use role=user (Ollama-universal)"
        );
        assert!(
            last.content.contains("Retrieved"),
            "injected content must be preserved verbatim"
        );
    }

    // ── Phase 21: Memory tests ────────────────────────────────────────────────

    #[test]
    fn prepare_messages_includes_recall_entries_as_memory_system_message() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let (action_tx, _) = tokio::sync::mpsc::channel(8);
        let (generation_tx, _) = tokio::sync::mpsc::channel(4);
        let orch = CoreOrchestrator::new(
            &cfg,
            "test-session".to_string(),
            tx,
            action_tx,
            generation_tx,
            crate::orchestrator::SharedDaemonState::new_degraded(),
        )
        .expect("orchestrator creation should succeed");

        let recall = vec![
            crate::retrieval::store::MemoryEntry {
                id: "id1".to_string(),
                content: "I'm building Project Dexter".to_string(),
                source: "operator".to_string(),
                entry_type: "fact".to_string(),
                session_id: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                similarity: 0.9,
            },
            crate::retrieval::store::MemoryEntry {
                id: "id2".to_string(),
                content: "I prefer Rust over Go".to_string(),
                source: "operator".to_string(),
                entry_type: "fact".to_string(),
                session_id: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                similarity: 0.75,
            },
        ];

        let messages = orch.prepare_messages_for_inference(&recall);
        // Phase 37.8: entries with session_id=None are treated as prior-session
        // (conservative — unknown provenance is framed as reference material).
        // Old "Memory:" label replaced with "Reference notes from prior sessions …".
        let memory_msg = messages
            .iter()
            .find(|m| m.content.starts_with("Reference notes from prior sessions"));
        assert!(
            memory_msg.is_some(),
            "A prior-session reference block must be injected"
        );
        let content = &memory_msg.unwrap().content;
        assert!(
            content.contains("I'm building Project Dexter"),
            "First recall entry must be present"
        );
        assert!(
            content.contains("I prefer Rust over Go"),
            "Second recall entry must be present"
        );
    }

    #[test]
    fn prepare_messages_skips_memory_injection_when_recall_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let (action_tx, _) = tokio::sync::mpsc::channel(8);
        let (generation_tx, _) = tokio::sync::mpsc::channel(4);
        let orch = CoreOrchestrator::new(
            &cfg,
            "test-session".to_string(),
            tx,
            action_tx,
            generation_tx,
            crate::orchestrator::SharedDaemonState::new_degraded(),
        )
        .expect("orchestrator creation should succeed");

        let messages = orch.prepare_messages_for_inference(&[]);
        // Phase 37.8: neither framing header should appear when recall is empty.
        let has_any_recall_block = messages.iter().any(|m| {
            m.content.starts_with("Reference notes from prior sessions")
                || m.content.starts_with("Earlier in this conversation:")
        });
        assert!(
            !has_any_recall_block,
            "No recall block when recall is empty"
        );
    }

    #[test]
    fn vision_image_attaches_to_user_message_skipping_trailing_tool_result() {
        // Phase 20 fix, updated for Round 3 role-contract change:
        // confirm the combined vision + retrieval-first path attaches the image
        // to the GENUINE user message even when a tool-result user-role message
        // follows it. Since tool results now ride on role="user" (see T0.1 in
        // the Round 3 diagnostic), the attachment logic must distinguish them
        // by content prefix via `is_tool_result_content`.
        use crate::inference::engine::Message;

        let fake_b64 = "dGVzdA==".to_string(); // "test" in base64

        let mut messages = vec![
            Message::system("personality"),
            Message::user("look at this"),
            Message::retrieval("[Retrieved: safari version] Safari 18.3"),
        ];

        // Round 3 attachment logic: skip user-role messages that are really
        // tool-result injections (identified by their content prefix).
        let target = messages
            .iter_mut()
            .rev()
            .find(|m| m.role == "user" && !is_tool_result_content(&m.content));
        if let Some(last_user) = target {
            last_user.images = Some(vec![fake_b64.clone()]);
        }

        // Assert: genuine user query (index 1) got the image.
        assert_eq!(messages[1].content, "look at this");
        assert_eq!(
            messages[1].images,
            Some(vec![fake_b64]),
            "Genuine user message must have the image"
        );

        // Assert: system message untouched.
        assert!(
            messages[0].images.is_none(),
            "System message must not have images"
        );

        // Assert: tool-result user-role message untouched and correctly classified.
        assert!(
            is_tool_result_content(&messages[2].content),
            "Retrieval injection must be classified as tool-result content"
        );
        assert!(
            messages[2].images.is_none(),
            "Tool-result message must never receive image attachment"
        );
    }

    #[test]
    fn screen_capture_path_prefix_generates_unique_per_call_paths() {
        // Verify the Phase 20 race condition fix: each call generates a unique path.
        use crate::constants::SCREEN_CAPTURE_PATH_PREFIX;
        let path1 = format!(
            "{SCREEN_CAPTURE_PATH_PREFIX}_{}.png",
            uuid::Uuid::new_v4().as_simple()
        );
        let path2 = format!(
            "{SCREEN_CAPTURE_PATH_PREFIX}_{}.png",
            uuid::Uuid::new_v4().as_simple()
        );

        assert_ne!(path1, path2, "Each invocation must produce a unique path");
        assert!(
            path1.starts_with("/tmp/dexter_screen_"),
            "Path must start with /tmp/dexter_screen_"
        );
        assert!(path1.ends_with(".png"), "Path must end with .png");
    }

    // ── Phase 24a tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn action_staleness_guard_discards_old() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx, _action_rx) = make_orchestrator_with_action_rx(tmp.path());

        // Drain initial events (IDLE etc.) that construction may have queued.
        tokio::task::yield_now().await;
        while rx.try_recv().is_ok() {}

        // Deliver a result for an unknown action_id — no interaction registered.
        let result = ActionResult {
            action_id: "unknown-id".to_string(),
            outcome: ActionOutcome::Completed {
                action_id: "unknown-id".to_string(),
                output: "hello".to_string(),
                rewritten_to: None,
            },
            trace_id: new_trace(),
        };
        let r = orch.handle_action_result(result).await;
        assert!(r.is_ok(), "Stale result should return Ok, not error");

        // No events emitted — result was silently discarded.
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "Stale result must not emit any events"
        );
    }

    #[tokio::test]
    async fn handle_action_result_transitions_to_feedback_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx, _action_rx) = make_orchestrator_with_action_rx(tmp.path());

        let action_id = Uuid::new_v4().to_string();

        // Insert a tracked interaction for this action_id.
        orch.interactions.insert(
            action_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: 0,
                original_content: String::new(),
                is_terminal_workflow: false,
            },
        );

        let result = ActionResult {
            action_id: action_id.clone(),
            outcome: ActionOutcome::Completed {
                action_id: action_id.clone(),
                output: "test output".to_string(),
                rewritten_to: None,
            },
            trace_id: new_trace(),
        };
        let r = orch.handle_action_result(result).await;
        assert!(r.is_ok());

        // Interaction should be marked Complete (handle_action_result transitions
        // through FeedbackPending → Complete synchronously when TTS is unavailable).
        let interaction = orch
            .interactions
            .get(&action_id)
            .expect("Interaction should still exist");
        assert!(
            matches!(interaction.stage, InteractionStage::Complete),
            "Stage should be Complete after handle_action_result"
        );
    }

    // ── Phase 38 / Codex finding [10]: in_flight_actions tracking ────────────

    #[tokio::test]
    async fn handle_action_result_removes_in_flight_action_handle() {
        // When an action result arrives, its tracked JoinHandle must be dropped
        // from the in_flight_actions map. Pre-Phase-38 the map didn't exist;
        // post-Phase-38 a non-removal would mean cancel paths try to abort
        // long-finished tasks (harmless but misleading) AND the map would
        // grow unboundedly across a session.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx, _action_rx) = make_orchestrator_with_action_rx(tmp.path());

        let action_id = Uuid::new_v4().to_string();
        // Simulate the dispatch site: register the interaction AND insert a
        // JoinHandle into the in-flight map. Using a never-completing future
        // so the handle is genuinely "in flight" until aborted/dropped.
        orch.interactions.insert(
            action_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: 0,
                original_content: String::new(),
                is_terminal_workflow: false,
            },
        );
        let dummy_handle = tokio::spawn(async {
            // Park forever so we know removal is what cleans up, not natural exit.
            std::future::pending::<()>().await;
        });
        orch.in_flight_actions
            .insert(action_id.clone(), dummy_handle);

        assert!(
            orch.in_flight_actions.contains_key(&action_id),
            "pre-condition: handle is registered"
        );

        let result = ActionResult {
            action_id: action_id.clone(),
            outcome: ActionOutcome::Completed {
                action_id: action_id.clone(),
                output: "ok".to_string(),
                rewritten_to: None,
            },
            trace_id: new_trace(),
        };
        let _ = orch.handle_action_result(result).await;

        assert!(
            !orch.in_flight_actions.contains_key(&action_id),
            "handle_action_result must remove the action's handle from in_flight_actions"
        );
    }

    #[tokio::test]
    async fn abort_active_generation_drains_in_flight_actions() {
        // Cancel paths must drop and abort every tracked action handle. With
        // Session 1's kill_on_drop(true), aborting → dropping the Tokio Child
        // → SIGKILL of the underlying subprocess — closing the loop on
        // "stop" actually stopping a running curl/yt-dlp/etc.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        // Insert two parked handles so we can verify drain (not just take-one).
        for _ in 0..2 {
            let id = Uuid::new_v4().to_string();
            let h = tokio::spawn(async { std::future::pending::<()>().await });
            orch.in_flight_actions.insert(id, h);
        }
        assert_eq!(orch.in_flight_actions.len(), 2);

        // No generation in flight, no producer slot — this exercises only
        // the in-flight-actions branch of the helper.
        let aborted = orch.abort_active_generation();

        assert!(
            aborted,
            "abort_active_generation must report it aborted at least one task"
        );
        assert!(
            orch.in_flight_actions.is_empty(),
            "drain must leave in_flight_actions empty"
        );
    }

    #[tokio::test]
    async fn abort_active_generation_with_nothing_in_flight_returns_false() {
        // Hotkey-as-attention-signal: when the operator taps the hotkey with
        // no generation/action in flight, the helper must be a no-op AND
        // report that nothing was aborted. The hotkey path uses this to gate
        // its log line and the maybe_rewarm_primary call.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        let aborted = orch.abort_active_generation();
        assert!(!aborted, "no-op cancel must report aborted=false");
    }

    #[tokio::test]
    async fn abort_active_generation_swaps_cancel_token() {
        // The cooperative cancel_token Arc must be replaced with a fresh one
        // after every abort, so the next generation starts uncancelled. This
        // pins the swap; without it, the second handle_text_input after a
        // cancel would short-circuit because cancel_token was still set true.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        let pre = orch.cancel_token.clone();
        let _ = orch.abort_active_generation();
        let post = orch.cancel_token.clone();

        assert!(
            !std::sync::Arc::ptr_eq(&pre, &post),
            "abort_active_generation must replace cancel_token with a fresh Arc"
        );
        assert!(
            !post.load(std::sync::atomic::Ordering::SeqCst),
            "fresh cancel_token must start as false"
        );
    }

    // ── Phase 38 / Codex finding [13]: TTS handle abort ──────────────────────

    #[tokio::test]
    async fn abort_active_generation_aborts_tracked_tts_handle() {
        // When TTS is mid-synthesis and a cancel fires, the TTS task must be
        // aborted — pre-Phase-38 it ran to completion, leaking late audio
        // frames to Swift after the LISTENING transition.
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        // Simulate make_tts_channel: spawn a parked task and publish its
        // abort handle into the slot.
        let tts_task = tokio::spawn(async { std::future::pending::<()>().await });
        *orch.tts_handle_abort.lock().unwrap() = Some(tts_task.abort_handle());

        // Verify pre-condition.
        assert!(
            orch.tts_handle_abort.lock().unwrap().is_some(),
            "pre-condition: TTS handle is registered"
        );

        let aborted = orch.abort_active_generation();

        assert!(
            aborted,
            "abort_active_generation must report it aborted at least one task"
        );
        assert!(
            orch.tts_handle_abort.lock().unwrap().is_none(),
            "TTS abort slot must be drained after abort_active_generation"
        );
        // Give Tokio a chance to process the abort.
        tokio::task::yield_now().await;
        assert!(
            tts_task.is_finished(),
            "the TTS task must be aborted (finished) after the cancel"
        );
    }

    #[tokio::test]
    async fn abort_active_generation_with_no_tts_handle_is_noop() {
        // Cancel fired with no TTS in flight (typed input mode, or the cancel
        // hits between TTS task completion and the next generation) must NOT
        // crash and must return aborted=false (so the hotkey path doesn't log
        // a misleading "aborting" message).
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        // No handle in the slot.
        assert!(orch.tts_handle_abort.lock().unwrap().is_none());

        let aborted = orch.abort_active_generation();
        assert!(
            !aborted,
            "no-op cancel with empty TTS slot must report aborted=false"
        );
    }

    /// Phase 32: after a Completed action result, a continuation generation must be
    /// spawned. This verifies: interaction marked Complete, entity set to THINKING,
    /// and the [Action result: ...] context was injected.
    ///
    /// We don't wait for gen_rx delivery — that requires Ollama to time out (5s+)
    /// which is outside unit-test scope. We test the synchronous effects: state
    /// transition and context injection, which happen before the spawn exits.
    #[tokio::test]
    async fn agentic_continuation_spawns_generation_on_action_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        let action_id = Uuid::new_v4().to_string();
        // depth=0: first action in the chain (direct user request).
        orch.interactions.insert(
            action_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: 0,
                original_content: "find and download the video".to_string(),
                is_terminal_workflow: false,
            },
        );

        let result = ActionResult {
            action_id: action_id.clone(),
            outcome: ActionOutcome::Completed {
                action_id: action_id.clone(),
                output: "page HTML: <a href=\"/video/123\">Big Daddy 4K</a>".to_string(),
                rewritten_to: None,
            },
            trace_id: new_trace(),
        };
        orch.handle_action_result(result).await.unwrap();
        tokio::task::yield_now().await;

        // 1. Interaction must be Complete — the old Interaction is consumed.
        let interaction = orch
            .interactions
            .get(&action_id)
            .expect("interaction must exist");
        assert!(
            matches!(interaction.stage, InteractionStage::Complete),
            "Interaction must be Complete after agentic continuation spawned"
        );

        // 2. EntityState::Thinking must have been sent (continuation signals operator
        //    that Dexter is working on the next step, not idle).
        let mut saw_thinking = false;
        while let Ok(evt) = rx.try_recv() {
            if let Ok(ServerEvent {
                event: Some(crate::ipc::proto::server_event::Event::EntityState(s)),
                ..
            }) = evt
            {
                if s.state == crate::ipc::proto::EntityState::Thinking as i32 {
                    saw_thinking = true;
                }
            }
        }
        assert!(
            saw_thinking,
            "Agentic continuation must emit EntityState::Thinking"
        );

        // 3. [Action result: ...] was injected into conversation context.
        // Role is "user" per the Round 3 fix — Ollama drops custom roles on
        // base-instruct models, so tool results must ride the user-role channel
        // with a `[Action result]` content-prefix for disambiguation.
        let messages = orch.context.messages();
        let injected = messages
            .iter()
            .any(|m| m.role == "user" && m.content.contains("Action result"));
        assert!(
            injected,
            "[Action result: ...] must be in context for the continuation model to see"
        );
    }

    /// Phase 32: when agentic_depth >= AGENTIC_MAX_DEPTH, the chain stops and the
    /// orchestrator speaks an explanatory message rather than looping forever.
    #[tokio::test]
    async fn agentic_chain_stops_at_max_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        let action_id = Uuid::new_v4().to_string();
        // Set depth at the limit.
        orch.interactions.insert(
            action_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: AGENTIC_MAX_DEPTH,
                original_content: "some request".to_string(),
                is_terminal_workflow: false,
            },
        );

        let result = ActionResult {
            action_id: action_id.clone(),
            outcome: ActionOutcome::Completed {
                action_id: action_id.clone(),
                output: "result".to_string(),
                rewritten_to: None,
            },
            trace_id: new_trace(),
        };
        orch.handle_action_result(result).await.unwrap();

        // A TextResponse containing the "couldn't finish" message must be sent.
        tokio::task::yield_now().await;
        let mut found_message = false;
        while let Ok(evt) = rx.try_recv() {
            if let Ok(ServerEvent {
                event: Some(crate::ipc::proto::server_event::Event::TextResponse(t)),
                ..
            }) = evt
            {
                if t.content.contains("steps") || t.content.contains("proceed") {
                    found_message = true;
                }
            }
        }
        assert!(
            found_message,
            "Max depth must emit an explanatory text response"
        );
    }

    /// Phase 36 / H3: when a Completed result arrives for an Interaction flagged
    /// `is_terminal_workflow`, the continuation must be SUPPRESSED. Without this
    /// guard, the agentic loop speculates a retry (re-emits the iMessage send
    /// action) and the operator sees a phantom second-send attempt.
    ///
    /// Assertions:
    ///  - No EntityState::Thinking event (no continuation kicked off).
    ///  - [Action result: ...] IS still injected (for conversation memory) —
    ///    we only skip the generation spawn, not the context update.
    #[tokio::test]
    async fn terminal_workflow_short_circuits_continuation_on_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        let action_id = Uuid::new_v4().to_string();
        orch.interactions.insert(
            action_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: 0,
                original_content: "text mom yes".to_string(),
                // The key flag — simulates is_terminal_send_action() == true on dispatch.
                is_terminal_workflow: true,
            },
        );

        let result = ActionResult {
            action_id: action_id.clone(),
            outcome: ActionOutcome::Completed {
                action_id: action_id.clone(),
                output: String::new(), // osascript `send` returns no stdout
                rewritten_to: None,
            },
            trace_id: new_trace(),
        };
        orch.handle_action_result(result).await.unwrap();
        tokio::task::yield_now().await;

        // 1. No THINKING event — continuation was suppressed.
        let mut saw_thinking = false;
        while let Ok(evt) = rx.try_recv() {
            if let Ok(ServerEvent {
                event: Some(crate::ipc::proto::server_event::Event::EntityState(s)),
                ..
            }) = evt
            {
                if s.state == crate::ipc::proto::EntityState::Thinking as i32 {
                    saw_thinking = true;
                }
            }
        }
        assert!(
            !saw_thinking,
            "Terminal-workflow Completed must NOT emit Thinking — continuation is suppressed"
        );

        // 2. The interaction itself is still marked Complete (stage transition happens
        //    before the short-circuit return).
        let interaction = orch
            .interactions
            .get(&action_id)
            .expect("interaction must still exist");
        assert!(
            matches!(interaction.stage, InteractionStage::Complete),
            "Interaction must be marked Complete after terminal short-circuit"
        );

        // 3. The [Action result] context injection still happened — the next
        //    natural-language turn should see the completion, not a phantom gap.
        let injected = orch
            .context
            .messages()
            .iter()
            .any(|m| m.role == "user" && m.content.contains("Action result"));
        assert!(
            injected,
            "[Action result] injection must happen even when continuation is short-circuited"
        );
    }

    /// Phase 36 / H3 regression guard: if the terminal action FAILED (Rejected),
    /// we must NOT short-circuit. Letting the continuation run gives the model a
    /// chance to diagnose the error ("couldn't find contact Mom, did you mean Tom?")
    /// instead of swallowing it as a silent "Sent."
    #[tokio::test]
    async fn terminal_workflow_does_not_short_circuit_on_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx, _gen_rx) = make_orchestrator_with_gen_rx(tmp.path());

        let action_id = Uuid::new_v4().to_string();
        orch.interactions.insert(
            action_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: 0,
                original_content: "text mom yes".to_string(),
                is_terminal_workflow: true,
            },
        );

        let result = ActionResult {
            action_id: action_id.clone(),
            outcome: ActionOutcome::Rejected {
                action_id: action_id.clone(),
                error: "Messages.app could not resolve buddy".to_string(),
            },
            trace_id: new_trace(),
        };
        orch.handle_action_result(result).await.unwrap();
        tokio::task::yield_now().await;

        // THINKING MUST appear — failures need the continuation so the model
        // can explain what went wrong and ask how to proceed.
        let mut saw_thinking = false;
        while let Ok(evt) = rx.try_recv() {
            if let Ok(ServerEvent {
                event: Some(crate::ipc::proto::server_event::Event::EntityState(s)),
                ..
            }) = evt
            {
                if s.state == crate::ipc::proto::EntityState::Thinking as i32 {
                    saw_thinking = true;
                }
            }
        }
        assert!(
            saw_thinking,
            "Rejected terminal action must still spawn a continuation so the model can diagnose"
        );
    }

    #[tokio::test]
    async fn interaction_gc_removes_old_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // Insert an interaction with created_at in the distant past.
        let old_id = "old-action".to_string();
        orch.interactions.insert(
            old_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now()
                    - std::time::Duration::from_secs(INTERACTION_TTL_SECS + 1),
                agentic_depth: 0,
                original_content: String::new(),
                is_terminal_workflow: false,
            },
        );

        // Insert a recent interaction that should survive.
        let new_id = "new-action".to_string();
        orch.interactions.insert(
            new_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: 0,
                original_content: String::new(),
                is_terminal_workflow: false,
            },
        );

        assert_eq!(orch.interactions.len(), 2);

        orch.gc_stale_interactions();

        assert_eq!(
            orch.interactions.len(),
            1,
            "GC should remove the old interaction"
        );
        assert!(
            orch.interactions.contains_key(&new_id),
            "Recent interaction should survive"
        );
        assert!(
            !orch.interactions.contains_key(&old_id),
            "Old interaction should be removed"
        );
    }

    #[tokio::test]
    async fn focused_state_on_action_dispatch() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, mut rx, mut action_rx) = make_orchestrator_with_action_rx(tmp.path());

        // Drain initial events.
        tokio::task::yield_now().await;
        while rx.try_recv().is_ok() {}

        // Manually simulate what handle_text_input does for SAFE action dispatch.
        let action_id = Uuid::new_v4().to_string();
        let spec = ActionSpec::Shell {
            args: vec!["echo".into(), "test".into()],
            working_dir: None,
            rationale: None,
            category_override: None,
        };
        let category = PolicyEngine::classify(&spec);
        let executor = orch.action_engine.executor_handle();
        let action_tx = orch.action_tx.clone();
        let aid = action_id.clone();
        let tid = new_trace();

        orch.interactions.insert(
            action_id.clone(),
            Interaction {
                trace_id: tid.clone(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: 0,
                original_content: String::new(),
                is_terminal_workflow: false,
            },
        );

        tokio::spawn(async move {
            let outcome = executor.execute(&aid, &spec, category, None).await;
            let _ = action_tx
                .send(ActionResult {
                    action_id: aid,
                    outcome,
                    trace_id: tid,
                })
                .await;
        });

        // Send FOCUSED state (as handle_text_input would).
        orch.send_state(EntityState::Focused, &new_trace())
            .await
            .unwrap();

        // Check that FOCUSED was emitted.
        tokio::task::yield_now().await;
        let mut found_focused = false;
        while let Ok(Ok(event)) = rx.try_recv() {
            if let Some(server_event::Event::EntityState(change)) = &event.event {
                if change.state == EntityState::Focused as i32 {
                    found_focused = true;
                }
            }
        }
        assert!(
            found_focused,
            "FOCUSED state should be emitted on action dispatch"
        );

        // Wait for the action result to arrive.
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), action_rx.recv()).await;
        assert!(
            result.is_ok(),
            "Action result should arrive within 5 seconds"
        );
        let result = result.unwrap().expect("Channel should not be closed");
        assert_eq!(result.action_id, action_id);
        assert!(matches!(result.outcome, ActionOutcome::Completed { .. }));
    }

    // ── Phase 24b tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn prefill_fires_on_hotkey() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // Before hotkey: last_prefill_at should be None.
        assert!(orch.last_prefill_at.is_none());

        // Simulate HotkeyActivated event.
        let sys_event = SystemEvent {
            r#type: SystemEventType::HotkeyActivated.into(),
            payload: String::new(),
        };
        let result = orch.handle_system_event(sys_event, new_trace()).await;
        assert!(result.is_ok());

        // After hotkey: prefill should have been invoked (last_prefill_at set).
        assert!(
            orch.last_prefill_at.is_some(),
            "prefill_inference_cache should have been called on HotkeyActivated"
        );
    }

    #[tokio::test]
    async fn prefill_fires_on_app_focused() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // Send an AppFocused event with a JSON payload that changes the context.
        // Payload must match AppFocusedPayload: { name, bundle_id, ax_element? }
        let sys_event = SystemEvent {
            r#type: SystemEventType::AppFocused.into(),
            payload: r#"{"name":"Safari","bundle_id":"com.apple.Safari"}"#.to_string(),
        };
        let result = orch.handle_system_event(sys_event, new_trace()).await;
        assert!(result.is_ok());

        // After app focus change: prefill should have been invoked.
        assert!(
            orch.last_prefill_at.is_some(),
            "prefill_inference_cache should have been called on AppFocused with context change"
        );
    }

    #[tokio::test]
    async fn prefill_debounced_on_context_change() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // First call sets last_prefill_at.
        orch.prefill_inference_cache();
        let first_prefill = orch
            .last_prefill_at
            .expect("should be set after first prefill");

        // Second call within debounce window should NOT update last_prefill_at.
        orch.prefill_inference_cache();
        let second_prefill = orch.last_prefill_at.expect("should still be set");

        assert_eq!(
            first_prefill, second_prefill,
            "Second prefill within 5s window should be debounced (same timestamp)"
        );
    }

    #[tokio::test]
    async fn hotkey_demotes_active_interactions_to_background() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut orch, _rx) = make_orchestrator(tmp.path());

        // Insert an Active interaction.
        let action_id = "test-action".to_string();
        orch.interactions.insert(
            action_id.clone(),
            Interaction {
                trace_id: new_trace(),
                stage: InteractionStage::ActionInFlight,
                priority: InteractionPriority::Active,
                created_at: Instant::now(),
                agentic_depth: 0,
                original_content: String::new(),
                is_terminal_workflow: false,
            },
        );

        // Simulate HotkeyActivated.
        let sys_event = SystemEvent {
            r#type: SystemEventType::HotkeyActivated.into(),
            payload: String::new(),
        };
        orch.handle_system_event(sys_event, new_trace())
            .await
            .unwrap();

        // The existing Active interaction should now be Background.
        let interaction = orch.interactions.get(&action_id).unwrap();
        assert_eq!(
            interaction.priority,
            InteractionPriority::Background,
            "HotkeyActivated should demote Active interactions to Background"
        );
    }

    #[test]
    fn generation_request_new_fields_default_to_none() {
        // Verify that the new Phase 24 fields on GenerationRequest have sensible defaults
        // and that existing construction patterns don't break.
        let req = GenerationRequest {
            model_name: "test".to_string(),
            messages: vec![],
            temperature: None,
            unload_after: false,
            keep_alive_override: None,
            num_predict: None,
            inactivity_timeout_override_secs: None,
            num_ctx_override: None,
        };
        assert!(req.keep_alive_override.is_none());
        assert!(req.num_predict.is_none());
        assert!(req.num_ctx_override.is_none());
    }

    #[test]
    fn generation_request_prefill_fields() {
        // Verify prefill-specific field values.
        let req = GenerationRequest {
            model_name: "qwen3:8b".to_string(),
            messages: vec![],
            temperature: None,
            unload_after: false,
            keep_alive_override: Some(FAST_MODEL_KEEP_ALIVE),
            num_predict: Some(1),
            inactivity_timeout_override_secs: None,
            num_ctx_override: None,
        };
        assert_eq!(req.keep_alive_override, Some("999m"));
        assert_eq!(req.num_predict, Some(1));
        assert!(!req.unload_after);
    }

    // ── Phase 24c: VadHint + classify_expected_response tests ────────────────

    /// Non-question sentences must never trigger a VadHint.
    #[test]
    fn classify_expected_response_returns_none_for_statements() {
        let statements = [
            "I've set a reminder for 3pm.",
            "The file has been saved.",
            "Okay, shutting down the server.",
            "Here is the summary you asked for.",
            "I can't find that file.",
        ];
        for s in statements {
            assert_eq!(
                CoreOrchestrator::classify_expected_response(s),
                None,
                "Expected None for: {s}"
            );
        }
    }

    /// Questions without yes/no patterns must not trigger a VadHint.
    /// We avoid flooding open-ended questions with a short endpoint — the operator
    /// might give a long answer to "What were you working on?" and cutting off at
    /// 256ms would be jarring.
    #[test]
    fn classify_expected_response_returns_none_for_open_ended_questions() {
        let open_ended = [
            "What would you like to work on today?",
            "Which file should I edit?",
            "How would you like me to handle that?",
            "When do you need this done?",
        ];
        for s in open_ended {
            assert_eq!(
                CoreOrchestrator::classify_expected_response(s),
                None,
                "Expected None for open-ended: {s}"
            );
        }
    }

    /// Yes/no questions must return Some(8) — the shortened silence threshold.
    #[test]
    fn classify_expected_response_returns_8_for_yes_no_questions() {
        let yes_no = [
            "Should I proceed with that?",
            "Do you want me to save the file?",
            "Would you like me to continue?",
            "Is that correct?",
            "Want me to run the tests?",
            "Shall I delete the old logs?",
            "Can I access your calendar?",
            "Is that right?",
            "Does that look right?",
        ];
        for s in yes_no {
            assert_eq!(
                CoreOrchestrator::classify_expected_response(s),
                Some(8),
                "Expected Some(8) for: {s}"
            );
        }
    }

    /// Sentences ending with '?' but containing no yes/no pattern must return None.
    /// Guards against over-eager classification (e.g. rhetorical questions, how/what/why).
    #[test]
    fn classify_expected_response_requires_both_question_mark_and_pattern() {
        // Has a yes/no pattern but no trailing '?'
        assert_eq!(
            CoreOrchestrator::classify_expected_response("should i do that, let me know"),
            None,
            "No trailing '?' — must return None even with matching pattern"
        );
        // Has a trailing '?' but no yes/no pattern
        assert_eq!(
            CoreOrchestrator::classify_expected_response("What files did you work on today?"),
            None,
            "No yes/no pattern — must return None even with trailing '?'"
        );
    }

    /// send_vad_hint_bg must send a VadHint ServerEvent with the correct frame count.
    #[tokio::test]
    async fn send_vad_hint_bg_sends_correct_frames() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<ServerEvent, Status>>(8);
        let sent = send_vad_hint_bg(&tx, 8, "trace-vad-1").await;
        assert!(
            sent,
            "send_vad_hint_bg must return true when channel is open"
        );
        let event = rx.recv().await.expect("Should receive a VadHint event");
        let event = event.expect("Event should be Ok");
        match event.event {
            Some(server_event::Event::VadHint(hint)) => {
                assert_eq!(hint.silence_frames, 8, "VadHint silence_frames must be 8");
            }
            other => panic!("Expected VadHint, got: {:?}", other),
        }
    }

    /// VadHint must NOT be sent for a statement (no trailing '?').
    /// This is the most common case — verify it produces no extraneous event.
    #[tokio::test]
    async fn send_vad_hint_not_emitted_for_statements() {
        // classify_expected_response returns None for statements.
        // In generate_primary, VadHint is only sent when classify returns Some.
        // This test verifies the pure function directly — the gate logic lives in
        // generate_primary which would require a live model to test end-to-end.
        assert_eq!(
            CoreOrchestrator::classify_expected_response("The task is complete."),
            None,
            "Statement must not produce a VadHint trigger"
        );
        assert_eq!(
            CoreOrchestrator::classify_expected_response("I've opened the file."),
            None,
            "Statement must not produce a VadHint trigger"
        );
    }

    // ── Phase 36: is_terminal_send_action (phantom-retry guard) ──────────────

    #[test]
    fn terminal_send_action_identifies_imessage_send() {
        // The canonical iMessage send AppleScript — must be flagged terminal.
        let spec = ActionSpec::AppleScript {
            script: r#"tell application "Messages"
                set targetService to 1st service whose service type = iMessage
                set targetBuddy to buddy "+15551234567" of targetService
                send "hi" to targetBuddy
            end tell"#
                .to_string(),
            rationale: None,
        };
        assert!(
            is_terminal_send_action(&spec),
            "iMessage send AppleScript must be flagged as terminal workflow"
        );
    }

    #[test]
    fn terminal_send_action_case_insensitive_match() {
        // Models sometimes vary casing — "Messages" vs "messages" vs "MESSAGES".
        // The function lowercases before substring check, so all variants must match.
        let spec = ActionSpec::AppleScript {
            script: r#"TELL APPLICATION "MESSAGES" SEND "y" TO BUDDY"#.to_string(),
            rationale: None,
        };
        assert!(
            is_terminal_send_action(&spec),
            "match must be case-insensitive"
        );
    }

    #[test]
    fn terminal_send_action_ignores_contacts_lookup() {
        // Contact resolution (no send) must NOT short-circuit — the continuation
        // model needs to see the result and decide who to send to.
        let spec = ActionSpec::AppleScript {
            script: r#"tell application "Contacts"
                set theName to name of person 1 whose name contains "Mom"
                return theName
            end tell"#
                .to_string(),
            rationale: None,
        };
        assert!(
            !is_terminal_send_action(&spec),
            "Contacts lookup (no send) must continue the agentic chain"
        );
    }

    #[test]
    fn terminal_send_action_ignores_messages_read() {
        // Reading recent messages without sending — must NOT be flagged.
        // Lacks the "send " substring with trailing space.
        let spec = ActionSpec::AppleScript {
            script: r#"tell application "Messages"
                get text of last 10 messages
            end tell"#
                .to_string(),
            rationale: None,
        };
        assert!(
            !is_terminal_send_action(&spec),
            "Reading Messages must not be flagged — no send verb present"
        );
    }

    #[test]
    fn terminal_send_action_ignores_non_applescript_actions() {
        let shell = ActionSpec::Shell {
            args: vec!["echo".to_string(), "hi".to_string()],
            working_dir: None,
            rationale: None,
            category_override: None,
        };
        assert!(
            !is_terminal_send_action(&shell),
            "Shell actions must never be flagged as terminal send"
        );

        let file_read = ActionSpec::FileRead {
            path: std::path::PathBuf::from("/tmp/x"),
        };
        assert!(
            !is_terminal_send_action(&file_read),
            "FileRead must never be flagged as terminal send"
        );
    }

    #[test]
    fn terminal_send_action_requires_send_verb_not_substring() {
        // The "send " trailing space rules out `resend`, `sendmessage`, etc.
        // A Messages tell block without a standalone "send " token must NOT match.
        let spec = ActionSpec::AppleScript {
            script: r#"tell application "Messages"
                set theResend to resend of last chat
            end tell"#
                .to_string(),
            rationale: None,
        };
        assert!(
            !is_terminal_send_action(&spec),
            "The `resend` substring must not trip the send-verb match"
        );
    }
}
