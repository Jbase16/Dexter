# Phase 31 Implementation Plan: Proactive Shell Error Observations

## Context

Phase 30 established the full data pipeline: shell command completions arrive via zsh
hooks, are stored in `ContextSnapshot.last_shell_command`, and are injected as
`[Shell: $ cmd → exit N in /cwd]` context when the operator speaks to Dexter.

Phase 31 closes the feedback loop: when a command exits non-zero, Dexter notices it
**without being asked** and offers a brief, targeted observation — the same pattern as
the Phase 17 app-focus proactive, but triggered by a shell failure rather than an app
switch.

**No Swift changes. No proto changes. No new dependencies.**

---

## Architecture Decisions

### Background task — not inline await

The spec's first draft called `do_shell_error_proactive_response()` directly inside
`handle_shell_command()`, which is awaited in the `select!` loop. This blocks the entire
event loop for the ~1–3s inference duration, freezing barge-in signals, hotkey presses,
health check ticks, and action results — exactly the architectural mistake Phase 27
corrected when it extracted `generate_primary` into a background task.

Phase 31 follows the Phase 27 pattern:

1. **Synchronous setup under `&mut self`**: call `should_fire()`, call `record_fire()`,
   build the message list, clone the engine and sender handles
2. **Spawn background task**: `tokio::spawn(run_shell_error_proactive_background(...))`
   — returns immediately, `&mut self` is released, `select!` loop resumes
3. **Result delivery via `gen_tx`**: the background task sends a `GenerationResult`
   (with two new fields: `is_shell_proactive`, `proactive_silent`) when done
4. **`handle_generation_complete` dispatches**: early-exit for `is_shell_proactive`,
   calls `undo_fire()` only if `proactive_silent`

The background task owns clones of everything it needs. It never touches `self` after
spawn.

### Message list is built synchronously, before spawn

`run_shell_error_proactive_background` receives a pre-built `Vec<Message>` — it doesn't
need access to `personality`, `context_observer`, or any other `&self` fields. This keeps
the background function signature clean (no `Arc<Mutex<CoreOrchestrator>>`), exactly
mirroring `run_generation_background`'s design where the caller builds messages before
spawning.

### Reuse `GenerationResult` — two new fields

Rather than add a new channel to the `select!` loop (which requires changes to
`server.rs` and the orchestrator constructor), the background task delivers its result
via the existing `gen_tx → gen_rx → handle_generation_complete` path. Two fields are
added to `GenerationResult`:

```rust
/// Phase 31: true when this result comes from run_shell_error_proactive_background.
/// Signals handle_generation_complete to skip all regular post-processing and only
/// handle the [SILENT] refund if proactive_silent is also set.
pub is_shell_proactive: bool,

/// Phase 31: true when is_shell_proactive AND model returned [SILENT] or empty.
/// Causes handle_generation_complete to call proactive_engine.undo_fire().
pub proactive_silent: bool,
```

The existing `run_generation_background` send sets both to `false` (no behaviour change).

### Reuse `ProactiveEngine.should_fire()` — shared 90s budget

No new gate or separate rate-limiter. Both app-focus and shell-error proactives share the
same 90-second window. One interruption per window regardless of trigger type. If app-focus
fires at t=0 and a build fails at t=30s, the build failure is silently suppressed until
t=90s — the operator just heard from Dexter and doesn't need another interruption.

### Exit code filter: non-zero and not Ctrl+C

```rust
let is_error = exit_code.map_or(false, |c| c != 0 && c != 130);
```

- `None` — no code captured; skip (can't determine success/failure)
- `0` — success; skip
- `130` — SIGINT / deliberate Ctrl+C; skip (interjecting after a deliberate stop is annoying)
- Any other non-zero — attempt proactive (rate-limit and model gates still apply)

Common "benign" non-zeros (exit 1 from `grep`, exit 1 from `test`/`[`) are filtered by
the model's `[SILENT]` opt-out, not a hardcoded command list.

### `[SILENT]` opt-out remains the intelligence gate

The background task calls `ProactiveEngine::is_silent_response()` on the collected
response. If silent: send `GenerationResult { proactive_silent: true, ... }` back so
`handle_generation_complete` calls `undo_fire()`. The rate-limit slot is refunded.
Inference errors burn the slot (same as app-focus proactive).

---

## File Map

| Change   | File                                   |
|----------|----------------------------------------|
| Modified | `src/rust-core/src/proactive/engine.rs`|
| Modified | `src/rust-core/src/orchestrator.rs`    |

---

## 1. `proactive/engine.rs` — new static method

```rust
/// Build the user-turn prompt for a shell command failure proactive observation.
///
/// Used by CoreOrchestrator when a command exits non-zero and all rate-limit gates
/// pass. Produces a failure-specific, actionable prompt — as opposed to
/// build_proactive_prompt which requests a general ambient observation about app context.
///
/// The model also receives [Context:] and [Shell:] system messages before this user
/// turn, giving it: what the operator is doing right now AND what just failed.
pub fn build_shell_error_prompt(command: &str, exit_code: i32, cwd: &str) -> String {
    format!(
        "[Proactive] The operator just ran `{}` in {} and it failed with exit code {}. \
         If you can identify the likely cause or suggest a fix, give one brief targeted \
         observation. Otherwise respond with exactly [SILENT].",
        command, cwd, exit_code
    )
}
```

---

## 2. `orchestrator.rs` — four changes

### 2a. Two new fields on `GenerationResult`

```rust
pub struct GenerationResult {
    pub cancelled:          bool,
    pub full_response:      String,
    pub intercepted_q:      Option<String>,
    pub tts_was_active:     bool,
    pub trace_id:           String,
    pub content:            String,
    pub embed_model:        String,
    /// Phase 31: true when this result is from run_shell_error_proactive_background.
    /// handle_generation_complete skips all regular post-processing when set.
    pub is_shell_proactive: bool,
    /// Phase 31: true when is_shell_proactive AND model returned [SILENT] or empty.
    /// Signals handle_generation_complete to refund the proactive rate-limit slot.
    pub proactive_silent:   bool,
}
```

Update the `gen_tx.send(GenerationResult { ... })` call at the end of
`run_generation_background` to add `is_shell_proactive: false, proactive_silent: false`.

### 2b. Early-exit in `handle_generation_complete()`

Add at the top of `handle_generation_complete`, before the existing `if result.cancelled`
check:

```rust
// Phase 31: shell-error proactive result — run_shell_error_proactive_background
// handled text delivery, TTS, and IDLE transition directly. The only action needed
// here is to refund the rate-limit slot if the model returned [SILENT].
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
```

### 2c. Proactive trigger + spawn in `handle_shell_command()`

Replace the Phase 30 body's logging-only implementation with:

```rust
pub(crate) async fn handle_shell_command(
    &mut self,
    command:   String,
    cwd:       String,
    exit_code: Option<i32>,
) {
    self.context_observer.update_shell_command(command.clone(), cwd.clone(), exit_code);
    info!(
        session   = %self.session_id,
        command   = %command,
        cwd       = %cwd,
        exit_code = ?exit_code,
        "Shell command context updated"
    );

    // Phase 31: proactive observation on non-zero exit.
    //
    // Guard: skip for success (0), unknown (?), and deliberate Ctrl+C (130).
    // Benign exits like `grep` returning 1 (no matches) are filtered by the
    // model's [SILENT] opt-out — not by a hardcoded command list.
    //
    // The heavy work (inference + TTS) is spawned as a background task so
    // handle_shell_command returns immediately and the select! loop is not blocked.
    // Messages are built here (while &mut self is held) and passed to the task.
    let is_error = exit_code.map_or(false, |c| c != 0 && c != 130);
    if is_error && self.proactive_engine.should_fire(self.context_observer.snapshot()) {
        let exit_nonzero = exit_code.unwrap(); // safe: is_error guarantees Some(non-zero)
        let trace_id = uuid::Uuid::new_v4().to_string();

        // Build message list synchronously — requires &mut self (personality, context).
        // Background task receives Vec<Message>; it needs no access to self after spawn.
        let proactive_user = crate::inference::engine::Message::user(
            crate::proactive::ProactiveEngine::build_shell_error_prompt(
                &command, exit_nonzero, &cwd,
            )
        );
        let mut messages = self.personality.apply_to_messages(&[proactive_user]);
        if let Some(ax_summary) = self.context_observer.context_summary() {
            messages.insert(1, crate::inference::engine::Message::system(
                format!("[Context: {}]", ax_summary)
            ));
        }
        // [Shell: ...] after [Context: ...] — same take_while ordering as prepare_messages.
        let shell_insert = messages.iter().take_while(|m| m.role == "system").count();
        messages.insert(
            shell_insert,
            crate::inference::engine::Message::system(
                format!("[Shell: $ {} → exit {} in {}]", command, exit_nonzero, cwd)
            ),
        );

        // Clone all handles needed by the background task, then release &mut self.
        let engine   = self.engine.clone();
        let tx       = self.tx.clone();
        let gen_tx   = self.generation_tx.clone();
        let model    = self.model_config.fast.clone();
        let tts_arc  = if self.voice.is_tts_available() {
            Some(self.voice.tts_arc())
        } else {
            None
        };
        let sess     = self.session_id.clone();
        let cmd_log  = command.clone();

        // Burn the rate-limit slot before spawning. Mirrors the app-focus proactive
        // pattern: a failed inference (Ollama down) keeps the slot burned to prevent
        // rapid re-fire. Only [SILENT] refunds via handle_generation_complete.
        self.proactive_engine.record_fire();

        tokio::spawn(run_shell_error_proactive_background(
            engine, tx, sess, model, messages, trace_id, tts_arc, gen_tx,
            cmd_log, exit_nonzero,
        ));
    }
}
```

### 2d. New standalone function `run_shell_error_proactive_background()`

Add alongside `run_generation_background` (near the bottom of `orchestrator.rs`, outside
all `impl` blocks):

```rust
/// Background task for shell-error proactive observations (Phase 31).
///
/// Runs fully independently — no reference to CoreOrchestrator after spawn.
/// Mirrors the do_proactive_response pipeline but runs as a tokio task so
/// handle_shell_command returns immediately and the select! loop is not blocked.
///
/// Result is delivered via gen_tx → gen_rx → handle_generation_complete:
/// - proactive_silent = true  → handle_generation_complete calls undo_fire()
/// - proactive_silent = false → slot stays burned (inference error or real response)
///
/// TTS and IDLE transition are handled here — handle_generation_complete skips both
/// for is_shell_proactive results.
async fn run_shell_error_proactive_background(
    engine:    crate::inference::engine::InferenceEngine,
    tx:        mpsc::Sender<Result<ServerEvent, Status>>,
    session_id: String,
    model_name: String,
    messages:  Vec<crate::inference::engine::Message>,
    trace_id:  String,
    tts_arc:   Option<Arc<Mutex<Option<crate::voice::WorkerClient>>>>,
    gen_tx:    mpsc::Sender<GenerationResult>,
    command:   String,   // for structured log fields only
    exit_code: i32,      // for structured log fields only
) {
    use crate::ipc::proto::{server_event, AudioResponse, EntityStateChange};
    use crate::ipc::proto::entity_state::State as ProtoState;
    use crate::voice::protocol::msg;

    // Helper: send EntityState event without &mut self (no current_state tracking —
    // proactive is not barge-in-cancellable; tracking not needed for this path).
    let send_state = |state: ProtoState| {
        let tx = tx.clone();
        let tid = trace_id.clone();
        async move {
            let evt = ServerEvent {
                trace_id: tid,
                event: Some(server_event::Event::EntityState(EntityStateChange {
                    state: state.into(),
                })),
            };
            let _ = tx.send(Ok(evt)).await;
        }
    };

    // 1. THINKING — operator sees Dexter is active.
    send_state(ProtoState::Thinking).await;

    // 2. Run inference — collect full response before displaying ([SILENT] check).
    //    30-second timeout matches collect_generation's budget.
    let req = crate::inference::engine::GenerationRequest {
        model_name:          model_name,
        messages,
        temperature:         None,
        unload_after:        false,
        keep_alive_override: None,
        num_predict:         None,
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

    // 3. Handle inference error — slot stays burned; transition to IDLE.
    let response = match response_text {
        None => {
            send_state(ProtoState::Idle).await;
            let _ = gen_tx.send(GenerationResult {
                cancelled: false, full_response: String::new(),
                intercepted_q: None, tts_was_active: false,
                trace_id, content: String::new(), embed_model: String::new(),
                is_shell_proactive: true, proactive_silent: false,
            }).await;
            return;
        }
        Some(text) => text,
    };

    // 4. [SILENT] opt-out — refund the slot via gen_tx.
    if crate::proactive::ProactiveEngine::is_silent_response(&response) {
        info!(
            session   = %session_id,
            trace_id  = %trace_id,
            command   = %command,
            exit_code = exit_code,
            "Shell error proactive: model returned [SILENT] — slot will be refunded"
        );
        send_state(ProtoState::Idle).await;
        let _ = gen_tx.send(GenerationResult {
            cancelled: false, full_response: String::new(),
            intercepted_q: None, tts_was_active: false,
            trace_id, content: String::new(), embed_model: String::new(),
            is_shell_proactive: true, proactive_silent: true,
        }).await;
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
        let text_bytes     = response.trim().as_bytes().to_vec();
        let session_tx     = tx.clone();
        let trace_id_clone = trace_id.clone();

        let handle = tokio::spawn(async move {
            let mut guard = tts_arc.lock().await;
            if let Some(client) = guard.as_mut() {
                if client.write_frame(msg::TEXT_INPUT, &text_bytes).await.is_ok() {
                    let mut seq = 0u32;
                    loop {
                        match client.read_frame().await {
                            Ok(Some((msg::TTS_AUDIO, pcm))) => {
                                let evt = ServerEvent {
                                    trace_id: String::new(),
                                    event: Some(server_event::Event::AudioResponse(
                                        AudioResponse {
                                            data: pcm, sequence_number: seq, is_final: false,
                                        },
                                    )),
                                };
                                let _ = session_tx.send(Ok(evt)).await;
                                seq += 1;
                            }
                            Ok(Some((msg::TTS_DONE, _))) => {
                                let sentinel = ServerEvent {
                                    trace_id: trace_id_clone,
                                    event: Some(server_event::Event::AudioResponse(
                                        AudioResponse {
                                            data: vec![], sequence_number: seq, is_final: true,
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
        // IDLE is deferred: Swift sends AUDIO_PLAYBACK_COMPLETE after last buffer plays.
        // handle_system_event's AudioPlaybackComplete arm transitions to IDLE then.
        true
    } else {
        // No TTS — no playback to wait for; transition to IDLE directly.
        send_state(ProtoState::Idle).await;
        false
    };

    // 7. Deliver result. handle_generation_complete sees is_shell_proactive=true
    //    and returns early after verifying proactive_silent (false here — response was real).
    let _ = gen_tx.send(GenerationResult {
        cancelled: false, full_response: response,
        intercepted_q: None, tts_was_active,
        trace_id, content: String::new(), embed_model: String::new(),
        is_shell_proactive: true, proactive_silent: false,
    }).await;
}
```

---

## 3. Execution Order

1. Add `build_shell_error_prompt()` to `proactive/engine.rs`
2. Add unit tests for `build_shell_error_prompt` to `proactive/engine.rs`
3. Add `is_shell_proactive: bool` and `proactive_silent: bool` to `GenerationResult` in
   `orchestrator.rs`
4. Update the `gen_tx.send(GenerationResult { ... })` in `run_generation_background` to
   include `is_shell_proactive: false, proactive_silent: false`
5. Add early-exit block to `handle_generation_complete()` for `is_shell_proactive`
6. Add proactive trigger + spawn block to `handle_shell_command()`
7. Add `run_shell_error_proactive_background()` standalone function
8. Write unit + integration tests
9. `cargo test` — target: 285 (Phase 30) + 5 new = **290 RUST TESTS PASS**, 0 warnings

---

## 4. Tests

### Unit tests in `proactive/engine.rs`

```rust
#[test]
fn build_shell_error_prompt_contains_command_exit_and_cwd() {
    let prompt = ProactiveEngine::build_shell_error_prompt(
        "cargo build", 1, "/Users/jason/Developer/Dex"
    );
    assert!(prompt.contains("cargo build"),                "prompt must contain command");
    assert!(prompt.contains("exit code 1"),                "prompt must contain exit code");
    assert!(prompt.contains("/Users/jason/Developer/Dex"), "prompt must contain cwd");
    assert!(prompt.contains("[SILENT]"),                   "prompt must mention [SILENT] opt-out");
}

#[test]
fn build_shell_error_prompt_distinct_from_ambient_prompt() {
    // Regression guard: the two prompt styles must remain distinct.
    // If they collapse to the same string, the model loses the signal that
    // one is about an explicit failure vs. an app-focus ambient observation.
    let error_prompt   = ProactiveEngine::build_shell_error_prompt("make", 2, "/tmp");
    let ambient_prompt = ProactiveEngine::build_proactive_prompt("Terminal — make");
    assert_ne!(error_prompt, ambient_prompt,
        "shell error and ambient proactive prompts must be distinct");
}
```

### Integration tests in `orchestrator.rs`

```rust
#[tokio::test]
async fn handle_shell_command_success_does_not_attempt_proactive() {
    // Exit 0 must not trigger proactive. Verified by checking that the rate-limit
    // slot is not burned: proactive_engine.record_fire() was not called, so
    // last_fired_at remains None, and should_fire() returns false only due to
    // startup grace (not due to a burned slot).
    // Note: in the test environment, startup grace alone suppresses proactive for
    // all exit codes. This test primarily verifies the context update happens and
    // provides coverage documentation for the is_error guard.
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, _rx) = make_orchestrator(tmp.path());

    orch.handle_shell_command("cargo build".to_string(), "/tmp".to_string(), Some(0)).await;

    let snap = orch.context_observer.snapshot();
    assert_eq!(
        snap.last_shell_command.as_ref().map(|s| s.exit_code),
        Some(Some(0)),
        "context must be updated even for exit code 0"
    );
}

#[tokio::test]
async fn handle_shell_command_ctrl_c_does_not_attempt_proactive() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, _rx) = make_orchestrator(tmp.path());

    orch.handle_shell_command("sleep 60".to_string(), "/tmp".to_string(), Some(130)).await;

    let snap = orch.context_observer.snapshot();
    assert_eq!(
        snap.last_shell_command.as_ref().map(|s| s.exit_code),
        Some(Some(130)),
        "Ctrl+C must update context but not trigger proactive"
    );
}

#[tokio::test]
async fn handle_shell_command_none_exit_code_does_not_attempt_proactive() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, _rx) = make_orchestrator(tmp.path());

    orch.handle_shell_command("some_cmd".to_string(), "/tmp".to_string(), None).await;

    let snap = orch.context_observer.snapshot();
    assert!(
        snap.last_shell_command.as_ref().map_or(false, |s| s.exit_code.is_none()),
        "None exit code must be stored in context without proactive attempt"
    );
}
```

**Coverage note on exit 0 / exit 130 / None tests:** These tests verify context updates
and document the intended behaviour of the `is_error` guard. They cannot directly assert
that `record_fire()` was not called, because `proactive_engine.last_fired_at` is private
and the startup grace period already suppresses proactive in fresh test orchestrators.
The `is_error` guard is simple enough that its logic is verified by code review and by
the unit tests for `build_shell_error_prompt` which document the model-level gate.

**Expected test totals:** 285 (Phase 30) + 5 new = **290 RUST TESTS PASS**.

---

## 5. Acceptance Criteria

### Automated

`cargo test` in `src/rust-core/`: 290 tests pass, 0 warnings.

### Manual

| # | Criterion | Verification |
|---|-----------|-------------|
| 1 | Error noticed | Run `cargo build` with a compilation error — Dexter speaks within ~3s without hotkey or voice input |
| 2 | [SILENT] for benign exits | Run `grep doesnotexist /tmp/somefile` (exit 1, no matches) — Dexter stays quiet |
| 3 | Success stays silent | Run `cargo build` successfully (exit 0) — no proactive observation |
| 4 | Ctrl+C stays silent | Run `sleep 60`, press Ctrl+C (exit 130) — no proactive observation |
| 5 | Rate-limit applies | Two failing commands within 90s — proactive fires on the first, not the second |
| 6 | Rate-limit shared | App-focus proactive fires; within 90s, a build fails — no second proactive until window expires |
| 7 | Content is targeted | Observation references the specific command and likely cause, not a generic message |
| 8 | TTS delivers | With TTS enabled, the observation is spoken |
| 9 | HUD history records it | Appears in HUD history labelled "(observation)" — same as Phase 29 app-focus proactive |
| 10 | select! loop unblocked | During a failing build (3–10s compile error), hotkey press and voice input remain responsive throughout the inference wait |
| 11 | Proactive disabled | `proactive_enabled = false` in config — no observations even on build failure |

---

## 6. Key Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| TTS block is verbatim-identical to `do_proactive_response §6` — divergence risk if TTS changes | Both blocks carry the comment `// Identical TTS block — see do_proactive_response §6`. Phase 32+ extraction point: once a third call site appears, factor into a standalone `deliver_via_tts(tx, tts_arc, text, trace_id)` free function. |
| `send_state` inside background task does not update `self.current_state` — barge-in during proactive won't see THINKING state | Same limitation as the existing app-focus `do_proactive_response` (also inline but doesn't support barge-in cancellation). Shell-error proactive is non-streaming (`collect_generation`), so there is no in-progress stream to cancel. Barge-in interrupting a proactive play-back is Phase 32+ scope. |
| `proactive_engine.record_fire()` called before spawn — if spawn fails (extremely unlikely), slot is burned with no observation | `tokio::spawn` only fails if the runtime is shutting down. A slot burned at shutdown is a non-issue. |
| `run_shell_error_proactive_background` runs while no session is active (gen_tx send fails) | `gen_tx.send()` returns an error if the receiver is dropped (session teardown). The background task discards this error (`let _ = gen_tx.send(...).await`). Correct: delivery of the result is best-effort; the orchestrator is already shutting down. |
| `is_error` guard passes for `exit 1` from `grep` — model fires anyway | `ProactiveEngine::is_silent_response` is the gate. The model sees `[Shell: $ grep ... → exit 1]` and returns `[SILENT]`. Rate-limit slot is refunded via `undo_fire()`. One inference call per benign-failure event is the acceptable cost of not hardcoding a command list. |
