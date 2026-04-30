# Phase 23 — Voice Pipeline Quality (Retroactive)

## Version 1.0 — 2026-03-16 (Written retroactively; Phase 23 is complete)

**Status:** ✅ COMPLETE — 251 Rust, 20 Python, Swift clean
**Depends on:** Phase 22 complete (retrieval pipeline hardening, 251 Rust tests)
**Session:** 2026-03-16-session-023

---

## Executive Summary

Phase 23 eliminates five classes of voice pipeline failure that make Dexter feel
broken during real-world use. Every fix was discovered during live testing —
pressing the hotkey, speaking a natural query ("What's the weather like?"), and
watching the system fail in different ways.

The session started with a successful end-to-end voice round-trip (transcript at
100% confidence), but the response path exposed cascading problems: a 57-second
browser action that froze the orchestrator, total silence after the action failed,
the entity stuck in LISTENING after an empty transcript, 60 per-frame diagnostic
logs flooding stderr, and the model choosing catastrophically slow browser actions
for simple queries.

Phase 23 fixes all five. The voice pipeline is now functionally correct end-to-end.

---

## Problem Inventory

| # | Problem | Discovery | Severity |
|---|---------|-----------|----------|
| 1 | **Silent failure after action execution** — browser/tool actions complete or fail with zero operator feedback. No speech, no text, nothing. | Browser action for "What's the weather like?" failed after 57s. Dexter went completely silent. | Critical — operator has no idea what happened |
| 2 | **Entity stuck in LISTENING after empty STT transcript** — when the STT worker returns no segments (ambient noise, false VAD trigger), the entity never transitions out of LISTENING. | Tested with short ambient sounds that triggered VAD but produced no speech. Entity stayed in LISTENING indefinitely. | Critical — system appears hung |
| 3 | **qwen3:8b hidden thinking chains** — qwen3 models default to generating a `<think>...</think>` reasoning chain before any visible output. Ollama filters these tokens from the stream, so the model appears completely silent for 30s–5min. | First qwen3:8b query produced no output for ~3 minutes. | Critical — unusable latency |
| 4 | **Embed model cold-load timeout** — `mxbai-embed-large` takes 30–45s to load from disk on first use. The embed call in `recall_relevant()` hits the 60s request timeout and memory recall is silently skipped for the first query. | First voice query after fresh start had no memory recall despite stored facts. | High — first-query degradation |
| 5 | **VoiceCapture diagnostic log noise** — 60 per-frame log lines flooding stderr during every VAD-active period. | Visible in Xcode console during any voice interaction. | Low — cosmetic but masks real errors |
| 6 | **Model choosing browser actions for simple queries** — the LLM routes "What's the weather like?" to a browser action (navigate to weather.com), blocking the entire orchestrator for 30–60s. | Live test: simple weather question triggered a 57-second blocking browser action. | High — poor model judgment causes cascading failures |

---

## Solutions Implemented

### 1. `speak_action_feedback` — Audible Action Result/Error via TTS

**Problem:** After `ActionOutcome::Completed` or `ActionOutcome::Rejected`, the
orchestrator logged the result and transitioned to IDLE. No text was sent to Swift.
No TTS was generated. The operator heard nothing.

**Root cause analysis:** The action execution block (step 9 of `handle_text_input`)
handled the `PendingApproval` path correctly (sends `ActionRequest` dialog + ALERT
state) but treated `Completed` and `Rejected` as fire-and-forget. The original
Phase 8 implementation assumed the operator would see action results in a UI panel
that was never built.

**Solution:** New orchestrator method `speak_action_feedback(text, trace_id)`:

```rust
async fn speak_action_feedback(
    &mut self,
    text:     &str,
    trace_id: &str,
) -> Result<bool, OrchestratorError>
```

**How it works:**

1. Always sends `text` to Swift UI via `send_text(text, true, trace_id)` — visible
   even when TTS is unavailable or the operator has headphones off.

2. If TTS is available: spawns a Tokio task that drives the full TTS delivery
   sequence: `TEXT_INPUT` → read `TTS_AUDIO` frames → forward as `AudioResponse`
   → send `is_final` sentinel on `TTS_DONE`.

3. Returns `bool` indicating whether TTS audio was dispatched.

**Where it's called:**

- `handle_text_input` step 9, `Completed` arm — speaks action output or "Done."
- `handle_text_input` step 9, `Rejected` arm — speaks "Sorry, I wasn't able to
  complete that action."
- `handle_action_approval`, `Completed` arm — speaks output or "Done."
- `handle_action_approval`, `Rejected` arm — speaks "Action cancelled."

**Pattern origin:** Same TTS delivery loop as `do_proactive_response` (Phase 17).
Extracted as a reusable helper because action feedback follows the identical
TEXT_INPUT → TTS_AUDIO → TTS_DONE → is_final pattern.

### 2. `action_tts_active` Flag — IDLE Gate for Action Feedback Audio

**Problem:** When `speak_action_feedback` dispatches TTS audio, the is_final
sentinel triggers Swift's `AUDIO_PLAYBACK_COMPLETE` round-trip which drives IDLE.
But the existing IDLE gate at the end of `handle_text_input` only checked
`tts_was_active` (response TTS) and `action_is_pending` (destructive approval).
Without a third check, the orchestrator could send IDLE directly *before* the
action feedback audio finished playing.

**Solution:** New boolean `action_tts_active`, set when `speak_action_feedback`
returns `true`. Added to the IDLE gate:

```rust
// Before (Phase 19):
if !action_is_pending && !tts_was_active {
    self.send_state(EntityState::Idle, &trace_id).await?;
}

// After (Phase 23):
if !action_is_pending && !tts_was_active && !action_tts_active {
    self.send_state(EntityState::Idle, &trace_id).await?;
}
```

When `action_tts_active` is true, IDLE is deferred to the `AUDIO_PLAYBACK_COMPLETE`
round-trip, which fires after the last action feedback buffer finishes playing.

### 3. Empty Transcript → `AUDIO_PLAYBACK_COMPLETE` Reset

**Problem:** When the STT worker returns zero non-empty segments (ambient noise,
false VAD trigger, brief tap on the desk), Swift receives an empty transcript and
the old code simply returned early. The orchestrator was in LISTENING state (sent
on hotkey press) with nothing to drive a transition out of it. The entity stayed
stuck in LISTENING until the next hotkey press.

**Root cause:** The state machine had no "cancel" path for LISTENING. The only
transitions out of LISTENING were: TextInput (starts THINKING) and
AudioPlaybackComplete (which requires TTS to have played). With an empty
transcript, neither fires.

**Solution:** Swift sends `AudioPlaybackComplete` as a generic "reset to Idle"
signal when the transcript is empty:

```swift
guard !transcript.isEmpty else {
    print("[DexterClient] Empty transcript — resetting entity to idle")
    let resetEvent = Dexter_V1_ClientEvent.with {
        $0.traceID   = UUID().uuidString
        $0.sessionID = sessionID
        $0.systemEvent = Dexter_V1_SystemEvent.with {
            $0.type = .audioPlaybackComplete
        }
    }
    await self?.send(resetEvent)
    return
}
```

**Why AudioPlaybackComplete and not a new event type:**

The Rust handler for `AUDIO_PLAYBACK_COMPLETE` already has the right behavior:
transition to IDLE unless `action_awaiting_approval` is true (which preserves
ALERT if an action dialog is open). Reusing the existing event avoids a proto
change, a Swift rebuild, and a new handler — for identical semantics.

**Guard implementation (orchestrator.rs lines 1216–1235, added Phase 19):**

```rust
Ok(SystemEventType::AudioPlaybackComplete) => {
    // Phase 19: if an action is awaiting operator approval, stay in ALERT.
    // handle_action_approval() will clear the flag and send IDLE when the
    // operator responds. Sending IDLE here would cancel the ALERT state.
    if self.action_awaiting_approval {
        info!(... "TTS playback complete — action pending approval, remaining in ALERT");
    } else {
        info!(... "TTS playback complete — transitioning to IDLE");
        self.send_state(EntityState::Idle, &trace_id).await?;
    }
}
```

**Tested by:**
- `audio_playback_complete_skips_idle_when_action_pending` (line 2548)
- `audio_playback_complete_sends_idle_after_action_flag_cleared` (line 2573)

Both tests verify the guard lifecycle: flag set → event suppressed, flag
cleared → event sends IDLE. The empty-transcript path from Phase 23 hits
this same handler and is correctly guarded — a destructive action awaiting
approval will not be silently cancelled.

### 4. `think: false` — Disable qwen3 Hidden Reasoning

**Problem:** qwen3 models default to hybrid thinking mode: before generating any
visible output, they produce a `<think>...</think>` chain. Ollama filters these
tokens from the response stream, so from the orchestrator's perspective the model
is completely silent for the entire reasoning phase — typically 30 seconds to
5+ minutes on CPU-only inference.

**Solution:** New field on `OllamaChatRequest`:

```rust
#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    // ... existing fields ...
    /// Disable qwen3's hybrid thinking mode (Phase 23).
    think: bool,
}
```

Set to `false` in both `generate_stream()` and `unload_model()`. Harmless for
non-qwen3 models — Ollama ignores unknown top-level fields.

**Impact:** qwen3:8b responses now begin within 2–4 seconds instead of 30s–5min.

### 5. Embed Model Warm-Up + `keep_alive: "10m"`

**Two independent changes, same goal: keep mxbai-embed-large ready.**

**A. Startup warm-up — `warm_up_embed()`:**

The embed model takes 30–45s to load from disk on first use. Without a warm-up,
the first `recall_relevant()` call's embed request hits the 60-second timeout
and memory recall is silently skipped.

```rust
pub fn warm_up_embed(&self) {
    let engine     = self.engine.clone();
    let embed_model = self.model_config.embed.clone();
    tokio::spawn(async move {
        info!("Warming up embed model: {embed_model}");
        let req = EmbeddingRequest {
            model_name: embed_model.clone(),
            input:      "warmup".to_string(),
        };
        match engine.embed(req).await {
            Ok(_)  => info!("Embed model warm: {embed_model}"),
            Err(e) => warn!(error = %e, "Embed warmup failed — first query may be slow"),
        }
    });
}
```

Called in `ipc/server.rs` after `start_voice()`, during session setup. The embed
model loads in the background while TTS and STT workers are starting. By the time
the operator speaks their first query (~10s after launch), the model is resident.

**B. `keep_alive: "10m"` on embed requests:**

Ollama's default `keep_alive` is 5 minutes. In a normal conversation, the gap
between voice queries can exceed 5 minutes (operator is reading, typing, etc.).
Without an extended TTL, the embed model gets evicted and the next query pays the
full 30–45s cold-load penalty again.

```rust
let body = OllamaEmbedRequest {
    model:      &req.model_name,
    input:      &req.input,
    keep_alive: "10m",
};
```

10 minutes is long enough to survive a normal conversation gap without consuming
GPU/RAM indefinitely. Embed models are small (~0.7GB) — the VRAM cost is negligible.

**C. Request timeout: 60 seconds.**

The reqwest client timeout for embed requests was increased to 60 seconds to
accommodate cold-load scenarios where the warm-up hasn't completed yet.

### 6. Persistent STT Worker with `TRANSCRIPT_DONE` Sentinel

**Problem:** Before Phase 23, each STT call spawned a new Python process, loaded
the Whisper model (~8s), processed audio, then exited. The model load time alone
made voice interaction feel completely broken.

**Solution:** Persistent STT worker with a new binary protocol sentinel.

**Architecture:**

```
Session start:
  CoreService::new() → tokio::spawn → WorkerClient::spawn(Stt)
  STT worker: load model → write_handshake("stt") → enter read loop

Each utterance:
  stream_audio() → acquire mutex → forward AUDIO_CHUNK frames
                 → send AUDIO_END
                 → read TRANSCRIPT frames
                 → read TRANSCRIPT_DONE (0x11) → release mutex
                 → worker stays alive for next utterance
```

**New protocol message: `MSG_TRANSCRIPT_DONE` (0x11)**

Added to both `src/rust-core/src/voice/protocol.rs` and
`src/python-workers/workers/protocol.py`. Sent by the STT worker after all
TRANSCRIPT frames for one utterance. Signals end-of-utterance without closing
the worker.

**Handshake ordering (critical):**

The Python worker calls `write_handshake()` AFTER loading the heavyweight model:

```python
model = WhisperModel("base.en", device="cpu", compute_type="int8")
write_handshake(sys.stdout.buffer, "stt")  # Signal ready AFTER model load
```

Rust considers the worker ready only after receiving the handshake. Loading
after the handshake risks pipe-buffer overflow: Rust might start sending audio
chunks before the Python process has finished model loading and entered its
read loop.

**Fallback:** If the pre-warm hasn't completed when `stream_audio()` is called
(user speaks very quickly after launch), the method falls back to on-demand spawn.
If the worker dies mid-utterance, the slot is cleared for clean re-spawn on the
next call.

**Mutex hold duration and health check design:**

The `Arc<Mutex<Option<WorkerClient>>>` is held for the full utterance duration
(potentially 10–30s) — through all `AUDIO_CHUNK` writes, `AUDIO_END`, and all
`TRANSCRIPT` reads until `TRANSCRIPT_DONE`. This is deliberate: Swift serializes
utterances (one `stream_audio` call at a time), so the mutex exists to serialize
the startup pre-warm against the first real call, not to handle concurrent
utterances.

**There is no active health check for the STT worker.** This is a deliberate
asymmetry with the TTS worker (which has periodic health checks via
`voice_health_check()`). The design rationale:

- **TTS worker** is used asynchronously by multiple features (response TTS,
  proactive TTS, action feedback TTS). A dead TTS worker might not be discovered
  for minutes if no TTS request happens to fire. Active health checks detect
  silent death between uses.

- **STT worker** is used synchronously and exclusively by `stream_audio()`.
  Every use exercises the full protocol (write frames, read frames). A dead
  worker is discovered immediately on the next `stream_audio()` call via
  `write_frame` or `read_frame` returning `Err` → `still_alive = false` →
  `*guard = None` → re-spawn on next call. Active health checks add complexity
  for zero benefit because the worker is always exercised on use.

**Mutex contention sites (exhaustive — 2 total):**

| Site | When | Duration | Contention possible? |
|------|------|----------|---------------------|
| `stt_warm.lock().await` (server.rs:61) | Once at startup | ~8s (model load) | Only with first `stream_audio` call, if operator speaks during startup |
| `stt_arc.lock().await` (server.rs:262) | Each utterance | 1–30s | No — Swift serializes utterances; pre-warm completes before first query |

No other code path acquires the STT mutex. No health check targets it. The
passive failure detection model is correct for a single-consumer resource.

### 7. VoiceCapture Diagnostic Logging Reduction

**Problem:** During every VAD-active period, `VoiceCapture.swift` logged RMS and
threshold values for every frame — 60 lines of output per second of speech.
This flooded Xcode's console and masked real error messages.

**Solution:** Reduced to exactly 2 summary log lines per capture session:

```swift
diagFrameCount += 1
if diagFrameCount == 1 {
    print(String(format: "[VoiceCapture] First frame received — rms=%.5f threshold=%.5f",
                 rms, Constants.VAD_ENERGY_THRESHOLD))
} else if diagFrameCount == 60 {
    print(String(format: "[VoiceCapture] Audio running normally (60 frames) — rms=%.5f",
                 rms))
}
```

Frame 1 confirms audio is arriving. Frame 60 confirms the pipeline is running
normally. Everything between is noise.

### 8. Browser Action Guidance in Personality Prompt

**Problem:** When asked "What's the weather like?", the LLM generated a browser
action to navigate to weather.com. This triggered a 57-second blocking operation
that froze the entire orchestrator. The model was making a reasonable decision
(it needs live data for weather) but the cost was catastrophic.

**Solution:** Added behavioral guidance to `config/personality/default.yaml`:

```yaml
Action cost and when to skip them:
  - Browser actions are SLOW — they block the entire system for 15–60 seconds.
    Only use browser when the operator EXPLICITLY asks you to open or search the web,
    or when you genuinely cannot answer without live data AND the delay is worth it.
  - Real-time queries (weather, stock prices, current news, sports scores) you CANNOT
    answer from training data: say so directly and honestly rather than attempting a
    browser action. Example: "I don't have live weather data. You could check Weather.app
    or say 'open weather.com' if you want me to browse it."
  - Shell and file actions are fast. Browser is always last resort.
```

This is a personality-layer fix, not an architectural one. The model learns that
honesty ("I don't have live data") is preferable to a 60-second blocking action.
Phase 24 addresses the underlying architectural problem (blocking actions) directly.

---

## File Map

| Change   | File | Description |
|----------|------|-------------|
| Modified | `src/rust-core/src/orchestrator.rs` | `speak_action_feedback` method, `action_tts_active` flag, `warm_up_embed`, action feedback in `handle_text_input` + `handle_action_approval` |
| Modified | `src/rust-core/src/inference/engine.rs` | `think: false` field on `OllamaChatRequest`, `keep_alive: "10m"` on `OllamaEmbedRequest`, 60s request timeout |
| Modified | `src/rust-core/src/ipc/server.rs` | Persistent STT worker (`Arc<Mutex<Option<WorkerClient>>>`), pre-warm task, `warm_up_embed()` call, `stream_audio()` rewrite for persistent mode |
| Modified | `src/rust-core/src/voice/protocol.rs` | `TRANSCRIPT_DONE: u8 = 0x11` |
| Modified | `src/python-workers/workers/protocol.py` | `MSG_TRANSCRIPT_DONE = 0x11` |
| Modified | `src/python-workers/workers/stt_worker.py` | Persistent mode: handshake-after-load, utterance loop, `TRANSCRIPT_DONE` sentinel |
| Modified | `src/swift/Sources/Dexter/Bridge/DexterClient.swift` | Empty transcript → `AudioPlaybackComplete` reset |
| Modified | `src/swift/Sources/Dexter/Voice/VoiceCapture.swift` | `diagFrameCount`, reduced to 2 summary log lines |
| Modified | `config/personality/default.yaml` | "Action cost and when to skip them" section |

---

## Test Impact

| Suite | Before (Phase 22) | After (Phase 23) | Delta |
|-------|--------------------|-------------------|-------|
| Rust (`cargo test`) | 251 | 251 | +0 (no new test files; existing test updated) |
| Python (`make test-python`) | 19 | 20 | +1 (TRANSCRIPT_DONE protocol test) |
| Swift (`swift build`) | Clean | Clean | No warnings from project code |

**Test fix required:** `action_approval_clears_action_awaiting_approval_flag`

The new `speak_action_feedback` call in `handle_action_approval` sends a
`TextResponse` event before `EntityStateChange(Idle)`. The test originally
expected IDLE as the first event after approval:

```rust
// Before (failed):
let next = rx.try_recv().unwrap().unwrap();
assert!(matches!(next.event, Some(server_event::Event::EntityState(_))));

// After (passes):
// Drain TextResponse events, then assert EntityStateChange(Idle)
loop {
    let next = rx.try_recv().unwrap().unwrap();
    match &next.event {
        Some(server_event::Event::TextResponse(_)) => continue,
        Some(server_event::Event::EntityState(change)) => {
            assert_eq!(change.state, EntityState::Idle as i32);
            break;
        }
        other => panic!("Unexpected event: {:?}", other),
    }
}
```

---

## Critical Implementation Patterns

### Pattern: `speak_action_feedback` TTS Delivery

Same TTS delivery loop as `do_proactive_response` (Phase 17):

```
TEXT_INPUT → [TTS_AUDIO frames → AudioResponse(is_final=false)] → TTS_DONE → AudioResponse(is_final=true)
```

The `is_final=true` sentinel arms Swift's `awaitingFinalCallback`. When the last
buffer finishes playing, `onPlaybackFinished` fires → `AUDIO_PLAYBACK_COMPLETE` →
Rust → IDLE.

### Pattern: `AUDIO_PLAYBACK_COMPLETE` as Generic "Go to Idle"

This event was designed for TTS playback completion (Phase 18). Phase 23 reuses
it as a generic reset signal. The Rust handler's `action_awaiting_approval` guard
ensures ALERT state is preserved when an action dialog is open. This makes the
reuse safe — the handler already distinguishes between "playback done" and
"cancel alert."

### Pattern: Persistent Worker with Handshake-After-Load

```
Python:  load_model()  →  write_handshake()  →  read_loop()
Rust:    spawn()       →  read_handshake()    →  worker is ready
```

The handshake gate ensures Rust never sends data to a worker that's still loading.
This prevents pipe-buffer overflow on long model loads.

### Pattern: `think: false` for qwen3

Added as a top-level field on the Ollama chat request body, not in `options`.
Harmless for non-qwen3 models (Ollama ignores unknown fields). Set in both
`generate_stream()` and `unload_model()` — the unload path also sends a chat
request with `keep_alive: "0"`.

---

## What Phase 23 Did NOT Fix (→ Phase 24)

Phase 23 made the voice pipeline **functionally correct** but left three
architectural problems unresolved:

1. **Actions still block the event loop** — `action_engine.submit()` is awaited
   inline. While we now speak the error/result afterward and guide the model to
   avoid browser actions, a legitimate browser action (operator explicitly asks)
   still freezes the pipeline for its full duration.

2. **Entity stuck in THINKING during long actions** — even after TTS plays,
   `tts_was_active = true` defers IDLE to `AUDIO_PLAYBACK_COMPLETE`, which queues
   behind the blocking `submit()` call. The orb shows THINKING for the entire
   action duration.

3. **Inference latency floor** — qwen3:8b with `think: false` is 2–4s for first
   token on warm model. The full pipeline (VAD→STT→routing→inference→TTS) is
   5–10s end-to-end.

These are addressed in Phase 24 (Concurrent Interaction Pipeline & Latency
Elimination).

---

## Acceptance Criteria (All Passing)

### Automated

| # | Criterion | Status |
|---|-----------|--------|
| 1 | `cargo test` — 251 tests, 0 failures, 0 warnings | ✅ |
| 2 | `make test-python` — 20 tests, 0 failures | ✅ |
| 3 | `swift build` — 0 warnings from project code | ✅ |

### Manual (Verified 2026-03-16)

| # | Criterion | Status |
|---|-----------|--------|
| 1 | Voice query transcribed at 100% confidence | ✅ ("What's the weather like?") |
| 2 | Action failure → Dexter speaks error message | ✅ |
| 3 | Action success → Dexter speaks result or "Done." | ✅ |
| 4 | Empty transcript → entity returns to IDLE | ✅ |
| 5 | qwen3:8b responds within 5s (warm model) | ✅ |
| 6 | First query after fresh start has memory recall | ✅ (embed warm-up) |
| 7 | VoiceCapture logs ≤ 2 lines per capture session | ✅ |
| 8 | Weather query → honest "no live data" response | ✅ (personality prompt) |
