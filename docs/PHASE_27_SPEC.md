# Phase 27 Implementation Plan: Voice Barge-In

## Context

After Phases 23–26, the voice interaction model is fast, readable, and responsive. One
fundamental limitation remains: the operator cannot interrupt Dexter mid-speech. If Dexter
is in the middle of a long TTS response and the operator wants to redirect or correct, the
only option is to wait for playback to finish.

Phase 27 implements full barge-in: the operator speaks naturally during SPEAKING state and
Dexter immediately stops TTS, cancels the ongoing generation, and transitions to LISTENING.
No hotkey required to interrupt — speech during SPEAKING is always a barge-in signal.

**Scope:** Proto change (new message), Swift changes (3 files), Rust changes (2 files).
No Python worker changes. No new dependencies.

---

## Diagnosis of Current State

### What already exists (not changing):

- `VoiceCapture.onSpeechStart` — callback on VAD rising edge, fires when `isActive = true`
- `DexterClient`: wires `capture.onSpeechStart = { audioPlayer.stop() }` — stops playback
- `AudioPlayer.stop()` — clears buffer queue, fires `onPlaybackFinished` if `awaitingFinalCallback`
  was set (so Rust gets `AUDIO_PLAYBACK_COMPLETE` even on interrupted proactive TTS)

### Three bugs to fix:

**Bug 1 — Re-arm race (AudioPlayer.swift line 133):**
`stop()` calls `player.play()` immediately after `player.stop()`. This re-arms the
`AVAudioPlayerNode`. TTS frames from Rust's still-running generation arrive after the
stop, are enqueued, and *play* — the operator hears the rest of the response they
intended to interrupt. The `player.play()` in `stop()` was correct for the Phase 18
proactive-observation use case (where the operator hadn't pressed the hotkey), but it
has the wrong semantics for barge-in.

**Fix:** Lazy re-arm. Remove `player.play()` from `stop()`. Add it to
`flushReadyBuffers()` — the player is re-armed only when new audio actually arrives.
Phase 18 `onPlaybackFinished` behavior is preserved (it fires before `player.play()`
is removed, so the AUDIO_PLAYBACK_COMPLETE round-trip is unaffected).

**Bug 2 — Gate blocks natural barge-in (VoiceCapture.swift):**
`onSpeechStart` only fires when `isActive = true`. `isActive` requires the hotkey to
have been pressed. True barge-in — operator speaks without pressing anything — is
currently impossible. During SPEAKING state, `isActive` is `false`, so `onSpeechStart`
never fires and audio continues playing.

**Fix:** Add `isSpeaking: Bool` field to VoiceCapture. Rising edge fires `onSpeechStart`
when `isActive || isSpeaking`. DexterClient sets `capture.isSpeaking` when SPEAKING
state arrives and clears it when SPEAKING ends.

**Bug 3 — Rust not informed (orchestrator.rs, server.rs):**
When barge-in stops Swift audio, Rust has no signal. It continues in SPEAKING state,
sends remaining TTS frames, and eventually transitions via the normal
`AUDIO_PLAYBACK_COMPLETE` path. This has two sub-problems discovered during spec review:

*Sub-problem 3a — No `entity_state` field on orchestrator:*
The original spec's `handle_barge_in` checked `self.entity_state`, but `CoreOrchestrator`
has no such field. `send_state()` is a fire-and-forget helper that sends a `ServerEvent`
via `self.tx` — it does not update any stored current-state field. The orchestrator has
no introspection into its own entity state.

Fix: Add `current_state: EntityState` field to `CoreOrchestrator`, initialized to
`EntityState::Idle`, updated at the top of `send_state()`.

*Sub-problem 3b — Session reader task is blocked while `generate_primary` runs:*
The original spec's `handle_barge_in` was to be called when a `BargIn` `ClientEvent`
arrived in the session's `select!` loop. However, `generate_primary` runs inside
`orchestrator.handle_event().await` which is in a `select!` arm body — not in an
expression position. While that arm body is executing, the `select!` loop is suspended.
`BargIn` events queue in the gRPC inbound stream but are not polled until
`handle_event().await` returns. By then, generation is complete and barge-in is moot.

*Sub-problem 3c — TTS sender is `UnboundedSender<String>`, moved into `generate_primary`:*
The original spec proposed `Arc<Mutex<Option<mpsc::Sender<String>>>>`. The actual type
is `UnboundedSender<String>`. More importantly, the sender is MOVED by value into
`generate_primary` and is not accessible from outside after that call begins. Even if
lifted to a shared slot, dropping a clone of an `UnboundedSender` does not close the
channel — all senders must be dropped. The slot approach cannot cancel the TTS channel.

**Fix for 3a/3b/3c:** Spawn `generate_primary` as a background Tokio task so the
session reader loop remains responsive to `BargIn`. This requires refactoring
`generate_primary` from `&mut self` to a standalone `async fn` accepting cloned/owned
data. `InferenceEngine` is `#[derive(Clone)]` (reqwest::Client is internally Arc, so
clone is cheap). `tx: mpsc::Sender<Result<ServerEvent, Status>>` is cloned.
Once spawned, the session `select!` loop resumes and can process `BargIn`. The
`Arc<AtomicBool>` cancel token is set by `handle_barge_in()` and polled per-token
inside the spawned function.

---

## Architecture Decisions

### Why spawn `generate_primary` instead of an alternative unary RPC

A lightweight `BargIn` unary RPC would run in a separate tonic task and could set a
shared `Arc<AtomicBool>` without blocking the session stream — this also works. It was
rejected because: (a) it requires a new proto RPC (not just a new message), which is a
larger proto contract change; (b) a new unary RPC from Swift requires a new gRPC stub
call site, adding coupling; (c) making `generate_primary` spawnable is an
improvement with broader benefits (future background generation for proactive responses,
progress reporting, etc.).

### Why `Arc<AtomicBool>` cancel token instead of `tokio::task::JoinHandle::abort()`

`JoinHandle::abort()` preemptively cancels at the next `await` point in the task —
including mid-frame HTTP reads in the token stream. This could leave the reqwest
connection in an undefined state. The cooperative `AtomicBool` approach allows
`generate_primary` to break cleanly from the token loop, flush the TTS remainder (or
not), and return. The spawned task is aborted via the JoinHandle only as a safety net
if the cooperative cancel doesn't respond (e.g. stuck await inside `generate_stream`).

### TTS channel cancellation via sender drop (still correct)

With the background-spawn approach: when barge-in sets the cancel token,
`generate_primary` breaks from the token loop and returns — dropping `tts_tx` (the
`UnboundedSender`). Since `generate_primary` is the only owner of this sender, dropping
it closes the channel and the TTS task's `recv()` returns `None` on the next poll.
No shared slot needed. The original "drop from outside" approach was wrong; the
"drop on function return after cooperative cancel" approach is correct.

### Post-generation result delivery via `oneshot` channel

`generate_primary` currently returns `(String, Option<String>)` used by
`handle_text_input` for post-processing (memory extraction, action extraction,
uncertainty follow-up). With background spawn, these results are delivered via a
`tokio::sync::oneshot::Sender` passed into the spawned function. The oneshot receiver
is stored in the session reader scope and polled as a new `select!` arm. A new
`orchestrator.handle_generation_complete(result)` method handles post-processing.

---

## File Map

| Change   | File                                           |
|----------|------------------------------------------------|
| Modified | `src/shared/proto/dexter.proto`                |
| Regenerate | `src/swift/Sources/Dexter/Bridge/generated/dexter.pb.swift` |
| Regenerate | `src/swift/Sources/Dexter/Bridge/generated/dexter.grpc.swift` |
| Modified | `src/swift/Sources/Dexter/Voice/AudioPlayer.swift`     |
| Modified | `src/swift/Sources/Dexter/Voice/VoiceCapture.swift`    |
| Modified | `src/swift/Sources/Dexter/Bridge/DexterClient.swift`   |
| Modified | `src/rust-core/src/ipc/server.rs`              |
| Modified | `src/rust-core/src/orchestrator.rs`            |

---

## 1. Proto: `BargIn` message

In `dexter.proto`, add to the `ClientEvent` oneof and add the message definition:

```protobuf
// Inside ClientEvent oneof event { ... }
BargIn barg_in = 11;   // verify this is the next available field number

// New message
message BargIn {
  string trace_id = 1;
}
```

Run `make proto` after this change to regenerate Swift and Rust bindings.

---

## 2. `AudioPlayer.swift` — lazy re-arm

### 2a. Remove `player.play()` from `stop()`

```swift
func stop() {
    queue.sync { [self] in
        let wasFinal = awaitingFinalCallback
        player.stop()
        // Removed: player.play()
        // Re-arm is now lazy — flushReadyBuffers() calls player.play() when new
        // audio arrives. This prevents TTS frames from a still-running generation
        // from playing after barge-in stopped the previous response.
        sequenceQueue.removeAll()
        pendingBufferCount    = 0
        nextExpectedSeq       = 0
        _isPlaying            = false
        awaitingFinalCallback = false
        // Phase 18 contract preserved: if is_final arrived before stop() was called,
        // fire onPlaybackFinished immediately so Rust gets AUDIO_PLAYBACK_COMPLETE.
        if wasFinal { onPlaybackFinished?() }
    }
}
```

### 2b. Add lazy re-arm to `flushReadyBuffers()`

```swift
private func flushReadyBuffers() {
    // Lazy re-arm: if stop() was called (barge-in path), the player node is stopped.
    // Re-arm it now that new audio has actually arrived. Idempotent when already playing.
    // AVAudioPlayerNode.isPlaying is thread-safe (documented by Apple).
    if !player.isPlaying { player.play() }

    while let idx = sequenceQueue.firstIndex(where: { $0.sequenceNumber == nextExpectedSeq }) {
        // ... existing buffer scheduling unchanged
    }
}
```

---

## 3. `VoiceCapture.swift` — `isSpeaking` gate

### 3a. Add `isSpeaking` field

```swift
/// Barge-in gate: set to `true` by DexterClient when EntityState.speaking arrives.
/// When set, a VAD rising edge fires onSpeechStart regardless of isActive —
/// the operator can interrupt without pressing the hotkey.
/// Cleared on any non-SPEAKING state transition.
/// Accessed exclusively on callbackQueue (same contract as isActive).
private var isSpeaking: Bool = false
```

### 3b. Public setter

```swift
/// Set the SPEAKING gate for barge-in detection. Thread-safe: dispatches to callbackQueue.
func setSpeaking(_ speaking: Bool) {
    callbackQueue.async { self.isSpeaking = speaking }
}
```

### 3c. Update `processVAD` rising-edge condition

```swift
// Before:
if isActive { onSpeechStart?() }

// After:
if isActive || isSpeaking { onSpeechStart?() }
```

---

## 4. `DexterClient.swift` — wire `isSpeaking` and send `BargIn`

### 4a. Set `isSpeaking` from entity state transitions

In the `entityState` handler:

```swift
case .entityState(let change):
    let state = EntityState(from: change.state)
    await MainActor.run {
        window.animatedEntity.entityState = state
        switch state {
        case .thinking:           window.hud.beginResponseStreaming()
        case .idle, .listening:   window.hud.scheduleAutoDismiss()
        default: break
        }
    }
    // Barge-in gate: isSpeaking = true only while entity is SPEAKING.
    capture.setSpeaking(state == .speaking)
    if state == .listening {
        capture.activate()
    }
```

### 4b. Send `BargIn` in `onSpeechStart`

```swift
capture.onSpeechStart = { [audioPlayer = self.audioPlayer, weak self, sessionID] in
    audioPlayer.stop()
    guard let client = self else { return }
    Task {
        let event = Dexter_V1_ClientEvent.with {
            $0.traceID   = UUID().uuidString
            $0.sessionID = sessionID
            $0.bargIn    = Dexter_V1_BargIn.with { $0.traceID = UUID().uuidString }
        }
        await client.send(event)
    }
}
```

`sessionID` must be added to the capture list for `onSpeechStart` (same pattern as
`onUtteranceComplete` and the existing `onSpeechStart` implementation).

---

## 5. `orchestrator.rs` — state field, background generation, barge-in handler

### 5a. New fields on `CoreOrchestrator`

```rust
/// Tracks the entity state last sent to Swift via send_state().
/// Phase 27: used by handle_barge_in() to no-op when not in SPEAKING state.
current_state: EntityState,

/// Cancel token for the background generate_primary task.
/// Set to true by handle_barge_in(); checked per-token in the generation loop.
/// Reset to false at the start of each spawned generation call.
cancel_token: Arc<AtomicBool>,

/// JoinHandle for the currently running background generation task.
/// Aborted as a safety net by handle_barge_in() if the cooperative cancel
/// token doesn't respond quickly (e.g. stuck await inside generate_stream).
generation_handle: Option<tokio::task::JoinHandle<()>>,
```

Initialize in `CoreOrchestrator::new()` / `make_orchestrator()`:
```rust
current_state:     EntityState::Idle,
cancel_token:      Arc::new(AtomicBool::new(false)),
generation_handle: None,
```

Add `use std::sync::atomic::{AtomicBool, Ordering};` to imports.

### 5b. Update `send_state()` to track current state

```rust
async fn send_state(&mut self, state: EntityState, trace_id: &str) -> Result<(), OrchestratorError> {
    self.current_state = state;   // ← add this line at the top
    // ... existing ServerEvent construction and tx.send() unchanged
}
```

### 5c. Refactor `generate_primary` to a background-spawnable standalone function

`generate_primary` currently takes `&mut self`. It must be refactored to accept
cloned/owned data so it can be passed to `tokio::spawn`.

`InferenceEngine` is `#[derive(Clone)]` — cheap clone (reqwest::Client is internally
Arc). `tx: mpsc::Sender<Result<ServerEvent, Status>>` is a standard clone.

The new signature:

```rust
/// Standalone async function — not a method on CoreOrchestrator.
/// Takes cloned engine and tx so it can be spawned as a background Tokio task.
/// The caller stores a JoinHandle and a oneshot sender to receive the result.
///
/// Returns (full_response: String, intercepted_q: Option<String>) via the
/// oneshot sender on normal completion, or drops it (signalling cancellation)
/// when the cancel token fires.
async fn generate_primary_background(
    engine:      InferenceEngine,              // cloned — cheap
    tx:          mpsc::Sender<Result<ServerEvent, Status>>,  // cloned
    model_name:  String,
    messages:    Vec<Message>,
    trace_id:    String,
    unload_after: bool,
    tts_tx:      Option<UnboundedSender<String>>,  // MOVED in; dropped on return
    cancel:      Arc<AtomicBool>,
    result_tx:   tokio::sync::oneshot::Sender<(String, Option<String>)>,
) {
    // Reset cancel token — a previous barge-in may have set it.
    cancel.store(false, Ordering::Relaxed);

    // ... existing generate_primary logic, with:
    //
    // 1. self.send_text(...)  → send_text_with(&tx, ...).await
    //    self.send_state(...) → send_state_with(&tx, ...).await
    //    self.send_vad_hint(...) → send_vad_hint_with(&tx, ...).await
    //    (These become standalone async free functions taking &mpsc::Sender)
    //
    // 2. After each token in the streaming loop, check the cancel token:
    //    if cancel.load(Ordering::Relaxed) {
    //        debug!("generate_primary: cancelled by barge-in");
    //        return;  // drop result_tx → receiver gets Err(RecvError)
    //    }
    //
    // 3. On normal completion: let _ = result_tx.send((full_response, intercepted_q));
    // 4. tts_tx is dropped when this function returns — closes the TTS channel.
}
```

The send helpers become standalone `async fn` taking `&mpsc::Sender<Result<ServerEvent, Status>>`:

```rust
async fn send_text_with(
    tx: &mpsc::Sender<Result<ServerEvent, Status>>,
    content: &str,
    is_final: bool,
    trace_id: &str,
) -> Result<(), OrchestratorError> { ... }

async fn send_state_with(
    tx: &mpsc::Sender<Result<ServerEvent, Status>>,
    state: EntityState,
    trace_id: &str,
) -> Result<(), OrchestratorError> { ... }

async fn send_vad_hint_with(
    tx: &mpsc::Sender<Result<ServerEvent, Status>>,
    frames: u32,
    trace_id: &str,
) -> Result<(), OrchestratorError> { ... }
```

### 5d. Update `handle_text_input` to spawn generation as background task

```rust
// Where generate_primary was awaited inline, replace with:
let (result_tx, result_rx) = tokio::sync::oneshot::channel();
let engine       = self.engine.clone();
let tx           = self.tx.clone();
let cancel       = self.cancel_token.clone();
let model_name_s = model_name.to_string();
let trace_id_s   = trace_id.to_string();

self.generation_handle = Some(tokio::spawn(generate_primary_background(
    engine, tx, model_name_s, messages, trace_id_s, unload_after,
    tts_tx_opt, cancel, result_tx,
)));
self.pending_generation_rx = Some(result_rx);

// Do NOT await the join handle here — return from handle_text_input.
// Post-processing happens in handle_generation_complete() when server.rs's
// select! arm fires on the oneshot receiver.
return Ok(());
```

Add `pending_generation_rx: Option<tokio::sync::oneshot::Receiver<(String, Option<String>)>>`
to the `CoreOrchestrator` struct.

### 5e. Add `handle_generation_complete()`

```rust
/// Called by server.rs when the background generation task's oneshot fires.
///
/// Performs the post-processing that was previously inline in handle_text_input
/// after the generate_primary await: memory extraction, action extraction,
/// uncertainty follow-up, session history persistence, IDLE transition.
pub async fn handle_generation_complete(
    &mut self,
    result: Result<(String, Option<String>), tokio::sync::oneshot::error::RecvError>,
    trace_id: String,
) -> Result<(), OrchestratorError> {
    let (full_response, intercepted_q) = match result {
        Ok(pair) => pair,
        Err(_)   => {
            // Generation was cancelled (barge-in dropped result_tx). No-op — state
            // already transitioned to LISTENING by handle_barge_in.
            debug!("handle_generation_complete: generation cancelled — no-op");
            return Ok(());
        }
    };

    // ... move existing post-generate_primary logic here:
    // - TTS is_final sentinel (if tts_was_active)
    // - memory extraction / extract_facts
    // - action extraction / send_action_request
    // - uncertainty follow-up (intercepted_q)
    // - session history push
    // - IDLE / ALERT / FOCUSED state transition
}
```

The `tts_join_handle` (TTS task JoinHandle from the spawned generation) must also be
communicated to `handle_generation_complete`. Simplest: wrap the oneshot result as
`GenerationResult { response: String, intercepted_q: Option<String>, tts_was_active: bool }`
and send that via the oneshot.

### 5f. Add `handle_barge_in()`

```rust
pub async fn handle_barge_in(&mut self, trace_id: String) -> Result<(), OrchestratorError> {
    // No-op if not in SPEAKING state — barge-in arrived after natural completion.
    if self.current_state != EntityState::Speaking {
        debug!(
            trace_id = %trace_id,
            state    = ?self.current_state,
            "handle_barge_in: not in SPEAKING — ignoring"
        );
        return Ok(());
    }

    info!(trace_id = %trace_id, "handle_barge_in: cancelling generation");

    // 1. Cooperative cancel: generation checks this per-token and exits cleanly.
    self.cancel_token.store(true, Ordering::Relaxed);

    // 2. Safety net abort: if the generation is stuck at a long await (e.g.
    //    waiting for the first token from a slow model), abort the task outright.
    if let Some(handle) = self.generation_handle.take() {
        handle.abort();
    }
    self.pending_generation_rx = None;

    // 3. Transition to LISTENING — operator is about to speak.
    //    tts_was_active / action_tts_active are not reset here; they are cleared
    //    automatically because handle_generation_complete returns early (RecvError)
    //    when the generation result_tx is dropped on abort.
    self.send_state(EntityState::Listening, &trace_id).await?;

    Ok(())
}
```

### 5g. Tests

- `handle_barge_in_no_ops_when_idle` — verify returns Ok without state change
- `handle_barge_in_transitions_to_listening_when_speaking` — set `current_state = Speaking`, call `handle_barge_in`, assert LISTENING ServerEvent sent
- `handle_barge_in_sets_cancel_token` — verify `cancel_token` is true after call
- `send_state_updates_current_state` — verify `current_state` field matches what was passed

---

## 6. `server.rs` — route `BargIn`, add generation result arm

### 6a. Store generation receiver in session scope

```rust
// After orchestrator construction in the reader task:
let mut pending_gen_rx: Option<tokio::sync::oneshot::Receiver<GenerationResult>> = None;
// (Or: keep it on the orchestrator as pending_generation_rx, polled via a helper)
```

The simplest approach: poll `orchestrator.pending_generation_rx` as a future in `select!`.
Since `select!` requires all arms to be known at compile time, use:

```rust
let gen_result = async {
    if let Some(ref mut rx) = orchestrator.pending_generation_rx {
        rx.await.ok()
    } else {
        std::future::pending::<Option<GenerationResult>>().await
    }
};
```

### 6b. Add `select!` arms

```rust
loop {
    tokio::select! {
        // ... existing arms unchanged ...

        gen = async {
            match orchestrator.pending_generation_rx {
                Some(ref mut rx) => rx.await.ok(),
                None             => { std::future::pending::<Option<GenerationResult>>().await }
            }
        } => {
            orchestrator.pending_generation_rx = None;
            let trace_id = new_trace_id();
            if let Err(e) = orchestrator.handle_generation_complete(gen, trace_id).await {
                error!(session = %session_trace, error = %e, "handle_generation_complete failed");
                break;
            }
        }

        msg = inbound.message() => {
            match msg {
                Ok(Some(event)) => {
                    // Route BargIn directly to handle_barge_in for low latency.
                    // All other events go through handle_event as before.
                    if let Some(client_event::Event::BargIn(barg_in)) = event.event {
                        if let Err(e) = orchestrator.handle_barge_in(barg_in.trace_id).await {
                            error!(session = %session_trace, error = %e, "handle_barge_in failed");
                            break;
                        }
                    } else {
                        if let Err(e) = orchestrator.handle_event(event).await {
                            error!(session = %session_trace, error = %e, "handle_event failed");
                            break;
                        }
                    }
                }
                // ... existing Ok(None) and Err arms unchanged
            }
        }

        // ... other existing arms unchanged
    }
}
```

The pending_generation_rx select! arm must be in the same `select!` block as inbound.message()
so BargIn from the session stream can be processed while the result future is pending.

---

## 7. Execution Order

1. Add `BargIn` to `dexter.proto` — run `make proto`
2. Modify `AudioPlayer.swift` — lazy re-arm
3. Modify `VoiceCapture.swift` — `isSpeaking` + `setSpeaking()`
4. Modify `DexterClient.swift` — `setSpeaking()` wiring + `BargIn` send in `onSpeechStart`
5. Modify `orchestrator.rs`:
   a. Add `current_state`, `cancel_token`, `generation_handle`, `pending_generation_rx` fields
   b. Update `send_state()` to track `current_state`
   c. Extract `send_text_with`, `send_state_with`, `send_vad_hint_with` as standalone helpers
   d. Refactor `generate_primary` → `generate_primary_background` (takes Engine clone, tx clone, cancel, result_tx)
   e. Update `handle_text_input` to spawn + store JoinHandle
   f. Add `handle_generation_complete()`
   g. Add `handle_barge_in()`
   h. Add tests
6. Modify `server.rs` — add generation result `select!` arm, route `BargIn` directly
7. `cd src/swift && swift build` — 0 errors, 0 warnings from project code
8. `cd src/rust-core && cargo test` — all prior tests pass + ≥4 new barge-in tests pass

---

## 8. Acceptance Criteria

### Automated

- `swift build` in `src/swift/` succeeds with 0 errors, 0 warnings from project code
- `cargo test` in `src/rust-core/` — all prior tests pass + ≥4 new barge-in tests pass

### Manual

| # | Criterion | Verification |
|---|-----------|-------------|
| 1 | Barge-in stops TTS | Ask "explain the Rust ownership model in detail" — speak over response → audio stops immediately |
| 2 | No TTS replay after stop | Verify no audio resumes after barge-in stop (Bug 1 regression) |
| 3 | No hotkey required | Barge-in fires by speaking alone, no hotkey press needed |
| 4 | Entity transitions to LISTENING | Entity visual state changes to LISTENING after barge-in, not stuck in SPEAKING |
| 5 | New utterance processed correctly | After barge-in, speak a new question — Dexter responds correctly |
| 6 | Normal non-barge-in playback unaffected | Short response plays fully with no premature stop |
| 7 | Natural end still works | Let a response play to completion — AUDIO_PLAYBACK_COMPLETE fires, entity goes IDLE normally |
| 8 | Hotkey barge-in still works | Pressing hotkey during SPEAKING still works (isActive path) |
| 9 | Multiple barge-ins | Two barge-ins in a row do not corrupt state |
| 10 | No interleaved responses | After barge-in, only the new response's text appears in HUD |

---

## 9. Key Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| `select!` futures referencing `orchestrator.pending_generation_rx` require careful ownership | Store the oneshot receiver in a local `Option` in the session scope (not on orchestrator) to avoid borrow checker issues. Orchestrator sets/clears it via a method. |
| Extracting `send_text_with` etc. as free functions duplicates code | They are simple wrappers: construct a `ServerEvent`, call `tx.send(Ok(event)).await`, map error to `OrchestratorError::ChannelClosed`. Each is ~10 lines. Duplication is minimal. |
| `generate_primary_background` spawned task panics and JoinHandle is dropped without await | `tokio::spawn` task panics are caught by the runtime; the JoinHandle fires with `Err(JoinError)`. The generation result `select!` arm receives `None` (via `ok()`), `handle_generation_complete` returns the cancelled no-op path. No session crash. |
| Cancel token left true if generation is cancelled and a new generation starts before it's reset | `generate_primary_background` resets `cancel.store(false)` as its first operation. This prevents a stale true from a previous barge-in from immediately cancelling the next generation. |
| Background noise triggers `isSpeaking`-gated barge-in (music, TV) | `VAD_ONSET_FRAMES = 2` requires 2 consecutive above-threshold frames. Brief transients are filtered. Sustained background noise at the threshold is the risk — acceptable given 4× ambient noise headroom. A dedicated barge-in threshold multiplier can be added in a follow-up if needed. |
| `handle_barge_in` called during non-voice proactive response (TTS playing from proactive path) | `current_state == Speaking` is true regardless of how SPEAKING was reached. Proactive TTS is correctly interruptible. The proactive path sets SPEAKING via `send_state` just as the regular path does. |
