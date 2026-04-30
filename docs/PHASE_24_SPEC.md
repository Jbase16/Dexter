# Phase 24 — Concurrent Interaction Pipeline & Latency Elimination

## Version 2.0 — 2026-03-16

---

## Problem Statement

Three architectural bottlenecks remain after Phase 23. Each is a non-negotiable —
the only acceptable outcome is full elimination, not mitigation.

| # | Problem | Root Cause | Impact |
|---|---------|-----------|--------|
| 1 | **Actions freeze the system for 30–60s** | `action_engine.submit()` is awaited inline inside `handle_text_input`, which runs inside the `select!` event loop. While `.submit()` blocks, no `ClientEvent` can be processed — hotkey presses, audio playback signals, and UI actions all queue silently. | Operator presses hotkey → nothing happens. System appears dead. |
| 2 | **Entity stays in THINKING after speech ends** | The orchestrator sends THINKING at inference start, then blocks in `submit()`. TTS completes and Swift sends `AUDIO_PLAYBACK_COMPLETE`, but it queues behind the blocked `select!` loop. The orb looks frozen for the entire action duration. | 30+ seconds of wrong visual state. Feels broken. |
| 3 | **Voice round-trip is 5–10s** | Sequential pipeline: VAD→STT→gRPC→routing→retrieval→cold inference→streaming→TTS→playback. Each stage waits for the previous to fully complete. | Conversational latency like a satellite phone. Kills the "presence" illusion. |

---

## Novel Solution Architecture

### Core Concept: Why These Problems Are Hard

These three problems seem independent, but they share a root cause: **the orchestrator
treats each voice interaction as a single, monolithic, sequential computation**. The
600-line `handle_text_input` method runs 10 phases in strict order. Any phase that
blocks (action execution) freezes everything. Any dead time between phases (waiting for
STT while inference sits idle) is wasted.

Commercial voice assistants avoid this by scaling horizontally in the cloud — they throw
more GPUs at the problem. Dexter can't. It has one GPU, one TTS worker, one STT worker,
36GB of shared memory. The solutions must exploit *structural concurrency within fixed
resources* — a fundamentally different optimization domain than "add more servers."

The three solutions below are interdependent. They're designed as a unified architecture,
not three independent patches.

---

## Solution 1: Speech-Concurrent Prompt Prefilling

### The Novel Insight

The system prompt + personality + context snapshot + memory recall entries constitute
500–1000 tokens. These tokens are **invariant within a single voice interaction** — they
don't change between the hotkey press and the transcript arrival. Currently, the
orchestrator doesn't touch the inference engine until after the transcript arrives from
Swift, processes routing, runs retrieval, builds the message list, and then sends
everything to Ollama. The LLM spends its first 200–500ms just processing tokens the
system already knew about.

**The key realization:** Ollama's KV cache is keyed by prompt prefix. If a previous
request processed `[system_prompt, context_snapshot]`, a subsequent request with
`[system_prompt, context_snapshot, user_query]` reuses the cached KV entries for the
matching prefix. Only the new user query tokens (~20–50) need processing.

This means: if we send a **prefill request** containing just the invariant prefix
*while the user is still speaking*, the KV cache is warm by the time the real query
arrives. The LLM skips 500–1000 tokens of prefill and produces its first output token
within ~100ms of receiving the user message.

No voice assistant does this. Cloud systems don't need to — their GPUs process system
prompts in single-digit milliseconds. Local systems haven't connected the "user is
speaking" signal to "inference engine should be preparing." This bridges the gap.

### Mechanism

**Trigger: hotkey press (LISTENING state transition)**

When the orchestrator receives `HotkeyActivated`, it currently just sends
`EntityState::Listening` to Swift. Now it *also* fires a background prefill:

```rust
Ok(SystemEventType::HotkeyActivated) => {
    self.send_state(EntityState::Listening, &trace_id).await?;

    // NOVEL: begin KV cache prefill while operator speaks.
    // The system prompt + context snapshot won't change during this utterance.
    self.prefill_inference_cache().await;
}
```

**Prefill implementation:**

```rust
async fn prefill_inference_cache(&self) {
    let recall_entries = vec![];  // no query yet → no recall
    let messages = self.prepare_messages_for_inference(&recall_entries);
    // messages = [system_prompt, context_snapshot] — the invariant prefix

    let engine     = self.engine.clone();
    let model_name = self.model_config.fast.clone();

    tokio::spawn(async move {
        // num_predict: 1 → process all input tokens (populating KV cache),
        // generate exactly 1 output token, then stop. The output is discarded.
        // Cost: ~200ms for 500-token prefix on warm qwen3:8b.
        // Benefit: next request with same prefix skips all those tokens.
        let url = format!("{}/api/chat", OLLAMA_BASE_URL);
        let body = serde_json::json!({
            "model":    model_name,
            "messages": messages.iter().map(|m| {
                serde_json::json!({"role": m.role, "content": m.content})
            }).collect::<Vec<_>>(),
            "stream":     false,
            "think":      false,
            "options":    { "num_predict": 1 },
            "keep_alive": FAST_MODEL_KEEP_ALIVE,
        });

        // Fire and forget — if it fails, the normal path runs.
        // The prefill races against the user's speech + STT processing.
        // Speech: ~1.5–3s. STT: ~1s. Prefill: ~200ms. The prefill wins by 2+ seconds.
        let client = reqwest::Client::new();
        let _ = client.post(&url).json(&body).send().await;
    });
}
```

**Why `num_predict: 1` and not 0:**

Ollama's `/api/chat` requires at least one generation step to commit the prompt
processing to the KV cache. With `num_predict: 0`, some llama.cpp backends skip
the prompt evaluation entirely (it's an optimization — why evaluate if you won't
generate?). `num_predict: 1` guarantees the full prompt is processed and cached.

**KV cache prefix matching — how it works under the hood:**

Ollama uses llama.cpp's KV cache, which works per-model-instance:
1. For each new request, tokenize the full prompt
2. Compare token-by-token against the cached sequence from the previous request
3. Reuse all matching prefix tokens (skip their attention computation)
4. Process only the divergent suffix

When the real request arrives with `[system_prompt, context_snapshot, user_query]`,
the prefix `[system_prompt, context_snapshot]` is already cached. The LLM only
processes the ~20–50 user query tokens before generating.

**Context change invalidation:**

If the operator switches apps between the hotkey press and the real query (extremely
unlikely during a 2–3 second utterance), the context snapshot changes. The prefill's
KV cache has the old context. Ollama detects the prefix divergence at the context
tokens and falls through to full processing. Cost of a wrong prediction: zero — it
degrades to current behavior. No incorrect output is ever produced.

**Expected latency reduction: 200–500ms in the common case (single-client Ollama,
no intervening requests between prefill and real query).** Ollama's KV cache is
slot-local: if the prefill occupies slot 0 and an intervening request (another app
using the same Ollama instance, a concurrent embed call) causes the real query to be
assigned slot 1, there is zero cache reuse. In a multi-slot or multi-client
configuration, the prefill degrades to a no-op — costs ~200ms, saves nothing. No
incorrect output is ever produced; the benefit is purely latency.

**Verification:** The integration test `prefill_actually_reduces_latency` (see
Acceptance Criteria) measures first-token latency with and without prefill across
10 paired trials and asserts that the prefilled median is at least 30% lower. This
is the only reliable way to confirm cache reuse is occurring — inspecting whether the
request was sent is insufficient.

### Why This Is Different From Simple Model Warm-Up

`warm_up_fast_model()` (Phase 23 pattern) loads the model weights into VRAM.
Speech-concurrent prefilling goes further: it pre-computes the *attention states*
for the specific prompt the next request will use. Model warm-up is like starting a
car engine. Prefill is like starting the engine, entering the destination, and
computing the route — all while the passenger is still getting in.

---

## Solution 2: Environmental-Signal-Driven KV Cache Maintenance

### The Novel Insight

Solution 1 exploits the dead time during speech. But there's an even larger window
of dead time: **between interactions**. The operator spends 30 seconds reading code,
switches from Xcode to Safari, reads a webpage, switches back. During all of this,
the context snapshot changes (new app, new focused element) and the inference engine
sits completely idle.

Currently, the KV cache from the previous interaction is stale — it has the old
context. When the operator speaks again, Ollama must re-process the full system
prompt with the new context from scratch.

**The novel architecture: the context observer triggers proactive KV cache updates.**

Every time the context snapshot changes (app switch, element focus change), the
orchestrator fires a background prefill with the updated context. The KV cache is
*always current*. When the operator speaks, the inference engine has zero prompt
processing to do — everything is already cached.

This is **predictive inference preparation driven by environmental signals**. It's
analogous to CPU instruction prefetching, but instead of predicting which memory
addresses will be accessed, we're predicting which prompt prefix will be needed and
pre-computing its attention states.

### Mechanism

```rust
// In handle_system_event, after context snapshot update:
Ok(SystemEventType::AppFocused) => {
    let changed = self.context_observer.update_from_app_focused(&sys.payload);
    if changed {
        // ... existing proactive observation logic ...

        // NOVEL: re-warm KV cache with updated context.
        // The next voice query will find its full prefix already cached.
        self.prefill_inference_cache().await;
    }
}

Ok(SystemEventType::AxElementChanged) => {
    let changed = self.context_observer.update_from_element_changed(&sys.payload);
    if changed {
        // Rate-limit: don't re-prefill on every keystroke.
        // Only fire if the element ROLE changed (not just value_preview).
        if self.context_observer.role_changed_since_last_prefill() {
            self.prefill_inference_cache().await;
        }
    }
}
```

**Rate limiting:**

`AxElementChanged` fires frequently (every keystroke in a text editor). The prefill
must be rate-limited to avoid flooding Ollama:

```rust
// On orchestrator
last_prefill_at: Option<Instant>,

async fn prefill_inference_cache(&mut self) {
    // Debounce: at most once per 5 seconds
    if let Some(last) = self.last_prefill_at {
        if last.elapsed() < Duration::from_secs(5) { return; }
    }
    self.last_prefill_at = Some(Instant::now());

    // ... same prefill logic as Solution 1 ...
}
```

**VRAM impact:**

The prefill request uses qwen3:8b (already pinned via `keep_alive`). It generates
1 token and stops. The total VRAM cost is: model weights (already loaded) + KV cache
for ~1000 tokens (~2MB for 8B model at Q4). Negligible.

**Combined with Solution 1:**

When the hotkey is pressed, `prefill_inference_cache()` runs again — but if the
context hasn't changed since the last environmental prefill, Ollama's KV cache
already has the exact prefix. The hotkey-triggered prefill completes in <10ms
(no new tokens to process). The system is *already ready* before the user opens
their mouth.

**Expected latency reduction: in the common case (no context change between last
environmental event and hotkey press), prompt prefill time drops to near-zero.**

---

## Solution 3: Interaction-Scoped Concurrency with Resource Multiplexing

### The Novel Insight

The action-blocking problem isn't just "spawn the action in the background." That's
the obvious engineering. The hard problem is **resource contention**.

Dexter has exactly one TTS worker. When a background action completes and the
orchestrator needs to speak the result, what happens if the operator just pressed the
hotkey and a new interaction's response is currently being synthesized? Two consumers
fight over one TTS pipe. Without careful design, this produces:
- Interleaved audio ("It's Done. three o' Completed. clock.")
- Deadlocked mutex (both interactions hold different parts of the pipeline)
- Silent failures (second consumer gets `Err` from `write_frame`)

Commercial voice assistants solve this with separate TTS instances per request
(cloud-scale). Dexter can't. It has one kokoro-82M worker with one stdin/stdout pipe.

**The novel architecture: Interaction-scoped state machines with priority-scheduled
access to constrained resources.**

Each voice interaction (hotkey press → response → optional action) is modeled as an
`Interaction` with its own lifecycle state. The orchestrator manages a set of
concurrent interactions. A priority scheduler controls access to the TTS worker:
the **active interaction** (the one the user most recently initiated) always has
priority. Background interactions (action results from previous queries) queue
their TTS requests and execute when the active interaction releases the worker.

This is the **actor-per-request pattern** from high-performance server architectures
(Actix, Erlang/OTP), but adapted for a system with *fixed, non-scalable resources*.
The novelty is in the priority scheduling — something cloud systems never need
because they can just allocate another worker.

### Architecture

```rust
/// Represents a single voice interaction from hotkey to final IDLE.
struct Interaction {
    id:           String,
    trace_id:     String,
    stage:        InteractionStage,
    priority:     InteractionPriority,
    created_at:   Instant,
}

enum InteractionStage {
    /// Waiting for STT transcript
    AwaitingTranscript,
    /// Inference + TTS in progress
    Generating,
    /// TTS done, action executing in background
    ActionInFlight { action_id: String },
    /// Action complete, TTS feedback queued
    FeedbackPending { text: String },
    /// All done — ready for cleanup
    Complete,
}

enum InteractionPriority {
    /// Most recent user-initiated interaction — owns the TTS worker
    Active,
    /// Previous interaction with a background action still running
    Background,
}
```

**When the operator starts a new interaction while an action is running:**

```
Timeline:
  t=0:   Operator says "Open weather.com"
  t=3:   Dexter speaks "Opening weather.com" → TTS finishes → action dispatched
  t=3:   Entity state: FOCUSED (action running in background)
  t=5:   Operator presses hotkey again
  t=5:   New Interaction created (Active priority)
  t=5:   Previous interaction demoted to Background priority
  t=5:   Entity state: LISTENING (new interaction)
  t=7:   Operator says "What time is it?"
  t=9:   Dexter speaks "It's 3:15 PM" → TTS finishes
  t=9:   Entity state: IDLE
  t=12:  Background action completes → TTS queue not empty
  t=12:  TTS worker is free (no Active interaction using it)
  t=12:  Background interaction speaks "Done. Weather.com is loaded."
  t=14:  Entity state: IDLE (both interactions complete)
```

**The key invariant: an Active interaction ALWAYS preempts Background TTS.**

### TTS Priority Scheduler

```rust
struct TtsPriorityQueue {
    /// Currently synthesizing interaction ID (if any)
    current_owner: Option<String>,
    /// Queued TTS requests, ordered by priority then submission time
    queue: VecDeque<TtsRequest>,
}

struct TtsRequest {
    interaction_id: String,
    text:           String,
    trace_id:       String,
    priority:       InteractionPriority,
}

impl TtsPriorityQueue {
    /// Submit a TTS request. Returns immediately — synthesis happens asynchronously.
    fn enqueue(&mut self, request: TtsRequest) {
        // Active-priority requests go to the front (after any other Active requests).
        // Background requests go to the back.
        match request.priority {
            InteractionPriority::Active => {
                let insert_pos = self.queue.iter()
                    .position(|r| matches!(r.priority, InteractionPriority::Background))
                    .unwrap_or(self.queue.len());
                self.queue.insert(insert_pos, request);
            }
            InteractionPriority::Background => {
                self.queue.push_back(request);
            }
        }
    }

    /// Called by the TTS consumer task when the worker is free.
    fn next(&mut self) -> Option<TtsRequest> {
        self.queue.pop_front()
    }
}
```

**TTS consumer task — new `select!` branch:**

```rust
// ipc/server.rs — the event loop gains two new branches

loop {
    tokio::select! {
        msg = inbound.message() => { ... }

        // NEW: action results from background tasks
        result = action_rx.recv() => {
            if let Some(result) = result {
                orchestrator.handle_action_result(result).await?;
            }
        }

        // NEW: TTS queue consumer — processes queued TTS requests sequentially
        _ = tts_ready_signal.notified() => {
            if let Some(request) = orchestrator.tts_queue.next() {
                orchestrator.process_tts_request(request).await?;
            }
        }

        _ = health_interval.tick() => { ... }
        _ = browser_health_interval.tick() => { ... }
    }
}
```

**Why this doesn't deadlock:**

The TTS worker is accessed through `Arc<Mutex<Option<WorkerClient>>>`. The
current design holds this mutex for the duration of a sentence synthesis (~200ms).
With the priority queue, only one TTS request is active at a time — the mutex is
never contended. The queue serializes access by construction, not by lock.

The `tts_ready_signal` is a `tokio::sync::Notify` that fires when:
1. A new TTS request is enqueued AND the worker isn't currently busy
2. The current TTS request completes (the worker becomes free)

This avoids polling — the consumer task only wakes when there's work to do.

### Entity State During Concurrent Interactions

The entity state reflects the *highest-priority active interaction*:

```rust
fn compute_entity_state(&self) -> EntityState {
    // Active interaction state takes precedence
    if let Some(active) = self.interactions.values()
        .find(|i| matches!(i.priority, InteractionPriority::Active))
    {
        return match active.stage {
            InteractionStage::AwaitingTranscript => EntityState::Listening,
            InteractionStage::Generating         => EntityState::Thinking,
            InteractionStage::ActionInFlight { .. } => EntityState::Focused,
            InteractionStage::FeedbackPending { .. } => EntityState::Speaking,
            InteractionStage::Complete           => EntityState::Idle,
        };
    }

    // No active interaction — check background
    if self.interactions.values().any(|i|
        matches!(i.stage, InteractionStage::ActionInFlight { .. } |
                          InteractionStage::FeedbackPending { .. })
    ) {
        return EntityState::Focused;  // Background work still running
    }

    EntityState::Idle
}
```

**SPEAKING state — emitted from the TTS task, not the orchestrator:**

```rust
// Inside the TTS synthesis loop:
if seq == 0 {
    // First audio frame of this synthesis — entity is now SPEAKING
    let state_evt = ServerEvent {
        trace_id: trace_id.clone(),
        event: Some(server_event::Event::EntityState(EntityStateChange {
            state: EntityState::Speaking.into(),
        })),
    };
    let _ = session_tx.send(Ok(state_evt)).await;
}
```

This is sent from inside the spawned TTS task, which already has a clone of
`session_tx`. No new channels needed. The transition is perceptually precise:
the entity shows SPEAKING at the exact moment audio starts streaming, not when
synthesis is queued.

### Action Dispatch — Non-Blocking by Construction

```rust
// In handle_text_input, replacing the current submit() call:

if let Some(spec) = action_spec {
    let category  = PolicyEngine::classify(&spec);
    let action_id = Uuid::new_v4().to_string();

    if category == ActionCategory::Destructive {
        // DESTRUCTIVE: same as today — present dialog, wait for approval
        self.send_action_request(/* ... */).await?;
        self.send_state(EntityState::Alert, &trace_id).await?;
        self.action_awaiting_approval = true;
        return Ok(());
    }

    // SAFE / CAUTIOUS: dispatch to background task
    let executor  = self.action_engine.executor_handle();
    let action_tx = self.action_tx.clone();
    let aid       = action_id.clone();
    let tid       = trace_id.clone();

    tokio::spawn(async move {
        let outcome = executor.execute(spec, &aid).await;
        let _ = action_tx.send(ActionResult { action_id: aid, outcome, trace_id: tid }).await;
    });

    // Track in-flight action for this interaction
    self.current_interaction_mut().stage = InteractionStage::ActionInFlight {
        action_id: action_id.clone(),
    };
    // Entity → FOCUSED (action running, operator can interact)
    self.send_state(EntityState::Focused, &trace_id).await?;
    return Ok(());  // Event loop is free immediately
}
```

**`ExecutorHandle` — clone-able execution context:**

```rust
/// Owns everything needed to execute an action independently of the orchestrator.
/// Clone-able: each spawned action task gets its own handle.
#[derive(Clone)]
pub struct ExecutorHandle {
    browser:   Arc<Mutex<BrowserCoordinator>>,
    audit:     Arc<Mutex<AuditLog>>,
    state_dir: PathBuf,
}

impl ExecutorHandle {
    pub async fn execute(&self, spec: ActionSpec, action_id: &str) -> ActionOutcome {
        let result = match &spec {
            ActionSpec::Shell { args, working_dir, .. } => {
                executor::execute_shell(args, working_dir.as_deref(), ACTION_DEFAULT_TIMEOUT_SECS).await
            }
            ActionSpec::Browser { action, .. } => {
                let browser = self.browser.lock().await;
                executor::execute_browser(&browser, action, BROWSER_WORKER_RESULT_TIMEOUT_SECS).await
            }
            // ... other action types ...
        };

        // Audit logging
        let entry = AuditEntry::from_result(action_id, &spec, &result);
        if let Ok(mut audit) = self.audit.lock().await {
            audit.append(entry);
        }

        match result {
            Ok(output) => ActionOutcome::Completed {
                action_id: action_id.to_string(),
                output,
            },
            Err(_) => ActionOutcome::Rejected {
                action_id: action_id.to_string(),
            },
        }
    }
}
```

**Why the `Arc<Mutex<BrowserCoordinator>>` works without deadlocks:**

The browser coordinator communicates with the Playwright worker via stdin/stdout
pipes. These pipes are inherently serial — only one command can be in flight at a
time. The mutex simply enforces this serialization explicitly. Since we only dispatch
one action at a time (the staleness guard ensures this), the mutex is effectively
uncontended. It exists for safety, not for real concurrent access.

### Interaction Lifecycle and Staleness Detection

**Lookup key:** `action_id` (UUID string). The `ActionResult` struct carries the
`action_id` that was assigned at dispatch time. The orchestrator maintains a
`HashMap<String, Interaction>` keyed by `action_id`.

**Staleness detection mechanism:**

```rust
// On orchestrator
interactions: HashMap<String, Interaction>,

// When dispatching an action:
self.interactions.insert(action_id.clone(), Interaction {
    id:         action_id.clone(),
    trace_id:   trace_id.clone(),
    stage:      InteractionStage::ActionInFlight { action_id: action_id.clone() },
    priority:   InteractionPriority::Active,
    created_at: Instant::now(),
});

// When a new hotkey press starts a new interaction:
// Demote all existing Active interactions to Background
for interaction in self.interactions.values_mut() {
    if matches!(interaction.priority, InteractionPriority::Active) {
        interaction.priority = InteractionPriority::Background;
    }
}

// When handle_action_result receives a result:
async fn handle_action_result(&mut self, result: ActionResult) -> Result<(), OrchestratorError> {
    let interaction = match self.interactions.get_mut(&result.action_id) {
        Some(i) => i,
        None => {
            // Unknown action_id — result arrived for an interaction that was
            // already garbage-collected. Log and discard.
            warn!(
                action_id = %result.action_id,
                "Action result for unknown interaction — discarding"
            );
            return Ok(());
        }
    };

    // Transition interaction to FeedbackPending
    let feedback_text = match &result.outcome {
        ActionOutcome::Completed { output, .. } => {
            if output.trim().is_empty() { "Done.".to_string() } else { output.clone() }
        }
        ActionOutcome::Rejected { .. } => {
            "Sorry, I wasn't able to complete that action.".to_string()
        }
    };
    interaction.stage = InteractionStage::FeedbackPending { text: feedback_text.clone() };

    // Queue TTS feedback with the interaction's priority
    self.tts_queue.enqueue(TtsRequest {
        interaction_id: interaction.id.clone(),
        text:           feedback_text,
        trace_id:       interaction.trace_id.clone(),
        priority:       interaction.priority.clone(),
    });
    self.tts_ready_signal.notify_one();

    Ok(())
}
```

**TTL and garbage collection:**

Completed interactions (stage = `Complete`) are removed from the `interactions`
map immediately after their TTS feedback finishes playing (triggered by
`AUDIO_PLAYBACK_COMPLETE` for that interaction's trace_id). As a safety net,
a periodic sweep (every 60 seconds, piggybacked on the health check timer)
removes any interaction older than 5 minutes regardless of stage — this catches
edge cases where a spawned action task silently panics and never delivers a result.

```rust
// In health check timer branch:
self.interactions.retain(|_, i| i.created_at.elapsed() < Duration::from_secs(300));
```

**What happens when a result arrives for an unknown `action_id`:**

1. The `interactions.get_mut()` call returns `None`
2. A warning is logged with the orphaned `action_id`
3. The result is discarded — no TTS feedback, no state transition
4. The method returns `Ok(())` — not an error, just a late/orphaned result

This covers:
- Action task completes after its interaction was garbage-collected (>5 min)
- Action task completes after `shutdown()` cleared the map
- Duplicate delivery (impossible with `mpsc`, but defensive)

**The staleness test (`action_staleness_guard_discards_old`):**

```rust
#[tokio::test]
async fn action_staleness_guard_discards_old() {
    let (mut orch, mut rx) = make_orchestrator(tmp.path());
    // No interaction registered for this action_id
    let result = ActionResult {
        action_id: "unknown-id".to_string(),
        outcome: ActionOutcome::Completed { action_id: "unknown-id".into(), output: "hi".into() },
        trace_id: new_trace(),
    };
    let r = orch.handle_action_result(result).await;
    assert!(r.is_ok());
    // No events emitted — result was silently discarded
    tokio::task::yield_now().await;
    assert!(rx.try_recv().is_err(), "stale result must not emit any events");
}
```

---

## Solution 4: Conversation-Adaptive Endpoint Detection

### The Novel Insight

The VAD uses a fixed silence threshold: 20 frames × 32ms = 640ms. This is a
compromise — short enough to not feel sluggish, long enough to not clip mid-sentence
pauses.

But the optimal threshold depends on conversational context:

| After Dexter says... | Expected response | Optimal silence |
|---------------------|-------------------|-----------------|
| "Should I delete this file?" | "Yes" / "No" | 250ms (8 frames) |
| "What should I remember?" | A sentence or two | 640ms (20 frames) |
| "Anything else?" | "No" / silence | 250ms (8 frames) |
| (operator initiates) | Unknown length | 640ms (20 frames) |

**The LLM already knows what kind of response it asked for.** It generated the
question. We can use the response content to predict the next utterance's
characteristics and adjust the VAD accordingly.

This is **top-down modulation of acoustic processing** — higher-level language
understanding shaping lower-level signal detection. The closest analogy in
neuroscience is predictive coding, where the brain's expectations about incoming
stimuli shape how sensory cortex processes them. No production voice assistant
does this — they all use fixed or learned endpoint detectors trained on millions
of utterances. We're using the LLM's own conversational understanding instead.

### Mechanism

**New proto message: `VadHint`**

```protobuf
message VadHint {
    uint32 silence_frames = 1;   // Override VAD_SILENCE_FRAMES for next utterance
}
```

Sent from Rust → Swift as a `ServerEvent` variant. Swift's `VoiceCapture` applies
the override for the next utterance only, then resets to the default.

**Response classification — timing is critical.**

The hint must be sent DURING streaming, not after generation completes. By the time
`generate_primary` returns and `full_response` is available, the operator has already
heard the response, may have started speaking, and the VAD falling edge may have
already fired with the default 640ms threshold. A hint sent after generation is
useless — the window it was supposed to optimize has already closed.

**Correct timing: integrate with the `SentenceSplitter` inside `generate_primary`.**

The `SentenceSplitter` detects complete sentences during token streaming. The VadHint
is computed on each complete sentence. When the LAST sentence is detected (either via
`splitter.flush()` at generation end, or when a sentence boundary is found in the
final tokens), `classify_expected_response` runs on that sentence and sends the hint
immediately — BEFORE the TTS `is_final` sentinel, so Swift receives the hint while
audio is still playing.

```rust
// Inside generate_primary, in the token loop:

// When splitter produces a sentence:
for sentence in splitter.push(&text) {
    if let Some(ref tx) = tts_tx {
        let _ = tx.send(sentence.clone());
    }
    // Track the latest complete sentence for VadHint classification
    last_sentence = Some(sentence);
}

// ... and at the end, after normal completion:
if let Some(remainder) = splitter.flush() {
    if let Some(ref tx) = tts_tx {
        let _ = tx.send(remainder.clone());
    }
    last_sentence = Some(remainder);
}

// Send VadHint based on the LAST sentence — before is_final sentinel
if let Some(ref sentence) = last_sentence {
    if let Some(hint) = classify_expected_response(sentence) {
        self.send_vad_hint(hint, trace_id).await?;
    }
}

// THEN send is_final text sentinel
self.send_text("", true, trace_id).await?;
```

The `send_vad_hint` method sends a `ServerEvent::VadHint` to Swift via the
existing `session_tx` channel. Swift receives it, applies it to `VoiceCapture`,
and the next utterance's silence detection uses the override.

**Why last sentence, not full response:**

Only the last sentence determines the expected reply type. "Let me explain how
that works. First, the system loads the model. Should I continue?" — only the
final question matters. Using `full_response` would require scanning the entire
text for the final question, which is equivalent to just using the last sentence.

```rust
fn classify_expected_response(last_sentence: &str) -> Option<VadHint> {
    let sentence = last_sentence.trim().to_lowercase();

    // Direct yes/no question patterns
    let yes_no_patterns = [
        "should i", "shall i", "do you want", "would you like",
        "is that ok", "is that correct", "right?", "yes or no",
        "want me to", "go ahead?",
    ];

    if sentence.ends_with('?') &&
       yes_no_patterns.iter().any(|p| sentence.contains(p))
    {
        return Some(VadHint { silence_frames: 8 });  // 256ms
    }

    // Open question or declarative — keep default
    None
}
```

**Swift-side integration:**

```swift
// VoiceCapture — new property
private var silenceFrameOverride: Int?

func applyVadHint(_ hint: VadHint) {
    callbackQueue.async {
        self.silenceFrameOverride = Int(hint.silenceFrames)
    }
}

// In processVAD, replace Constants.VAD_SILENCE_FRAMES with:
let threshold = silenceFrameOverride ?? Constants.VAD_SILENCE_FRAMES

// On falling edge (utterance delivered), reset:
silenceFrameOverride = nil  // One-shot: next utterance uses default
```

**Expected latency reduction: 200–400ms on yes/no follow-ups**, which are common
in action confirmation flows ("Should I run this?" → "Yes").

---

## Supporting Engineering

These practical changes support the novel solutions above. They are necessary but
not individually novel.

### A. Model `keep_alive` Pinning + Startup Warm-Up

Pin qwen3:8b permanently in VRAM so the KV cache prefill (Solutions 1+2) works
reliably — the model must be loaded for KV caching to function.

```rust
// constants.rs
pub const FAST_MODEL_KEEP_ALIVE: &str = "999m";  // ~16 hours

// OllamaChatRequest — always send keep_alive for FAST tier
let keep_alive = if req.unload_after {
    Some("0")
} else if req.model_name == self.config.fast_model {
    Some(FAST_MODEL_KEEP_ALIVE)
} else {
    None
};
```

**VRAM budget (36GB unified):**

| Model | Size | Policy |
|-------|------|--------|
| qwen3:8b (FAST) | ~5GB | Pinned permanently |
| mxbai-embed-large (EMBED) | ~0.7GB | `keep_alive: "10m"` |
| mistral-small:24b (PRIMARY) | ~16GB | On-demand, default TTL |
| deepseek-r1:32b (HEAVY) | ~20GB | On-demand, evict after use |
| OS + Swift + Rust | ~5GB | Always |
| **Available for PRIMARY** | **~24GB** | Sufficient for Q4_K_M |

Startup warm-up (same pattern as `warm_up_embed`):

```rust
pub fn warm_up_fast_model(&self) {
    let engine = self.engine.clone();
    let model  = self.model_config.fast.clone();
    tokio::spawn(async move {
        info!("Warming up FAST model: {model}");
        let req = GenerationRequest {
            model_name: model.clone(),
            messages: vec![Message { role: "user".into(), content: "hi".into(), images: None }],
            temperature: None,
            unload_after: false,
        };
        match engine.generate_stream(req).await {
            Ok(mut rx) => { while rx.recv().await.is_some() {} }
            Err(e) => warn!(error = %e, "FAST model warmup failed"),
        }
        info!("FAST model warm: {model}");
    });
}
```

### B. Parallel Routing + Retrieval

Routing and memory recall are independent — run them concurrently:

```rust
// Destructure to satisfy the borrow checker — disjoint fields
let context_messages = self.context.messages();
let engine           = &self.engine;
let embed_model      = &self.model_config.embed;
let router           = &self.router;
let retrieval        = &self.retrieval;

let (decision, recall_entries) = tokio::join!(
    async { router.route(&context_messages) },
    async { retrieval.recall_relevant(engine, embed_model, &content).await },
);
```

Saves ~200–400ms by overlapping the embedding API call (for recall) with CPU-only
routing logic.

### C. STT-to-Orchestrator Fast Path

Currently, the STT transcript flows: Rust (stream_audio) → gRPC → Swift → Swift
sends TextInput → gRPC → Rust (orchestrator). The Swift round-trip adds ~50–100ms
and is unnecessary for the inference trigger — Swift only needs the transcript for
display.

**New architecture: Swift-side echo suppression.**

The dedup must live on the Swift side, not the Rust side. A Rust-side
`processed_traces` HashSet has a race condition: the `select!` loop is
single-threaded, processing one branch at a time. If the gRPC echo from Swift
arrives in the inbound channel before the internal channel is polled, it processes
as a normal `TextInput` (not in `processed_traces` yet), and the fast path's event
arrives second — both execute. The interaction runs twice.

Swift controls the ordering deterministically: it receives the transcript from
`stream_audio`'s gRPC response, then decides whether to echo it back.

**Mechanism:**

1. **Rust side:** `stream_audio()` adds a `fast_path: bool` field to the
   `TranscriptChunk` gRPC response. When the orchestrator's internal channel
   (`orchestrator_tx`) is available, set `fast_path: true` on the transcript
   chunk AND deliver the transcript directly to the orchestrator.

2. **Swift side:** In the `streamAudioTask` response handler, check the
   `fast_path` flag on the final transcript chunk. If `true`, display the
   transcript in the UI but do NOT send it back as a `TextInput` event — the
   orchestrator already has it.

```swift
// DexterClient.swift — streamAudioTask response handler
var transcript = ""
var isFastPath = false
for try await chunk in response.messages {
    if chunk.isFinal {
        transcript = chunk.text
        isFastPath = chunk.fastPath
    }
}

guard !transcript.isEmpty else {
    // ... existing empty-transcript AUDIO_PLAYBACK_COMPLETE reset ...
    return
}

// Display transcript in UI regardless of fast_path
// (via a new UI-only event, or by letting the orchestrator's
// handle_text_input send the echo via TextResponse as it already does)

if !isFastPath {
    // Fallback path: no fast-path delivery happened — send TextInput as before
    let event = Dexter_V1_ClientEvent.with {
        $0.traceID   = UUID().uuidString
        $0.sessionID = sessionID
        $0.textInput = Dexter_V1_TextInput.with { $0.content = transcript }
    }
    await self?.send(event)
}
```

3. **Rust side — internal channel delivery:**

```rust
// In stream_audio(), after TRANSCRIPT_DONE:
let fast_path = if let Some(ref otx) = self.orchestrator_tx {
    // Deliver directly to orchestrator — bypasses gRPC round-trip
    let _ = otx.send(InternalEvent::TranscriptReady {
        text:     accumulated_text.clone(),
        trace_id: trace_id.clone(),
    }).await;
    true
} else {
    false  // Channel not available — Swift will echo as before
};

// Send transcript to Swift with fast_path flag
let chunk = TranscriptChunk {
    text:            accumulated_text,
    is_final:        true,
    sequence_number: seq,
    fast_path,       // NEW field — Swift uses this to suppress echo
};
let _ = tx.send(Ok(chunk)).await;
```

4. **Orchestrator select! branch:**

```rust
internal_event = internal_rx.recv() => {
    if let Some(InternalEvent::TranscriptReady { text, trace_id }) = internal_event {
        orchestrator.handle_text_input(text, trace_id).await?;
    }
}
```

**Why this eliminates the race:**

The ordering is now deterministic by construction:
- Rust sends the transcript via both channels in the same task, before yielding
- The `fast_path` flag is set atomically with the internal delivery
- Swift receives the flag and suppresses the echo deterministically
- There is no window where both paths can trigger `handle_text_input`

If `orchestrator_tx` is `None` (initialization edge case), `fast_path` is `false`,
Swift echoes the transcript as before, and the system degrades to current behavior.

**Expected improvement: 50–100ms saved on every voice interaction.**

---

## Combined Latency Budget

| Stage | Phase 23 | Phase 24 (best) | Phase 24 (no cache) | Technique |
|-------|---------|----------------|-------------------|-----------|
| VAD falling edge → STT start | 50ms | 50ms | 50ms | — |
| STT inference | 1000ms | 1000ms | 1000ms | — |
| STT → orchestrator | 100ms | **5ms** | **5ms** | Fast path (C) |
| Routing + retrieval | 500ms | **250ms** | **250ms** | Parallel (B) |
| Prompt prefill (system tokens) | 300ms | **0ms** ¹ | 300ms | Prefill (1+2) — if cache hits |
| qwen3:8b first token | 2000ms | **200ms** ¹ | **500ms** | Warm model (A) ± cached prefix |
| To first sentence | 700ms | 700ms | 700ms | — |
| TTS first sentence | 300ms | 300ms | 300ms | Already concurrent |
| Audio playback start | 50ms | 50ms | 50ms | — |
| **Total (first interaction)** | **5000ms** | **2550ms** | **3150ms** | |
| **Total (yes/no follow-up)** | **5000ms** | **2150ms** | **2750ms** | Adaptive VAD (4) |

¹ KV cache reuse depends on Ollama assigning the same slot to the prefill and real
request. Single-client Ollama (Dexter's sole user) achieves this in the common case.
Multi-client or intervening requests may cause a slot miss, falling back to "no cache"
column. The integration test `prefill_actually_reduces_latency` verifies this on the
target machine.

**Action execution latency: eliminated entirely.** Actions run in the background.
The operator hears the verbal response and can start a new interaction immediately.
Action feedback is spoken asynchronously when the worker completes.

---

## File Map

| Change   | File                                                  | Description |
|----------|-------------------------------------------------------|-------------|
| Modified | `src/rust-core/src/orchestrator.rs`                   | Interaction struct, prefill_inference_cache, handle_action_result, VadHint emission, parallel routing, fast-path dedup, TTS priority queue |
| Modified | `src/rust-core/src/action/engine.rs`                  | ExecutorHandle, classify_and_dispatch → background spawn |
| Modified | `src/rust-core/src/action/audit.rs`                   | `Arc<Mutex<AuditLog>>` for concurrent access |
| Modified | `src/rust-core/src/ipc/server.rs`                     | action_rx + tts_ready_signal + orchestrator_tx in select! loop |
| Modified | `src/rust-core/src/ipc/core_service.rs`               | Dual transcript delivery (Swift + orchestrator fast path) |
| Modified | `src/rust-core/src/inference/engine.rs`               | FAST_MODEL_KEEP_ALIVE in OllamaChatRequest, OllamaOptions num_predict field |
| Modified | `src/rust-core/src/constants.rs`                      | FAST_MODEL_KEEP_ALIVE constant |
| Modified | `src/rust-core/src/voice/coordinator.rs`              | No structural changes (already Arc-based) |
| Modified | `src/shared/proto/dexter.proto`                       | VadHint message, add to ServerEvent oneof |
| Modified | `src/swift/Sources/Dexter/Voice/VoiceCapture.swift`   | silenceFrameOverride, applyVadHint |
| Modified | `src/swift/Sources/Dexter/Bridge/DexterClient.swift`  | Handle VadHint event, forward to VoiceCapture |

---

## Implementation Order

### Phase 24a: Infrastructure (Sessions 1–2)

1. `ExecutorHandle` struct + `Arc<Mutex<AuditLog>>` refactor
2. `classify_and_dispatch` on ActionEngine → background spawn
3. `action_rx` channel in `select!` loop + `handle_action_result`
4. `Interaction` struct + `action_in_flight` tracking
5. FOCUSED state wiring (dispatch → FOCUSED, result → feedback → IDLE)
6. SPEAKING state from TTS task (first audio frame → SPEAKING)
7. Tests: action future delivery, staleness guard, FOCUSED lifecycle

### Phase 24b: Latency Architecture (Sessions 3–4)

8. `FAST_MODEL_KEEP_ALIVE` + `warm_up_fast_model()` at startup
9. `num_predict` field in `OllamaOptions`
10. `prefill_inference_cache()` — the core KV prefill method
11. Hotkey-triggered prefill (Solution 1)
12. Environmental-signal prefill with debounce (Solution 2)
13. Parallel routing + retrieval (`tokio::join!`)
14. Tests: prefill fires on hotkey, prefill fires on app switch, debounce works

### Phase 24c: Endpoint + Fast Path (Sessions 5–6)

15. `VadHint` proto message + Swift integration (`applyVadHint`, `silenceFrameOverride`)
16. `classify_expected_response` integrated into `generate_primary`'s streaming loop — sends VadHint after last sentence, BEFORE `is_final` sentinel
17. STT fast-path: `fast_path` field on `TranscriptChunk`, Swift-side echo suppression, `orchestrator_tx` internal channel
18. TTS priority queue + Notify-based consumer
19. Interaction lifecycle: `HashMap<String, Interaction>`, GC sweep, `handle_action_result` with unknown-ID guard
20. Tests: VadHint ordering, fast-path echo suppression, fast-path fallback, priority queue, staleness guard, interaction GC
21. `prefill_actually_reduces_latency` integration test (requires warm Ollama)
22. Full integration test suite — `cargo test` + `make test-python` clean

---

## Acceptance Criteria

### Automated

| # | Criterion |
|---|-----------|
| 1 | `cargo test` — 0 failures, 0 warnings from project code |
| 2 | `make test-python` — 0 failures |
| 3 | `action_future_delivers_result` — background action → result via action_rx |
| 4 | `action_staleness_guard_discards_old` — stale result logged and dropped |
| 5 | `focused_state_on_action_dispatch` — FOCUSED emitted when action spawned |
| 6 | `speaking_state_on_first_audio_frame` — SPEAKING emitted on seq=0 |
| 7 | `parallel_routing_and_retrieval` — both complete, results correct |
| 8 | `prefill_fires_on_hotkey` — prefill request sent after HotkeyActivated |
| 9 | `prefill_debounced_on_context_change` — second change within 5s skipped |
| 10 | `prefill_actually_reduces_latency` — integration test: 10 paired trials (with/without prefill), median first-token latency with prefill is ≥30% lower. Requires warm Ollama + qwen3:8b loaded. |
| 11 | `vad_hint_sent_before_is_final` — VadHint event appears in the output stream before the `is_final` TextResponse sentinel, not after generation completes |
| 12 | `vad_hint_reduces_silence_frames` — yes/no question → 8-frame hint |
| 13 | `fast_path_suppresses_swift_echo` — when fast_path=true, Swift does not send TextInput back |
| 14 | `fast_path_fallback_echoes` — when fast_path=false, Swift sends TextInput as before |
| 15 | `tts_priority_queue_active_first` — Active requests processed before Background |
| 16 | `stale_action_result_discarded` — result for unknown action_id logs warning, emits no events |
| 17 | `interaction_gc_removes_old_entries` — interactions older than 5 min removed by periodic sweep |

### Manual

| # | Criterion | Verification |
|---|-----------|-------------|
| 1 | Voice round-trip < 3s | End-of-speech → first audio playback |
| 2 | Hotkey works during action | Press hotkey while browser runs → new interaction starts |
| 3 | FOCUSED during action | Orb shows deep blue steady pulse |
| 4 | SPEAKING during TTS | Orb changes on first audio, not synthesis start |
| 5 | Action feedback spoken | Browser finishes → "Done." spoken asynchronously |
| 6 | Yes/no follow-up < 2s | Ask Dexter a question → "yes" → response < 2s |
| 7 | No regression | Normal flow cycles states correctly, gRPC stable |

---

## Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| Ollama KV cache slot mismatch between prefill and real request | KV cache is slot-local. If another request (embed call, other client) intervenes and causes a slot reassignment, the prefill's cached KV entries are on a different slot than the real request — zero cache reuse. **Detection:** `prefill_actually_reduces_latency` integration test measures first-token latency with/without prefill across 10 paired trials. If median improvement is <30%, the feature is disabled at runtime via a config flag. **Impact of miss:** costs ~200ms (the prefill request), saves nothing. No incorrect output. |
| Prefill request interferes with concurrent inference | The prefill targets FAST model only. PRIMARY/HEAVY/CODE run on different model instances. Ollama serializes requests to the same model — prefill completes in <200ms, well before the real query arrives 2–3s later. |
| `Arc<Mutex<AuditLog>>` contention | Audit writes are single JSONL appends (~0.1ms). One action at a time → mutex effectively uncontended. |
| TTS priority queue starvation of Background requests | Background requests are processed whenever the worker is free and no Active request is pending. A conversation with rapid-fire interactions could delay feedback indefinitely — but the text is always displayed in the UI immediately via `send_text`. Audio feedback is best-effort. |
| Environmental prefill floods Ollama on rapid context switches | 5-second debounce. Maximum prefill rate: 12/minute. Each is a tiny request (1 output token). Ollama handles this trivially. |
| `VadHint` arrives too late (after operator starts speaking) | Hint is sent DURING streaming (after last sentence detected by `SentenceSplitter`), BEFORE the `is_final` text sentinel. Swift receives it while TTS audio is still playing. For a typical 2-sentence response with 1s of audio, the hint arrives ~500ms before playback ends — well before the operator can respond. Test `vad_hint_sent_before_is_final` verifies ordering. |
| `VadHint` with wrong silence_frames clips user speech | Hint is conservative: only shortens for yes/no questions where the response is predictably short. Default 640ms applies for all other cases. One-shot reset after each utterance — a wrong hint affects exactly one interaction. |
| Spawned action outlives session | `shutdown()` clears the `interactions` map. Spawned task's `action_tx.send()` returns `Err` (receiver dropped) — task exits cleanly. No zombie processes. |
| Action result arrives for garbage-collected interaction | `handle_action_result` checks `interactions.get_mut(&action_id)`. Returns `None` → logs warning → discards result → returns `Ok(())`. No panic, no TTS, no state transition. |
