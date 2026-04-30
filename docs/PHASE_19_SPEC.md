# Phase 19 — Hallucination Guard: Live Retrieval Activation
## Spec version 1.1 — Session 019, 2026-03-14

> **Status:** NEXT PHASE.
> This document is the authoritative implementation guide for Phase 19.
> All architectural decisions are locked. Implement exactly as written.
>
> **Spec version history:**
> v1.0 — Initial spec
> v1.1 — Three pre-implementation fixes:
>   (1) `debug_assert!` replacing silent `let _ = other` in interceptor scan path
>   (2) `prepare_messages_for_inference()` helper required to fix re-prompt persona loss
>   (3) `RetrievalPipelineTrait` abstraction made mandatory prerequisite (was conditional in pitfalls)

---

## 1. What Phase 19 Delivers

The hallucination architecture has been structurally planned since Phase 0 and is the
central differentiator in the proposal: "When Dexter doesn't know something, he goes and
finds it." The `RetrievalPipeline`, `WebRetriever`, and `VectorStore` all exist from
Phase 9. What they don't have is an activation path — nothing in the orchestrator currently
calls them during inference. This phase wires that path end-to-end.

| Deliverable | What It Does |
|-------------|--------------|
| **Personality layer sentinel instructions** | System prompt gains an explicit instruction to emit `[UNCERTAIN: <query>]` when the model is genuinely uncertain about factual content. No speculation; use the sentinel. |
| **`UncertaintyInterceptor`** (new Rust module) | Scans the token stream for the sentinel marker. When found: suppresses it, returns the extracted query, and records pre-marker text to flush. Independently testable. |
| **Retrieval activation in orchestrator** | `handle_text_input` wraps its token loop with the interceptor. On sentinel detection: flush pre-marker text to UI, emit a bridging phrase, fire `retrieval_pipeline.retrieve()` as a background Tokio task, await result, inject into conversation as `tool_result`, re-prompt. |
| **Retrieval-first classifier** | Before routing to the model, pattern-match the input for query types that are always better answered from retrieval (current date/time, software versions, named people's current roles, recent news). These bypass model generation for the factual portion. |
| **Graceful retrieval failure** | If web retrieval times out or returns no usable result: orchestrator injects a failure context and instructs the model to state uncertainty explicitly. No confabulation path. |

**What this does NOT include:**
- Cross-session memory (conversation history embeddings, VectorStore persistence across
  sessions) — Phase 21. That's a distinct pipeline.
- Vision/OCR worker for apps without AX text — Phase 23.
- Retrieval-first for retrieval-from-local-VectorStore (episodic memory) — depends on
  Phase 21 populating the store first.

**Test count target:** 214 Rust passing (currently 204). 10 new tests.

---

## 2. What Already Exists (Do Not Rebuild)

| Component | Phase | Relevance to Phase 19 |
|-----------|-------|-----------------------|
| `RetrievalPipeline::retrieve(query)` | 9 | Primary activation target. Called from orchestrator on sentinel detection and retrieval-first queries. |
| `WebRetriever` + extraction pipeline | 9 | Used by `RetrievalPipeline` internally. No changes needed. |
| `VectorStore::search(query)` | 9 | Available for local retrieval. Phase 19 uses it only if local results exist (no cross-session history yet — Phase 21 will populate it). |
| `PersonalityLayer::build_system_prompt()` | 5 | Phase 19 adds the sentinel instruction block. |
| `ModelRouter::route(input)` | 5 | Phase 19 adds a `retrieval_first` check before routing. |
| `generate_stream()` token loop in `handle_text_input` | 16 | Phase 19 wraps this loop with `UncertaintyInterceptor`. |
| `SentenceSplitter` → TTS path | 13 | Bridging phrases and re-prompted responses flow through the existing TTS pipeline unchanged. |
| `ConversationContext::push_tool_result()` | 5 | Used to inject retrieval results into conversation before re-prompt. Verify this method exists; if not, Phase 19 adds it. |

---

## 3. Architecture

### 3.1 The sentinel format

The model is instructed to output the following structured marker when it is genuinely
uncertain about factual content:

```
[UNCERTAIN: <query>]
```

**Why this format over natural language detection:**
Natural language patterns ("I'm not sure about...", "I don't know...") are ambiguous —
the model uses them in contexts where retrieval is not appropriate (e.g., explaining a
design choice). A structured marker with a machine-readable format:
- Unambiguous: either present or not.
- Parameterized: the query to retrieve is embedded in the marker, not inferred from context.
- Unlikely to appear naturally in non-instructed output.
- Scannable in a streaming byte buffer with a simple Boyer-Moore window.

**Why `[UNCERTAIN: query]` not `<retrieve>` XML:**
XML-style tags can appear in code blocks and technical output. Square bracket markers
with ALL_CAPS sentinel words are visually distinctive and less likely to collide with
code samples, markdown, or HTML that the model might legitimately produce.

The `[UNCERTAIN:` prefix is 12 characters — long enough to be uniquely identifiable, short
enough to be fully buffered in the first 1-2 tokens of a typical model output.

**System prompt addition (personality/layer.rs):**

```
UNCERTAINTY PROTOCOL:
When you are genuinely uncertain about a specific factual claim — a current date, a
software version, a named person's current role, a recent event — output exactly:
[UNCERTAIN: <query>]
where <query> is a precise, self-contained retrieval query for the missing fact.
Do not guess. Do not interpolate from training data for facts that may have changed.
Use the marker once per uncertain fact, then stop generating until you receive the result.
This marker is intercepted automatically. The operator never sees it.
```

**When to use the sentinel (model instruction):**
- Current date or time
- Software version numbers
- Named individuals' current positions, roles, or status
- Events that may have occurred after training cutoff
- Prices, statistics, or quantities that change over time

**When NOT to use it (model instruction):**
- Conceptual explanations (even if complex)
- Code generation or debugging
- Architectural reasoning
- Operator's own codebase or local context

### 3.2 `UncertaintyInterceptor` — token stream scanner

```
Token stream from generate_stream()
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│  UncertaintyInterceptor                                      │
│                                                              │
│  State machine:                                              │
│    Passthrough  ─── sees "[UNCERTAIN:" ──► Capturing        │
│    Capturing    ─── sees "]"           ──► Intercepted       │
│    Capturing    ─── buffer overflows   ──► Passthrough       │
│    Intercepted  ─── single-use         ──► (reset to pass)  │
│                                                              │
│  Outputs:                                                    │
│    flush_text: Option<String>  ← text before the marker     │
│    query:      Option<String>  ← extracted retrieval query  │
└─────────────────────────────────────────────────────────────┘
    │
    ▼
Orchestrator decides:
  if query.is_some() → intercept path
  else               → forward flush_text to UI
```

The interceptor maintains a rolling buffer of recently seen tokens. It does not buffer
the entire stream — only a window large enough to hold the sentinel:
`[UNCERTAIN: <query>]` where `<query>` is capped at 200 characters. Maximum buffer size
is 220 characters. Beyond that, the window flushes (the model is generating freeform
text, not a sentinel).

**State transitions:**

```
State::Passthrough:
  Accumulate token in window.
  If window ends with "[UNCERTAIN:" (or is a prefix of it):
    → remain in Scanning, do not flush.
  If window does NOT contain an in-progress "[UNCERTAIN:" prefix:
    → flush window prefix to output (keep only the potential-prefix tail).

State::Capturing:
  Accumulate token.
  If "]" found:
    → extract everything between "[UNCERTAIN:" and "]" as query.
    → transition to Intercepted.
  If buffer length > 220:
    → flush entire buffer as passthrough text.
    → transition to Passthrough.

State::Intercepted:
  Return (pre_marker_text, Some(query)).
  Reset to Passthrough.
```

**Important:** The interceptor is **single-use per generation call**. If the model
outputs two `[UNCERTAIN:]` markers in one response (which the system prompt should
prevent but which is possible), only the first is intercepted; the second is flushed
as literal text. The orchestrator re-prompts after the first interception, so a second
marker in the re-prompted response restarts the cycle.

### 3.3 Orchestrator text input flow — with retrieval

The full updated flow for `handle_text_input`:

```
1. Receive TextInput { text, ... }
2. Retrieval-first check (§3.4):
   if is_retrieval_first_query(&text):
     → results = retrieval_pipeline.retrieve(&text).await
     → inject results as context (no model call for factual part)
     → route to model with results + question → stream response → done
3. Route to model (existing Phase 16 path)
4. Begin streaming tokens through UncertaintyInterceptor
5. For each token:
   a. Pass to interceptor → get (flush_text, Option<query>)
   b. If flush_text: send TextResponse chunks to UI / TTS pipeline (existing path)
   c. If query.is_some():
      → flush any remaining pre-marker text
      → emit bridging phrase (§3.5) as TextResponse to UI
      → fire retrieval_pipeline.retrieve(&query) as background Tokio task
      → stop current generation (drop the stream)
      → await retrieval result (with timeout)
      → if Ok(results): inject as tool_result, re-prompt → stream new response (goto 4)
      → if Err/timeout:  inject failure context, re-prompt → stream explicit uncertainty
6. Generation complete → existing SPEAKING/IDLE handling (Phase 18/19 path unchanged)
```

### 3.4 Retrieval-first classifier

Before routing to the model, `is_retrieval_first_query()` pattern-matches against a set
of query categories known to require fresh factual data:

```rust
pub fn is_retrieval_first_query(input: &str) -> bool {
    // Normalize for matching (lowercase, trim)
    let s = input.trim().to_lowercase();

    // Current date / time
    DATETIME_PATTERNS.iter().any(|p| s.contains(p))
    // Software version numbers
    || VERSION_PATTERNS.iter().any(|p| s.contains(p))
    // Named person's current role / status
    || PERSON_STATUS_PATTERNS.iter().any(|p| s.contains(p))
    // Recent news / current events
    || NEWS_PATTERNS.iter().any(|p| s.contains(p))
}
```

**Pattern sets (initial):**

```rust
const DATETIME_PATTERNS: &[&str] = &[
    "what time is it", "what's the time", "current time",
    "what date is it", "what's the date", "today's date",
    "what day is it", "what year is it",
];

const VERSION_PATTERNS: &[&str] = &[
    "latest version of", "current version of", "newest version of",
    "what version of", "which version of",
];

const PERSON_STATUS_PATTERNS: &[&str] = &[
    "who is the current", "who is the ceo", "who is the president",
    "who runs ", "who leads ", "who is cto", "who is cfo",
    "what is [a-z]+ doing now", // regex match handled separately
];

const NEWS_PATTERNS: &[&str] = &[
    "what happened with", "latest news on", "recent news about",
    "what's happening with", "any updates on",
];
```

**Important:** Retrieval-first does NOT replace the model entirely. The model is still
called to synthesize, explain, and contextualize the retrieved fact. Only the factual
lookup is preempted. The prompt becomes:

```
[Retrieved fact: <result>]

Original question: <input>

Answer using the retrieved fact above. Do not add speculation beyond it.
```

**Why rule-based, not a routing model:** A small routing model (Phi-3-mini style) is
architecturally planned (see §2.2.7 of IMPLEMENTATION_PLAN.md). But rule-based matching
covers the highest-frequency retrieval-first patterns with zero latency and is trivially
testable. The classifier is an abstracted function — `is_retrieval_first_query()` — so
a model-based classifier can replace it without touching the orchestrator call site.

### 3.5 Bridging phrases

When the uncertainty sentinel is intercepted, the orchestrator emits a short bridging
phrase before firing retrieval. This is deterministic (no additional inference call) and
chosen to match Dexter's voice profile:

```rust
const BRIDGING_PHRASES: &[&str] = &[
    "Let me check on that.",
    "One moment.",
    "Looking that up.",
    "Let me verify.",
];
```

Selection: `BRIDGING_PHRASES[trace_id.as_bytes()[0] as usize % BRIDGING_PHRASES.len()]`

Using the first byte of the `trace_id` (already a UUID) as a cheap pseudo-random
selector — reproducible for a given trace, varied across requests. No RNG dependency.

The bridging phrase is emitted as a `TextResponse` chunk and flows into the TTS pipeline
through the existing `SentenceSplitter`. All four variants end with a period — they are
valid sentence boundaries.

### 3.6 Retrieval result injection

After retrieval completes, the result is injected into `ConversationContext` as a
`tool_result` message before re-prompting:

```rust
// Successful retrieval
let injection = format!(
    "Retrieved result for query '{query}':\n\n{result}\n\n\
     Use this information to answer the original question. \
     Do not speculate beyond what is retrieved.",
    query  = query,
    result = result.text,
);
context.push_tool_result("retrieval", &injection);

// Failed retrieval
let injection = format!(
    "Retrieval for query '{query}' failed (reason: {reason}). \
     State your uncertainty about this fact explicitly. \
     Do not speculate or generate from training memory.",
    query  = query,
    reason = err,
);
context.push_tool_result("retrieval_failed", &injection);
```

The re-prompt uses the same routing path as the original query (same model, same context
window). The model receives: [system prompt] + [conversation history] + [original question]
+ [tool_result injection] and generates a grounded response.

### 3.7 Retrieval timeout and backpressure

Web retrieval can be slow. The orchestrator awaits the retrieval task with a `tokio::time::timeout`:

```rust
const RETRIEVAL_TIMEOUT_SECS: u64 = 10;

let result = tokio::time::timeout(
    Duration::from_secs(RETRIEVAL_TIMEOUT_SECS),
    retrieval_task,
).await;

match result {
    Ok(Ok(retrieved))  => { /* inject and re-prompt */ }
    Ok(Err(err))       => { /* inject failure context */ }
    Err(_timeout)      => { /* inject timeout context */ }
}
```

10 seconds is chosen as the point where user patience for a "one moment" response
is exhausted. The bridging phrase has already been sent; the operator is waiting.

---

## 4. Files Changed

| File | Change |
|------|--------|
| `src/rust-core/src/personality/layer.rs` | Add uncertainty protocol block to system prompt template |
| `src/rust-core/src/inference/interceptor.rs` (NEW) | `UncertaintyInterceptor` struct + state machine + unit tests |
| `src/rust-core/src/inference/mod.rs` | `pub mod interceptor;` export |
| `src/rust-core/src/inference/retrieval_classifier.rs` (NEW) | `is_retrieval_first_query()` + pattern constants + unit tests |
| `src/rust-core/src/inference/mod.rs` | `pub mod retrieval_classifier;` export |
| `src/rust-core/src/context/snapshot.rs` | Verify `app_context` / `focused_element` fields accessible to orchestrator. No changes expected. |
| `src/rust-core/src/retrieval/pipeline.rs` | Verify `retrieve(&str) -> Result<RetrievalResult>` signature. If `RetrievalResult` doesn't expose `.text`, add accessor. |
| `src/rust-core/src/session/context.rs` | Verify or add `push_tool_result(role: &str, content: &str)` to `ConversationContext`. |
| `src/rust-core/src/orchestrator.rs` | Wire interceptor + retrieval-first into `handle_text_input`; add `is_final: false` to any `AudioResponse` in regular TTS path that's missing it (Phase 19 cleanup) |
| `docs/SESSION_STATE.json` | Phase 19 complete, test counts updated to 214 |

---

## 5. New Tests (10 total)

### inference/interceptor.rs (4 tests — inline in the new file)

| Test | Validates |
|------|-----------|
| `interceptor_passes_through_clean_token_stream` | Stream with no sentinel → all text flushed as-is, query is None |
| `interceptor_detects_sentinel_mid_stream` | `"The answer is [UNCERTAIN: current year]."` → pre-marker text flushed, query = "current year" |
| `interceptor_handles_split_token_sentinel` | Sentinel split across multiple tokens (e.g. `"[UNCE"`, `"RTAIN: foo]"`) → correctly detected |
| `interceptor_ignores_overlong_capture_buffer` | `[UNCERTAIN: ` followed by 300 chars without `]` → flushed as passthrough, no query extracted |

### inference/retrieval_classifier.rs (3 tests — inline in the new file)

| Test | Validates |
|------|-----------|
| `classifier_detects_datetime_query` | "what time is it" → true; "what's the date today" → true |
| `classifier_detects_version_query` | "what is the latest version of rust" → true; "latest version of xcode" → true |
| `classifier_passes_non_retrieval_query` | "how does async rust work" → false; "explain the borrow checker" → false |

### orchestrator.rs (3 tests)

| Test | Validates |
|------|-----------|
| `handle_text_input_clean_response_no_retrieval` | Normal text input, no sentinel in mock response → TextResponse chunks forwarded, retrieval never called |
| `handle_text_input_sentinel_triggers_retrieval` | Mock inference returns `[UNCERTAIN: foo]` → retrieval called with "foo", result injected into context, second generate call made |
| `handle_text_input_retrieval_first_preempts_model` | Input matches datetime pattern → retrieval called before first model call; model receives injected result |

---

## 6. Implementation Guide

Implement in exactly this order. Run `cargo test` after each Rust step.

---

### Step 1: Verify and patch prerequisite APIs

Before building new code, verify these exist and have the right signatures.

**Required: `RetrievalPipelineTrait` — NOT optional.**

The three orchestrator unit tests in Step 5f require injecting a mock that controls what
`retrieve()` returns. If `RetrievalPipeline` is a concrete struct with no trait, the tests
must spawn real HTTP requests — that makes them `#[ignore]`-gated integration tests, not
unit tests. This phase requires unit-testable orchestrator behaviour.

Add a trait in `src/rust-core/src/retrieval/pipeline.rs`:

```rust
/// Abstraction over the retrieval pipeline for test injection.
///
/// `RetrievalPipeline` is the concrete production implementation.
/// `MockRetrievalPipeline` is the test implementation defined in `#[cfg(test)]`
/// blocks in `orchestrator.rs`.
#[async_trait::async_trait]
pub trait RetrievalPipelineTrait: Send + Sync {
    async fn retrieve(
        &self,
        query: &str,
    ) -> Result<RetrievalResult, Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait::async_trait]
impl RetrievalPipelineTrait for RetrievalPipeline {
    async fn retrieve(
        &self,
        query: &str,
    ) -> Result<RetrievalResult, Box<dyn std::error::Error + Send + Sync>> {
        // delegate to the existing concrete method
        self.retrieve_impl(query).await
    }
}
```

Change `CoreOrchestrator.retrieval_pipeline` from `RetrievalPipeline` to
`Box<dyn RetrievalPipelineTrait>` (or `Arc<dyn RetrievalPipelineTrait>`).
`make_orchestrator()` in tests provides a `MockRetrievalPipeline`.

If `async_trait` is not already a dependency, add it: `async-trait = "0.1"` in `Cargo.toml`.
If `async_trait` is already used elsewhere in the codebase, this is zero cost.

**`src/rust-core/src/session/context.rs`:**

Look for `ConversationContext`. It must have a method to inject a synthetic message that
appears as retrieved context to the model. If `push_tool_result` doesn't exist, add:

```rust
/// Inject a tool result into the conversation context.
///
/// Used by the retrieval pipeline to provide factual grounding before re-prompting.
/// The injection appears to the model as an assistant-side context fact, not a
/// user turn, so it doesn't disrupt conversational coherence.
pub fn push_tool_result(&mut self, role: &str, content: &str) {
    self.messages.push(ConversationMessage {
        role:    role.to_string(),
        content: content.to_string(),
    });
}
```

**`src/rust-core/src/retrieval/pipeline.rs`:**

Look for the `retrieve()` method and confirm:
- It accepts a `&str` query.
- It returns something with accessible text content.
- It is `async`.

If the return type is opaque, add:
```rust
pub struct RetrievalResult {
    pub query:  String,
    pub text:   String,    // primary extracted content (first usable result)
    pub source: String,    // URL or source identifier for logging
    pub confidence: f32,   // 0.0–1.0, used for failure-mode decisions
}
```

If `retrieve()` returns `Ok(results: Vec<RetrievalResult>)`, Phase 19 uses `results[0]`
(highest-ranked result) or falls back to the failure path if the vec is empty.

**After Step 1: `cargo test` — all 204 passing, 0 new failures (no logic changed).**

---

### Step 2: Personality layer — add uncertainty protocol

**File:** `src/rust-core/src/personality/layer.rs`

Find `build_system_prompt()` or the method that assembles the system prompt string.
Append the following block to the end of the system prompt, before any dynamic context
injection:

```rust
const UNCERTAINTY_PROTOCOL: &str = r#"
UNCERTAINTY PROTOCOL:
When you are genuinely uncertain about a specific factual claim — a current date,
a software version number, a named person's current role, a recent event — output
exactly this marker and nothing else on that topic:
[UNCERTAIN: <query>]
where <query> is a precise, self-contained web search query that would retrieve the
missing fact.

Use the marker for:
- Current or recent dates, times, and events
- Software version numbers (they change)
- Named individuals' current titles, positions, or status
- Any statistic or quantity that changes over time

Do NOT use the marker for:
- Conceptual explanations, even if complex
- Code generation or debugging
- Architectural or design reasoning
- Content about the operator's local machine or codebase

After emitting the marker, stop generating. The retrieval result will be injected
into this conversation and you will continue your response from it.

This marker is intercepted automatically — the operator never sees it.
"#;
```

Add `UNCERTAINTY_PROTOCOL` to the assembled system prompt string. Place it after the
personality directives but before any dynamic context (app name, focused element, etc.).

No new tests for this step — existing personality layer tests verify the system prompt
structure; adding a constant block does not break them.

---

### Step 3: `UncertaintyInterceptor` — new module

**File:** `src/rust-core/src/inference/interceptor.rs` (create new)

```rust
//! Uncertainty sentinel interception for the streaming token pipeline.
//!
//! The model is instructed to emit `[UNCERTAIN: <query>]` when it encounters genuine
//! factual uncertainty. This module scans the token stream, intercepts the marker,
//! and returns the extracted query and any pre-marker text that should be flushed.
//!
//! The interceptor is single-use per generation call — it intercepts the first marker
//! and then reverts to passthrough. The orchestrator is responsible for restarting
//! the generation loop after retrieval.

const SENTINEL_PREFIX: &str  = "[UNCERTAIN:";
const SENTINEL_CLOSE:  char  = ']';
const MAX_QUERY_LEN:   usize = 200;

/// State of the uncertainty scanner.
#[derive(Debug, PartialEq)]
enum State {
    /// Forwarding tokens to the caller unchanged.
    Passthrough,
    /// We saw `[UNCERTAIN:` — accumulating the query up to `]`.
    Capturing,
    /// A complete sentinel was intercepted. Returned once, then reset to Passthrough.
    Intercepted,
}

/// Wraps a streaming token source and intercepts `[UNCERTAIN: <query>]` markers.
///
/// Usage:
/// ```
/// let mut ic = UncertaintyInterceptor::new();
/// for token in model_stream {
///     match ic.process(&token) {
///         InterceptorOutput::Passthrough(text) => send_to_ui(text),
///         InterceptorOutput::Intercepted { flush, query } => {
///             if let Some(pre) = flush { send_to_ui(pre); }
///             handle_retrieval(query);
///         }
///     }
/// }
/// ```
pub struct UncertaintyInterceptor {
    state:  State,
    buffer: String,
}

/// Result of processing a single token.
#[derive(Debug)]
pub enum InterceptorOutput {
    /// Text to forward to the UI / TTS pipeline immediately.
    Passthrough(String),
    /// Sentinel detected. `flush` is any pre-marker text; `query` is the retrieval query.
    Intercepted { flush: Option<String>, query: String },
}

impl UncertaintyInterceptor {
    pub fn new() -> Self {
        Self {
            state:  State::Passthrough,
            buffer: String::with_capacity(64),
        }
    }

    /// Process one token from the model's output stream.
    ///
    /// Returns `InterceptorOutput::Passthrough` for normal text and
    /// `InterceptorOutput::Intercepted` when the sentinel is fully received.
    ///
    /// After an `Intercepted` result, the interceptor resets to `Passthrough`
    /// (single-use: the orchestrator restarts generation for the re-prompted response).
    pub fn process(&mut self, token: &str) -> InterceptorOutput {
        match self.state {
            State::Passthrough | State::Intercepted => {
                if self.state == State::Intercepted {
                    self.state = State::Passthrough;
                    self.buffer.clear();
                }
                self.buffer.push_str(token);
                self.scan_for_prefix()
            }
            State::Capturing => {
                self.buffer.push_str(token);
                self.scan_for_close()
            }
        }
    }

    // ── private helpers ────────────────────────────────────────────────────────

    fn scan_for_prefix(&mut self) -> InterceptorOutput {
        // Check if the buffer contains the full sentinel prefix.
        if let Some(idx) = self.buffer.find(SENTINEL_PREFIX) {
            // Everything before the sentinel is safe to flush.
            let pre_marker: String = self.buffer[..idx].to_string();
            // Keep everything from [UNCERTAIN: onwards in the buffer.
            let after = self.buffer[idx + SENTINEL_PREFIX.len()..].to_string();
            self.buffer = after;
            self.state  = State::Capturing;
            // Check if the close bracket is already in the carried-over text.
            let capturing_result = self.scan_for_close();
            return match capturing_result {
                InterceptorOutput::Intercepted { flush: _, query } => {
                    InterceptorOutput::Intercepted {
                        flush: if pre_marker.is_empty() { None } else { Some(pre_marker) },
                        query,
                    }
                }
                other => {
                    // scan_for_close() returned Passthrough("") — still accumulating
                    // the query, waiting for the closing ']'. State is already Capturing.
                    //
                    // In this code path `other` can ONLY ever be Passthrough("") because:
                    //   (a) Intercepted is handled by the first match arm above.
                    //   (b) The overflow guard in scan_for_close() returns Passthrough(flush)
                    //       only when self.buffer.len() > MAX_QUERY_LEN. We just assigned
                    //       self.buffer = after (content after "[UNCERTAIN:"), which can only
                    //       exceed MAX_QUERY_LEN if the token itself is >200 chars — not
                    //       possible in any realistic model output.
                    //
                    // The Passthrough("") is intentionally not returned — we return the
                    // pre-marker text instead. Capturing state is preserved for the next token.
                    debug_assert!(
                        matches!(&other, InterceptorOutput::Passthrough(s) if s.is_empty()),
                        "scan_for_close in prefix-found branch must return Passthrough(\"\"), \
                         got {:?}", other
                    );
                    if !pre_marker.is_empty() {
                        InterceptorOutput::Passthrough(pre_marker)
                    } else {
                        InterceptorOutput::Passthrough(String::new())
                    }
                }
            };
        }

        // Check for a potential in-progress prefix at the end of the buffer.
        // (e.g., buffer ends with "[UNCE" — might be start of "[UNCERTAIN:")
        let prefix_tail = Self::longest_prefix_tail(&self.buffer, SENTINEL_PREFIX);
        if prefix_tail > 0 {
            // Flush everything before the potential prefix start.
            let safe_end = self.buffer.len() - prefix_tail;
            let flush: String = self.buffer[..safe_end].to_string();
            self.buffer = self.buffer[safe_end..].to_string();
            InterceptorOutput::Passthrough(flush)
        } else {
            // No sentinel in sight — flush the whole buffer.
            let flush = self.buffer.clone();
            self.buffer.clear();
            InterceptorOutput::Passthrough(flush)
        }
    }

    fn scan_for_close(&mut self) -> InterceptorOutput {
        if let Some(close_pos) = self.buffer.find(SENTINEL_CLOSE) {
            let query: String = self.buffer[..close_pos].trim().to_string();
            // Drop the captured buffer (including the close bracket).
            self.buffer = self.buffer[close_pos + 1..].to_string();
            self.state  = State::Intercepted;
            return InterceptorOutput::Intercepted { flush: None, query };
        }

        // Buffer overflow guard — if we've accumulated more than MAX_QUERY_LEN
        // characters without seeing a close bracket, this is not a real sentinel.
        if self.buffer.len() > MAX_QUERY_LEN {
            let flush = format!("{}{}", SENTINEL_PREFIX, self.buffer.clone());
            self.buffer.clear();
            self.state = State::Passthrough;
            return InterceptorOutput::Passthrough(flush);
        }

        // Still waiting for the close bracket.
        InterceptorOutput::Passthrough(String::new())
    }

    /// Returns the length of the longest suffix of `haystack` that is a prefix of `needle`.
    /// Used to detect in-progress sentinel matches at the end of the buffer.
    fn longest_prefix_tail(haystack: &str, needle: &str) -> usize {
        let hb = haystack.as_bytes();
        let nb = needle.as_bytes();
        let max_check = nb.len().min(hb.len());
        for len in (1..=max_check).rev() {
            if hb[hb.len() - len..] == nb[..len] {
                return len;
            }
        }
        0
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn run_stream(tokens: &[&str]) -> (Vec<String>, Option<String>) {
        let mut ic     = UncertaintyInterceptor::new();
        let mut output = Vec::new();
        let mut query  = None;
        for token in tokens {
            match ic.process(token) {
                InterceptorOutput::Passthrough(t) => {
                    if !t.is_empty() { output.push(t); }
                }
                InterceptorOutput::Intercepted { flush, query: q } => {
                    if let Some(f) = flush { if !f.is_empty() { output.push(f); } }
                    query = Some(q);
                }
            }
        }
        (output, query)
    }

    #[test]
    fn interceptor_passes_through_clean_token_stream() {
        let tokens = &["The ", "answer ", "is ", "42."];
        let (flushed, query) = run_stream(tokens);
        assert_eq!(flushed.join(""), "The answer is 42.");
        assert!(query.is_none());
    }

    #[test]
    fn interceptor_detects_sentinel_mid_stream() {
        let tokens = &["The answer is ", "[UNCERTAIN: current year]", " and nothing more."];
        let (flushed, query) = run_stream(tokens);
        assert_eq!(query.as_deref(), Some("current year"));
        // Pre-marker text must be flushed.
        assert!(flushed.iter().any(|t| t.contains("The answer is")));
    }

    #[test]
    fn interceptor_handles_split_token_sentinel() {
        // Sentinel split across three tokens — common in practice.
        let tokens = &["The ", "[UNCE", "RTAIN: latest rust version", "]", " done."];
        let (_, query) = run_stream(tokens);
        assert_eq!(query.as_deref(), Some("latest rust version"));
    }

    #[test]
    fn interceptor_ignores_overlong_capture_buffer() {
        let long_content: String = "x".repeat(250);
        let token = format!("[UNCERTAIN: {}", long_content); // no closing ]
        let tokens: Vec<&str> = vec![&token];
        let (flushed, query) = run_stream(&tokens);
        assert!(query.is_none(), "no close bracket → should not intercept");
        // The buffer overflow should result in the text being flushed as passthrough.
        assert!(!flushed.is_empty(), "overlong buffer should flush as passthrough");
    }
}
```

Add `pub mod interceptor;` to `src/rust-core/src/inference/mod.rs`.

**After Step 3: `cargo test` → 208 passing, 0 warnings (4 new interceptor tests).**

---

### Step 4: Retrieval-first classifier — new module

**File:** `src/rust-core/src/inference/retrieval_classifier.rs` (create new)

```rust
//! Retrieval-first query classifier.
//!
//! Identifies query types that are always better answered from fresh retrieval
//! than from model memory. For these queries, the orchestrator fires retrieval
//! before the first model call and injects the result as context.
//!
//! Rule-based for now — see §3.4 of PHASE_19_SPEC.md for the rationale.
//! The function signature is the abstraction boundary; a model-based classifier
//! can replace the body without changing any call site.

/// Returns true if the query should be resolved by retrieval before model inference.
///
/// Called by the orchestrator at the start of `handle_text_input`, before routing.
/// A `true` result means: run `RetrievalPipeline::retrieve(input)` first and inject
/// the result into context before the first `generate_stream` call.
pub fn is_retrieval_first_query(input: &str) -> bool {
    let s = input.trim().to_lowercase();
    DATETIME_PATTERNS.iter().any(|p| s.contains(p))
        || VERSION_PATTERNS.iter().any(|p| s.contains(p))
        || PERSON_STATUS_PATTERNS.iter().any(|p| s.contains(p))
        || NEWS_PATTERNS.iter().any(|p| s.contains(p))
}

const DATETIME_PATTERNS: &[&str] = &[
    "what time is it",
    "what's the time",
    "current time",
    "what date is it",
    "what's the date",
    "today's date",
    "what day is it",
    "what year is it",
    "what month is it",
];

const VERSION_PATTERNS: &[&str] = &[
    "latest version of",
    "current version of",
    "newest version of",
    "what version of",
    "which version of",
    "most recent version of",
];

const PERSON_STATUS_PATTERNS: &[&str] = &[
    "who is the current",
    "who is the ceo",
    "who is the president",
    "who is the prime minister",
    "who runs ",
    "who leads ",
    "who is cto",
    "who is cfo",
    "who is the head of",
];

const NEWS_PATTERNS: &[&str] = &[
    "what happened with",
    "latest news on",
    "latest news about",
    "recent news about",
    "what's happening with",
    "any updates on",
    "what's new with",
];

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_detects_datetime_query() {
        assert!(is_retrieval_first_query("what time is it?"));
        assert!(is_retrieval_first_query("What's the date today?"));
        assert!(is_retrieval_first_query("What year is it currently?"));
    }

    #[test]
    fn classifier_detects_version_query() {
        assert!(is_retrieval_first_query("what is the latest version of rust?"));
        assert!(is_retrieval_first_query("latest version of xcode"));
        assert!(is_retrieval_first_query("Which version of swift is current?"));
    }

    #[test]
    fn classifier_passes_non_retrieval_query() {
        assert!(!is_retrieval_first_query("how does async rust work?"));
        assert!(!is_retrieval_first_query("explain the borrow checker"));
        assert!(!is_retrieval_first_query("write me a function that sorts a vec"));
        assert!(!is_retrieval_first_query("what is polymorphism"));
    }
}
```

Add `pub mod retrieval_classifier;` to `src/rust-core/src/inference/mod.rs`.

**After Step 4: `cargo test` → 211 passing, 0 warnings (3 new classifier tests).**

---

### Step 5: Orchestrator — wire retrieval-first + interceptor

**File:** `src/rust-core/src/orchestrator.rs`

This is the most involved step. The changes are localized to `handle_text_input` (or
its equivalent — the method that processes `TextInput` events).

#### 5a. Extract `prepare_messages_for_inference()` helper (REQUIRED before re-prompt)

**This step must be done before the re-prompt call is written.** The Phase 16 context
snapshot injection runs inline in `handle_text_input` — it builds or mutates a message
list with the personality system prompt (index 0) and the current context snapshot
(index 1). The re-prompt call in §5e must receive the same treatment.

Calling `generate_stream(&self.context, &self.personality)` directly for the re-prompt
passes raw `ConversationContext.messages` without the Phase 16 context snapshot injection.
The model receives a message list that lacks the current machine context and — depending
on how `generate_stream` applies personality — may also lack the uncertainty protocol
instructions. The re-prompted response would be out-of-persona and unaware of what the
operator is doing.

Extract the message-assembly pipeline into a private helper:

```rust
/// Build the complete message list for a `generate_stream` call.
///
/// Applies (in order):
///   1. Personality system prompt — `PersonalityLayer::apply_to_messages()` inserts
///      or merges the system prompt at index 0.
///   2. Current context snapshot — Phase 16 injection, inserted at index 1
///      (after the system message, before conversation history).
///   3. Conversation history — remaining messages from `self.context`.
///
/// Both the original generation call and the re-prompt after retrieval MUST use
/// this helper. Calling `generate_stream` with `&self.context` directly skips
/// steps 1 and 2, producing an out-of-persona response without the uncertainty
/// protocol instructions.
fn prepare_messages_for_inference(&self) -> Vec<Message> {
    // Clone so the original context is not mutated.
    let mut messages = self.context.messages.clone();

    // Step 1: apply personality (inserts system prompt at index 0 or merges
    // with an existing system message).
    self.personality.apply_to_messages(&mut messages);

    // Step 2: inject Phase 16 context snapshot at index 1 (after system prompt).
    if let Some(summary) = self.context_observer.snapshot().context_summary() {
        let insert_pos = if messages.first()
            .map(|m| m.role.as_str() == "system")
            .unwrap_or(false)
        {
            1
        } else {
            0
        };
        messages.insert(insert_pos, Message {
            role:    "system".to_string(),
            content: summary,
        });
    }

    messages
}
```

**Replace the current inline Phase 16 injection in `handle_text_input`** with a call to
`self.prepare_messages_for_inference()` and pass the returned `Vec<Message>` to
`generate_stream`. This is a refactor of the existing original-generation call, not an
additional invocation.

#### 5c. Add imports at top of file

```rust
use crate::inference::interceptor::{UncertaintyInterceptor, InterceptorOutput};
use crate::inference::retrieval_classifier::is_retrieval_first_query;

const BRIDGING_PHRASES: &[&str] = &[
    "Let me check on that.",
    "One moment.",
    "Looking that up.",
    "Let me verify.",
];

const RETRIEVAL_TIMEOUT_SECS: u64 = 10;
```

#### 5d. Add helper: `bridging_phrase`

```rust
/// Select a bridging phrase from the static set.
///
/// Uses the first byte of the trace_id as a cheap pseudo-random index — varied
/// across requests, reproducible for a given trace, requires no RNG dependency.
fn bridging_phrase(trace_id: &str) -> &'static str {
    let idx = trace_id.as_bytes().first().copied().unwrap_or(0) as usize;
    BRIDGING_PHRASES[idx % BRIDGING_PHRASES.len()]
}
```

#### 5e. Add helper: `send_text`

If the orchestrator doesn't already have a method to send a `TextResponse` string chunk,
add one:

```rust
async fn send_text(&self, text: &str, trace_id: &str) -> Result<(), OrchestratorError> {
    use crate::ipc::proto::{server_event, TextResponse};
    let event = ServerEvent {
        trace_id: trace_id.to_string(),
        event: Some(server_event::Event::TextResponse(TextResponse {
            chunk: text.to_string(),
            is_final: false,
        })),
    };
    self.tx.send(Ok(event)).await
        .map_err(|_| OrchestratorError::ChannelClosed)
}
```

#### 5f. Modify `handle_text_input` — retrieval-first check

At the top of `handle_text_input`, before routing:

```rust
// Phase 19: retrieval-first for queries that are always stale in model memory.
if is_retrieval_first_query(&input.text) {
    info!(
        session  = %self.session_id,
        trace_id = %trace_id,
        input    = %input.text,
        "Retrieval-first query detected — fetching before model call"
    );
    self.send_state(EntityState::Thinking, &trace_id).await?;

    let retrieval_result = tokio::time::timeout(
        Duration::from_secs(RETRIEVAL_TIMEOUT_SECS),
        self.retrieval_pipeline.retrieve(&input.text),
    ).await;

    let injected_context = match retrieval_result {
        Ok(Ok(result)) => {
            info!(
                session = %self.session_id,
                source  = %result.source,
                "Retrieval-first: result obtained"
            );
            format!(
                "Retrieved fact for query '{query}':\n{text}\n(Source: {source})\n\n\
                 Use the retrieved fact above to answer the question. \
                 Do not speculate beyond it.",
                query  = input.text,
                text   = result.text,
                source = result.source,
            )
        }
        Ok(Err(err)) => {
            warn!(session = %self.session_id, error = %err, "Retrieval-first: retrieval error");
            format!(
                "Retrieval for '{}' failed: {}. \
                 State that you cannot confirm this fact and why.",
                input.text, err
            )
        }
        Err(_timeout) => {
            warn!(session = %self.session_id, "Retrieval-first: timeout");
            format!(
                "Retrieval for '{}' timed out. \
                 State that you cannot confirm this fact right now.",
                input.text
            )
        }
    };

    // Inject retrieval result into context and proceed to model call below.
    self.context.push_tool_result("retrieval", &injected_context);
    // Fall through to normal routing — model synthesizes from injected context.
}
```

#### 5g. Modify the token streaming loop — add interceptor

Find the section in `handle_text_input` where tokens from `generate_stream` are forwarded
to the UI. Wrap it with the interceptor:

```rust
// Phase 19: wrap token stream with uncertainty interceptor.
let mut interceptor   = UncertaintyInterceptor::new();
let mut intercepted_q: Option<String> = None;

'token_loop: while let Some(token_result) = stream.next().await {
    match token_result {
        Ok(token) => {
            match interceptor.process(&token) {
                InterceptorOutput::Passthrough(text) => {
                    if !text.is_empty() {
                        self.send_text(&text, &trace_id).await?;
                        // existing TTS sentence-splitter feeding path (unchanged)
                        sentence_splitter.push(&text);
                        self.flush_tts_sentences(&mut sentence_splitter, &trace_id).await?;
                    }
                }
                InterceptorOutput::Intercepted { flush, query } => {
                    // Flush any pre-marker text.
                    if let Some(pre) = flush {
                        if !pre.is_empty() {
                            self.send_text(&pre, &trace_id).await?;
                            sentence_splitter.push(&pre);
                            self.flush_tts_sentences(&mut sentence_splitter, &trace_id).await?;
                        }
                    }
                    intercepted_q = Some(query);
                    break 'token_loop;   // stop current generation
                }
            }
        }
        Err(e) => {
            error!(session = %self.session_id, error = %e, "Token stream error");
            break 'token_loop;
        }
    }
}

// Phase 19: handle intercepted retrieval query.
if let Some(query) = intercepted_q {
    info!(
        session  = %self.session_id,
        trace_id = %trace_id,
        query    = %query,
        "Uncertainty sentinel intercepted — firing retrieval"
    );

    // Send bridging phrase to operator.
    let bridge = bridging_phrase(&trace_id);
    self.send_text(bridge, &trace_id).await?;
    sentence_splitter.push(bridge);
    self.flush_tts_sentences(&mut sentence_splitter, &trace_id).await?;

    // Fire retrieval (with timeout).
    let retrieval_result = tokio::time::timeout(
        Duration::from_secs(RETRIEVAL_TIMEOUT_SECS),
        self.retrieval_pipeline.retrieve(&query),
    ).await;

    let injection = match retrieval_result {
        Ok(Ok(result)) => {
            info!(
                session = %self.session_id,
                source  = %result.source,
                "Retrieval complete — injecting and re-prompting"
            );
            format!(
                "Retrieved result for query '{query}':\n{text}\n(Source: {source})\n\n\
                 Continue your response using this retrieved information. \
                 Do not speculate beyond it.",
                query  = query,
                text   = result.text,
                source = result.source,
            )
        }
        Ok(Err(err)) => {
            warn!(session = %self.session_id, error = %err, "Retrieval error after sentinel");
            format!(
                "Retrieval for '{}' failed: {}. \
                 State your uncertainty about this fact explicitly. \
                 Do not generate from memory.",
                query, err
            )
        }
        Err(_timeout) => {
            warn!(session = %self.session_id, "Retrieval timeout after sentinel");
            format!(
                "Retrieval for '{}' timed out. \
                 Acknowledge that you cannot confirm this fact right now.",
                query
            )
        }
    };

    // Inject and re-prompt. The new stream runs through the same loop above.
    // Since `interceptor` is already in Passthrough state (single-use), the
    // re-prompted response streams normally.
    self.context.push_tool_result("retrieval", &injection);
    let re_prompt_route = self.router.route(&self.context).await?;

    // IMPORTANT: use prepare_messages_for_inference(), NOT &self.context directly.
    // The re-prompt must go through the same personality + context snapshot assembly
    // as the original generation call (Step 5a). Passing &self.context skips Phase 16
    // context injection and the personality system prompt — the model loses its persona
    // and uncertainty protocol instructions on every retrieval response.
    let re_prompt_messages = self.prepare_messages_for_inference();
    let mut re_stream = self.inference
        .generate_stream(re_prompt_route.model, &re_prompt_messages, &self.personality)
        .await?;

    while let Some(token_result) = re_stream.next().await {
        match token_result {
            Ok(token) => {
                if !token.is_empty() {
                    self.send_text(&token, &trace_id).await?;
                    sentence_splitter.push(&token);
                    self.flush_tts_sentences(&mut sentence_splitter, &trace_id).await?;
                }
            }
            Err(e) => {
                error!(session = %self.session_id, error = %e, "Re-prompt stream error");
                break;
            }
        }
    }
}
// ... existing TTS finalization, is_final sentinel, IDLE handling (unchanged) ...
```

**Note:** The existing TTS finalization — `SentenceSplitter::flush()`, `is_final` sentinel
(from Phase 18/19), and IDLE transition — runs after the token loop regardless of whether
retrieval was triggered. No changes needed there.

#### 5h. Add 3 orchestrator unit tests

These require a mock `RetrievalPipeline`. Add a `MockRetrievalPipeline` in the test module
(similar to existing mock components) that returns a fixed `RetrievalResult`:

```rust
#[tokio::test]
async fn handle_text_input_clean_response_no_retrieval() {
    // Mock inference returns a clean response with no sentinel.
    // Verify: TextResponse chunks forwarded, retrieval never called.
    let (mut orch, mut rx) = make_orchestrator_with_mock_inference(
        "The borrow checker enforces ownership rules at compile time.",
    );
    let evt = text_input_event("explain the borrow checker");
    orch.handle_text_input(evt, new_trace()).await.unwrap();

    let text_chunks: Vec<_> = collect_text_responses(&mut rx);
    assert!(!text_chunks.is_empty());
    assert!(orch.retrieval_call_count() == 0);
}

#[tokio::test]
async fn handle_text_input_sentinel_triggers_retrieval() {
    // Mock inference returns sentinel; mock retrieval returns a result.
    // Verify: bridging phrase sent, retrieval called with correct query,
    // re-prompted response sent.
    let (mut orch, mut rx) = make_orchestrator_with_mock_inference(
        "The current version is [UNCERTAIN: latest stable rust version].",
    );
    let evt = text_input_event("what is the current version of rust?");
    orch.handle_text_input(evt, new_trace()).await.unwrap();

    let text_chunks = collect_text_responses(&mut rx);
    // Bridging phrase must appear.
    let all_text = text_chunks.join("");
    assert!(
        BRIDGING_PHRASES.iter().any(|p| all_text.contains(p)),
        "bridging phrase must be sent before re-prompt"
    );
    // Retrieval must have been called with the extracted query.
    assert!(orch.last_retrieval_query().as_deref()
        == Some("latest stable rust version"));
}

#[tokio::test]
async fn handle_text_input_retrieval_first_preempts_model() {
    // Retrieval-first query: retrieval fires before first model call.
    // Verify: retrieval called with original query, model call count = 1 (not 0).
    let (mut orch, mut rx) = make_orchestrator_with_mock_inference(
        "The current time is as retrieved.",
    );
    let evt = text_input_event("what time is it?");
    orch.handle_text_input(evt, new_trace()).await.unwrap();

    // Retrieval must have been called before the model.
    assert!(orch.retrieval_call_count() >= 1);
    assert!(orch.retrieval_was_called_before_model());
    let _ = rx;
}
```

**After Step 5: `cargo test` → 214 passing, 0 warnings (3 new orchestrator tests).**

---

### Step 6: Full regression

```bash
cargo test            # 214 passing, 0 failed, 0 warnings
cd src/swift && swift build  # 0 project-code warnings (no Swift changes this phase)
uv run pytest -q      # 19 passed
```

**Manual validation (requires `make run`):**

1. Ask "what time is it?" → entity goes THINKING, Dexter retrieves and responds with
   current time (not a hallucinated time from training data) ✓
2. Ask "explain how async rust works" → normal response, no retrieval triggered ✓
3. Ask something the model would be uncertain about (e.g., "what is the latest version
   of macOS?") → entity goes THINKING, bridging phrase spoken ("Let me check on that."
   or variant), then grounded answer ✓
4. If web retrieval fails (turn off network): Dexter says "I can't confirm this right now"
   rather than hallucinating ✓
5. All previous hotkey / proactive behavior unchanged ✓

---

## 7. Acceptance Checklist

- [ ] AC-1  `UNCERTAINTY_PROTOCOL` added to system prompt; model instructed on when/how to use `[UNCERTAIN: <query>]`
- [ ] AC-2  `UncertaintyInterceptor` new module in `src/rust-core/src/inference/interceptor.rs`
- [ ] AC-3  Interceptor correctly detects `[UNCERTAIN: query]` split across multiple tokens
- [ ] AC-4  Interceptor flushes pre-marker text before returning `Intercepted`
- [ ] AC-5  Interceptor buffer-overflow guard prevents false intercepts on overlong non-sentinel text
- [ ] AC-6  `is_retrieval_first_query()` returns true for datetime / version / person-status patterns
- [ ] AC-7  `is_retrieval_first_query()` returns false for conceptual / code queries
- [ ] AC-8  Retrieval-first path fires retrieval before first model call
- [ ] AC-9  Retrieval-first result injected into context via `push_tool_result` before routing
- [ ] AC-10 Sentinel interception path: stops current generation on sentinel
- [ ] AC-11 Bridging phrase emitted to UI before retrieval fires
- [ ] AC-12 Bridging phrase flows into TTS pipeline (sentence splitter picks it up)
- [ ] AC-13 Retrieval query extracted correctly from `[UNCERTAIN: query]` marker
- [ ] AC-14 Successful retrieval result injected into context, model re-prompted
- [ ] AC-15 Failed retrieval (error): failure context injected, model instructed to state uncertainty
- [ ] AC-16 Retrieval timeout (10s): timeout context injected, model instructed to state uncertainty
- [ ] AC-17 Re-prompted response streams normally through existing TTS + IDLE path
- [ ] AC-17a Re-prompted response uses `prepare_messages_for_inference()` — personality system prompt present and uncertainty protocol in system context on re-prompt
- [ ] AC-17b Re-prompted response includes Phase 16 context snapshot (current app/element visible to model on re-prompt)
- [ ] AC-18 Existing SPEAKING→IDLE timing (Phase 18/19) unaffected by retrieval path
- [ ] AC-23 `RetrievalPipelineTrait` exists in `retrieval/pipeline.rs`; `CoreOrchestrator` holds `Box<dyn RetrievalPipelineTrait>` or equivalent; `MockRetrievalPipeline` in `#[cfg(test)]`
- [ ] AC-19 `cargo test` ≥ 214 passing, 0 failed
- [ ] AC-20 `cargo test` 0 warnings
- [ ] AC-21 `swift build` 0 project-code warnings
- [ ] AC-22 `uv run pytest` 19/19

---

## 8. Known Pitfalls

**Pitfall: Interceptor returns empty `Passthrough` during scan window**

When the buffer contains a potential-prefix tail (e.g., `"[UNCE"`) but the sentinel isn't
confirmed yet, `scan_for_prefix` returns `Passthrough("")`. The orchestrator must handle
empty passthrough strings silently (not forward zero-length `TextResponse` chunks or push
them to the TTS pipeline). The `if !text.is_empty()` guard in the token loop handles this.

**Pitfall: `push_tool_result` changes conversation context permanently**

`push_tool_result` appends to `ConversationContext.messages`. If the re-prompt fails
(second generation stream errors out), the injected retrieval result stays in context for
subsequent turns. This is intentional — the retrieved fact remains available as context.
If the retrieval failure injection is pushed and the subsequent model call uses it, the
model's explicit uncertainty acknowledgment is also in context for future turns.

**Pitfall: Retrieval-first check runs on every text input**

`is_retrieval_first_query` is a fast string scan — no allocations beyond `to_lowercase()`,
which is O(n) on input length. For typical conversational inputs (< 200 chars), this is
sub-microsecond. Not a latency concern.

**Pitfall: Re-prompt stream doesn't go through the interceptor**

The re-prompt response uses a fresh `re_stream` and does NOT go through the interceptor
(which is already in `Intercepted` → `Passthrough` state after the first sentinel). If
the model outputs a second `[UNCERTAIN:]` in the re-prompted response, it will be passed
through as literal text. This is acceptable: the system prompt instructs "use the marker
once per uncertain fact." Two nested retrievals in one response would require a recursive
loop and is not worth the complexity for v1. The literal marker appearing in output (very
rare in practice) is a minor visual artifact, not a correctness failure.

**Pitfall: `SentenceSplitter` doesn't flush on bridging phrase if it has no terminal punctuation**

All four bridging phrases end with `.` — they are complete sentence boundaries. The
existing `SentenceSplitter` will flush them immediately. Do not modify the bridging
phrases to remove terminal punctuation.

**Pitfall: `MockRetrievalPipeline` in `#[cfg(test)]` blocks**

`RetrievalPipelineTrait` is a mandatory prerequisite (see Step 1). `MockRetrievalPipeline`
lives in `#[cfg(test)]` inside `orchestrator.rs` and holds a canned `RetrievalResult` plus
counters (`call_count`, `was_called_before_model`). These counters are what the orchestrator
unit tests assert against. Keep the mock minimal — one canned response, no logic.

---

## 9. Phase 21 Preview (not in scope here)

Phase 21 is **Cross-Session Memory**: embedding conversation turns into the `VectorStore`,
retrieving relevant prior context at session start and on relevant queries, and surfacing
"you asked me about this before" awareness. Phase 19's `push_tool_result` and
`RetrievalPipeline::retrieve()` call paths are reused in Phase 21 — the scaffolding is
already correct.

After Phase 21, the local vector store becomes a first-class retrieval source. Phase 19's
retrieval-first classifier will be extended to check local store first (for operator-specific
facts) before firing web retrieval.
