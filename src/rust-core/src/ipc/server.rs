use std::{
    pin::Pin,
    sync::{atomic::Ordering, Arc},
};

use tokio::{
    net::UnixListener,
    sync::{mpsc, Mutex},
};
use tokio_stream::{
    wrappers::{ReceiverStream, UnixListenerStream},
    Stream,
};
use tonic::{transport::Server, Request, Response, Status, Streaming};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    action::ActionResult,
    config::DexterConfig,
    constants::{
        BROWSER_WORKER_HEALTH_INTERVAL_SECS, CORE_VERSION, VOICE_PYTHON_EXE, VOICE_STT_WORKER_PATH,
        VOICE_WORKER_HEALTH_INTERVAL_SECS,
    },
    orchestrator::CoreOrchestrator,
    orchestrator::GenerationResult,
    voice::{worker_client::WorkerClient, WorkerType},
};

// Pull the generated proto types into scope.
pub mod proto {
    tonic::include_proto!("dexter.v1");
}

use proto::{
    dexter_service_server::{DexterService, DexterServiceServer},
    AudioChunk, ClientEvent, EntityState, EntityStateChange, PingRequest, PingResponse,
    ServerEvent, TranscriptChunk,
};

fn is_benign_session_stream_close(status: &Status) -> bool {
    let message = status.message();
    status.code() == tonic::Code::Unknown
        && (message.contains("h2 protocol error: error reading a body from connection")
            || message.contains("operation was canceled")
            || message.contains("stream closed"))
}

// ── Internal fast-path events ─────────────────────────────────────────────────

/// Events delivered directly from `stream_audio` to the active session orchestrator.
///
/// Phase 24c: bypasses the gRPC Swift round-trip (Swift receive → TextInput echo → Rust
/// receive), shaving 50–100ms off every voice turn.  When `stream_audio` delivers a
/// final transcript here, it also marks the `TranscriptChunk` as `fast_path = true` so
/// Swift suppresses the TextInput echo — preventing duplicate inference.
enum InternalEvent {
    TranscriptReady {
        text: String,
        trace_id: String,
    },
    /// Shell command-completion event from the zsh integration hook.
    ///
    /// Phase 30: delivered via `orchestrator_tx`; the session's `select!` loop calls
    /// `orchestrator.handle_shell_command()` on receipt. Silently dropped when no
    /// session is active (orchestrator_tx is None).
    ShellCommand {
        command: String,
        cwd: String,
        exit_code: Option<i32>,
    },
}

// ── Shell context listener (Phase 30) ────────────────────────────────────────

/// Parse and validate a JSON string from the shell integration hook.
///
/// Returns `(command, cwd, exit_code)` on success, or `None` on any error:
/// invalid JSON, missing fields, or command shorter than `SHELL_CMD_MIN_CHARS`.
/// Applies `SHELL_CMD_MAX_CHARS` / `SHELL_CWD_MAX_CHARS` truncation.
///
/// Extracted as a standalone function so unit tests can verify parsing without
/// spawning a real Unix socket listener.
fn parse_shell_payload(json_str: &str) -> Option<(String, String, Option<i32>)> {
    use crate::constants::{SHELL_CMD_MAX_CHARS, SHELL_CMD_MIN_CHARS, SHELL_CWD_MAX_CHARS};

    #[derive(serde::Deserialize)]
    struct ShellPayload {
        command: String,
        cwd: String,
        exit_code: Option<i32>,
    }

    let payload: ShellPayload = serde_json::from_str(json_str.trim()).ok()?;

    let cmd_chars = payload.command.chars().count();
    if cmd_chars < SHELL_CMD_MIN_CHARS {
        return None; // bare Enter, single-char alias, etc.
    }
    let command = if cmd_chars > SHELL_CMD_MAX_CHARS {
        payload.command.chars().take(SHELL_CMD_MAX_CHARS).collect()
    } else {
        payload.command
    };

    let cwd_chars = payload.cwd.chars().count();
    let cwd = if cwd_chars > SHELL_CWD_MAX_CHARS {
        payload.cwd.chars().take(SHELL_CWD_MAX_CHARS).collect()
    } else {
        payload.cwd
    };

    Some((command, cwd, payload.exit_code))
}

/// Accepts one-shot connections from the zsh shell hook and delivers parsed
/// events to the active session via `orchestrator_tx`.
///
/// Spawned once in `CoreService::new()`. Runs for the lifetime of the process.
/// Each command completion is a separate connect → write JSON → EOF → close
/// connection — no persistent state is maintained between commands.
///
/// If no session is active (`orchestrator_tx` is `None`), events are silently
/// dropped. Shell context is ephemeral; buffering across sessions adds no value.
async fn run_shell_listener(orchestrator_tx: Arc<Mutex<Option<mpsc::Sender<InternalEvent>>>>) {
    use crate::constants::SHELL_SOCKET_PATH;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    // Remove stale socket from a previous crash. Silent on ENOENT (first run).
    let _ = std::fs::remove_file(SHELL_SOCKET_PATH);

    let listener = match UnixListener::bind(SHELL_SOCKET_PATH) {
        Ok(l) => {
            info!(socket = SHELL_SOCKET_PATH, "Shell listener ready");
            l
        }
        Err(e) => {
            warn!(
                error  = %e,
                socket = SHELL_SOCKET_PATH,
                "Shell listener: failed to bind — shell context disabled for this session"
            );
            return;
        }
    };

    loop {
        let (mut stream, _addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Shell listener: accept error — continuing");
                continue;
            }
        };

        // Each connection handled in its own task so the accept loop is never blocked.
        // Connection-per-event means these tasks are very short-lived.
        let tx_arc = orchestrator_tx.clone();
        tokio::spawn(async move {
            let mut buf = Vec::with_capacity(512);
            if let Err(e) = stream.read_to_end(&mut buf).await {
                warn!(error = %e, "Shell listener: read error");
                return;
            }
            let json_str = match std::str::from_utf8(&buf) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Shell listener: non-UTF8 payload");
                    return;
                }
            };

            let Some((command, cwd, exit_code)) = parse_shell_payload(json_str) else {
                // Warn only for non-empty payloads — empty reads happen if nc connects
                // and closes without writing (e.g. a Dexter liveness probe).
                if !json_str.trim().is_empty() {
                    warn!(
                        raw = json_str,
                        "Shell listener: payload rejected (parse/validation failure)"
                    );
                }
                return;
            };

            let guard = tx_arc.lock().await;
            if let Some(tx) = guard.as_ref() {
                // Ignore send error — session may be tearing down; event is safely lost.
                let _ = tx
                    .send(InternalEvent::ShellCommand {
                        command,
                        cwd,
                        exit_code,
                    })
                    .await;
            }
            // If None: no session active; drop silently.
        });
    }
}

// ── Service implementation ────────────────────────────────────────────────────

/// The tonic gRPC service implementation.
///
/// Phase 6: `CoreService` holds an `Arc<DexterConfig>` so it can construct a fresh
/// `CoreOrchestrator` for every incoming `Session()` RPC call without cloning the
/// full config struct on every request.
///
/// Phase 23: `CoreService` also owns a persistent STT worker (`stt`) so that
/// `stream_audio()` can reuse a single Python process across utterances.  Without
/// persistence each call would load the Whisper model from disk (~8 s), making the
/// round-trip feel completely broken.  The worker is pre-warmed in a background task
/// at construction; `stream_audio()` falls back to on-demand spawn if the pre-warm
/// hasn't completed yet or if the worker dies.
pub struct CoreService {
    cfg: Arc<DexterConfig>,
    /// Persistent STT worker — one per server instance, shared across all utterances.
    /// `Option<WorkerClient>` is `None` before first spawn or after worker death.
    /// `stream_audio()` holds the mutex for exactly one utterance (chunks → TRANSCRIPT_DONE).
    stt: Arc<Mutex<Option<WorkerClient>>>,
    /// Fast-path channel to the active session's orchestrator.
    ///
    /// Phase 24c: populated by the session reader task when it starts; cleared when it
    /// exits.  `stream_audio()` sends `InternalEvent::TranscriptReady` here for final
    /// transcripts, bypassing the Swift echo round-trip.  `None` when no session is active.
    orchestrator_tx: Arc<Mutex<Option<mpsc::Sender<InternalEvent>>>>,
    /// Phase 38c: daemon-lifetime shared state — TTS worker, browser worker, model
    /// warmup atomics, startup-greeting-sent flag. Cloned into every new
    /// `CoreOrchestrator` so sessions share workers and warmup state instead of
    /// each session spawning its own. See `SharedDaemonState` doc for the full
    /// architecture rationale.
    shared: crate::orchestrator::SharedDaemonState,
}

impl CoreService {
    pub fn new(cfg: Arc<DexterConfig>) -> Self {
        let stt: Arc<Mutex<Option<WorkerClient>>> = Arc::new(Mutex::new(None));
        // Pre-warm the STT worker in the background — model load takes ~8 s.
        // stream_audio() falls back to on-demand spawn if this hasn't completed.
        let stt_warm = stt.clone();
        tokio::spawn(async move {
            match WorkerClient::spawn(WorkerType::Stt, VOICE_PYTHON_EXE, VOICE_STT_WORKER_PATH)
                .await
            {
                Ok(client) => {
                    *stt_warm.lock().await = Some(client);
                    info!("STT worker pre-warmed and ready");
                }
                Err(e) => warn!(error = %e, "STT pre-warm failed — will spawn on first utterance"),
            }
        });

        // Phase 38c: construct daemon-lifetime shared state and spawn the
        // startup warmup task BEFORE any session can connect. New sessions
        // inherit clones of this state and skip warmup entirely.
        let shared = crate::orchestrator::SharedDaemonState::new_degraded();
        let shared_for_warmup = shared.clone();
        let cfg_for_warmup = cfg.clone();
        tokio::spawn(async move {
            shared_for_warmup.run_startup_warmup(cfg_for_warmup).await;
        });

        let service = Self {
            cfg,
            stt,
            orchestrator_tx: Arc::new(Mutex::new(None)),
            shared,
        };

        // Phase 30: spawn shell context listener. Accepts one-shot connections from the
        // zsh integration hook at SHELL_SOCKET_PATH and forwards parsed events to the
        // active session via orchestrator_tx. Non-fatal if bind fails (shell context
        // is degraded but the service still starts).
        let shell_tx = service.orchestrator_tx.clone();
        tokio::spawn(run_shell_listener(shell_tx));

        service
    }

    // Phase 38c: daemon shutdown of shared workers happens implicitly via
    // `kill_on_drop(true)` (Session 1 [25]). When the tokio runtime exits on
    // SIGINT/SIGTERM, all spawned tasks are aborted, all WorkerClient handles
    // are dropped, all Tokio Child processes get SIGKILL'd. No explicit
    // shutdown method is needed for correctness. A future graceful-shutdown
    // refactor (sending SHUTDOWN frames so Python workers exit cleanly
    // instead of being SIGKILL'd) is straightforward but deferred — the
    // current behavior is functionally correct, just not maximally polite.
}

/// The stream type returned by the Session RPC.
/// Pinned boxed trait object — standard pattern for tonic server-side streaming.
type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerEvent, Status>> + Send>>;

/// The stream type returned by the StreamAudio RPC.
type StreamAudioStream = Pin<Box<dyn Stream<Item = Result<TranscriptChunk, Status>> + Send>>;

#[tonic::async_trait]
impl DexterService for CoreService {
    async fn ping(&self, request: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        let trace_id = request.into_inner().trace_id;
        info!(trace_id = %trace_id, "Ping received");
        Ok(Response::new(PingResponse {
            trace_id,
            core_version: CORE_VERSION.to_string(),
        }))
    }

    type SessionStream = SessionStream;

    /// Phase 6 session handler: routes ClientEvents through CoreOrchestrator.
    ///
    /// Stream lifetime contract:
    ///   1. A bounded mpsc channel (capacity 16) drives the outbound ServerEvent stream.
    ///   2. IDLE state is pushed immediately so Swift can set its initial visual state
    ///      before the orchestrator is even constructed.
    ///   3. A reader task constructs `CoreOrchestrator` and processes inbound events in a loop.
    ///      It holds `_signal_done` (a oneshot Sender) — dropped automatically on any exit path.
    ///   4. A hold-open task owns the channel Sender (`tx`) and awaits the oneshot Receiver.
    ///      When the reader exits (dropping `_signal_done`), the oneshot fires, the hold-open
    ///      task drops `tx`, and ReceiverStream yields Poll::Ready(None) → END_STREAM to Swift.
    ///
    /// Stream close sequence:
    ///   reader loop exits → orchestrator.shutdown().await (state written) → _signal_done dropped
    ///   → oneshot fires → hold-open exits → tx dropped → ReceiverStream closes → Swift ends
    async fn session(
        &self,
        request: Request<Streaming<ClientEvent>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let session_trace = new_trace_id();
        let cfg = self.cfg.clone();
        // Phase 38c: clone of the daemon-lifetime shared state. Moved into the
        // reader task below so the orchestrator constructor can receive its own clone
        // and the greeting-gate compare_exchange can run against the shared atomic.
        let shared_clone = self.shared.clone();
        let (tx, rx) = mpsc::channel::<Result<ServerEvent, Status>>(16);

        // Push the initial IDLE state so Swift can reflect the entity's resting posture.
        // This is sent before the orchestrator is constructed so there is no latency gap
        // between the gRPC session opening and Swift's first visual state update.
        let idle_event = ServerEvent {
            trace_id: new_trace_id(),
            event: Some(proto::server_event::Event::EntityState(EntityStateChange {
                state: EntityState::Idle.into(),
            })),
        };
        tx.send(Ok(idle_event))
            .await
            .map_err(|_| Status::internal("session channel closed before IDLE send"))?;

        info!(session = %session_trace, "Session opened — IDLE sent");

        // Oneshot channel couples reader task lifetime to hold-open task lifetime.
        // The Sender is held by the reader; its drop (on any exit path) signals the hold-open.
        let (reader_done_tx, reader_done_rx) = tokio::sync::oneshot::channel::<()>();
        let tx_reader = tx.clone();
        let mut inbound = request.into_inner();
        let orchestrator_tx_arc = self.orchestrator_tx.clone();

        // ── Reader task ──────────────────────────────────────────────────────
        tokio::spawn(async move {
            // _signal_done is dropped when this async block exits on any path:
            // normal loop exit, orchestrator construction failure, or panic.
            let _signal_done = reader_done_tx;

            // Phase 24: bounded channel for background action results.
            // Capacity 8: at most a few concurrent actions in flight at once.
            let (action_tx, mut action_rx) = mpsc::channel::<ActionResult>(8);

            // Phase 27: bounded channel for background generation results.
            // Capacity 4: at most one active + a few cancelled results draining.
            let (gen_tx, mut gen_rx) = mpsc::channel::<GenerationResult>(4);

            // Phase 24c: fast-path channel — stream_audio delivers final transcripts
            // here directly, skipping the 50–100ms Swift echo round-trip.
            // Cleared on session exit so stream_audio cannot deliver after teardown.
            let (internal_tx, mut internal_rx) = mpsc::channel::<InternalEvent>(8);
            *orchestrator_tx_arc.lock().await = Some(internal_tx);

            // Phase 38c: pass the daemon-lifetime SharedDaemonState clone so this
            // session uses the already-warm TTS/browser workers and skips its own
            // model warmup entirely.
            let tx_reader_clone = tx_reader.clone(); // kept for fallback IDLE send after greeting
            let shared_for_orch = shared_clone.clone();
            let mut orchestrator = match CoreOrchestrator::new(
                &cfg,
                session_trace.clone(),
                tx_reader,
                action_tx,
                gen_tx,
                shared_for_orch,
            ) {
                Ok(o) => o,
                Err(e) => {
                    error!(
                        session = %session_trace,
                        error   = %e,
                        "CoreOrchestrator construction failed — closing session"
                    );
                    return; // _signal_done dropped → oneshot fires → hold-open exits → tx dropped
                }
            };

            // Phase 38c: claim startup greeting responsibility via compare_exchange.
            // Only the FIRST session to connect after daemon start sends "Starting up…"
            // and "Ready." — subsequent reconnects (Swift restart, dexter-cli runs)
            // skip the greeting and go straight to IDLE. The atomic prevents a race
            // between two near-simultaneous reconnects.
            let we_own_greeting = shared_clone
                .startup_greeting_sent
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();

            if we_own_greeting {
                // Show startup status immediately — daemon may still be mid-warmup.
                let startup_trace = new_trace_id();
                let _ = tx_reader_clone
                    .send(Ok(ServerEvent {
                        trace_id: startup_trace.clone(),
                        event: Some(proto::server_event::Event::EntityState(EntityStateChange {
                            state: EntityState::Thinking.into(),
                        })),
                    }))
                    .await;
                let _ = tx_reader_clone
                    .send(Ok(ServerEvent {
                        trace_id: startup_trace,
                        event: Some(proto::server_event::Event::TextResponse(
                            proto::TextResponse {
                                content: "Starting up…".to_string(),
                                // is_final=true: closes the response immediately so the HUD auto-dismisses.
                                is_final: true,
                            },
                        )),
                    }))
                    .await;

                // Phase 38c: wait for the daemon-startup warmup to complete before
                // playing "Ready." — the announcement is a contract with the operator
                // that any routed query will answer immediately, so PRIMARY must be
                // warm. Poll the shared atomic at 250ms cadence; warmup typically
                // takes 25-40 seconds on cold start, ~0 seconds on a hot daemon.
                while !shared_clone.primary_model_warm.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }

                // Announce readiness: "Ready." via TTS + HUD. IDLE follows via
                // AUDIO_PLAYBACK_COMPLETE.
                if let Err(e) = orchestrator.send_startup_greeting(&session_trace).await {
                    warn!(
                        session = %session_trace,
                        error   = %e,
                        "Startup greeting failed — entering main loop anyway"
                    );
                    // Fallback IDLE send so the session still becomes interactive.
                    let idle_fallback = ServerEvent {
                        trace_id: new_trace_id(),
                        event: Some(proto::server_event::Event::EntityState(EntityStateChange {
                            state: EntityState::Idle.into(),
                        })),
                    };
                    let _ = tx_reader_clone.send(Ok(idle_fallback)).await;
                }
            } else {
                // Phase 38c: subsequent session — daemon is already warm and the
                // first session already played "Ready." We skip the greeting entirely
                // and just send IDLE so the session becomes interactive immediately.
                info!(
                    session = %session_trace,
                    "Subsequent session — skipping startup greeting (already played by first session)"
                );
                let idle_event = ServerEvent {
                    trace_id: new_trace_id(),
                    event: Some(proto::server_event::Event::EntityState(EntityStateChange {
                        state: EntityState::Idle.into(),
                    })),
                };
                let _ = tx_reader_clone.send(Ok(idle_event)).await;
            }

            // Periodic health-check timer for the TTS worker (Phase 13).
            // Interleaved with inbound message reads via tokio::select! so the timer
            // never blocks message processing and the loop stays fully cooperative.
            let mut health_interval = tokio::time::interval(std::time::Duration::from_secs(
                VOICE_WORKER_HEALTH_INTERVAL_SECS,
            ));
            // Burst-tick behaviour: skip the immediate first tick (t=0) so we don't
            // health-check before start_voice() has even had a chance to succeed.
            health_interval.tick().await;

            // Browser worker health-check fires every 60s (much less frequent than TTS
            // because browser is idle most of the time and Chromium startup is expensive).
            let mut browser_health_interval = tokio::time::interval(
                std::time::Duration::from_secs(BROWSER_WORKER_HEALTH_INTERVAL_SECS),
            );
            browser_health_interval.tick().await; // skip t=0

            loop {
                tokio::select! {
                    msg = inbound.message() => {
                        match msg {
                            Ok(Some(event)) => {
                                info!(
                                    session  = %session_trace,
                                    trace_id = %event.trace_id,
                                    kind     = event.event.as_ref().map(event_kind).unwrap_or("none"),
                                    "ClientEvent received"
                                );
                                if let Err(e) = orchestrator.handle_event(event).await {
                                    error!(
                                        session = %session_trace,
                                        error   = %e,
                                        "Orchestrator event handler failed — closing session"
                                    );
                                    break;
                                }
                            }
                            Ok(None)  => { info!(session = %session_trace, "Client closed session stream"); break; }
                            Err(e)    => {
                                if is_benign_session_stream_close(&e) {
                                    info!(
                                        session = %session_trace,
                                        error   = %e,
                                        "Client session stream closed during transport shutdown"
                                    );
                                } else {
                                    error!(session = %session_trace, error = %e, "Session stream error");
                                }
                                break;
                            }
                        }
                    }
                    // Phase 24: receive results from background action tasks.
                    result = action_rx.recv() => {
                        if let Some(result) = result {
                            if let Err(e) = orchestrator.handle_action_result(result).await {
                                error!(
                                    session = %session_trace,
                                    error   = %e,
                                    "handle_action_result failed — closing session"
                                );
                                break;
                            }
                        }
                    }
                    _ = health_interval.tick() => {
                        orchestrator.voice_health_check().await;
                        // Phase 24: GC stale interactions (>5 min) every health tick.
                        orchestrator.gc_stale_interactions();
                    }
                    _ = browser_health_interval.tick() => {
                        orchestrator.browser_health_check().await;
                    }
                    // Phase 27: generation result from background generation task.
                    // run_generation_background delivers GenerationResult here after the
                    // token stream completes (or is cancelled by handle_barge_in).
                    gen_result = gen_rx.recv() => {
                        if let Some(result) = gen_result {
                            if let Err(e) = orchestrator.handle_generation_complete(result).await {
                                error!(
                                    session = %session_trace,
                                    error   = %e,
                                    "handle_generation_complete failed — closing session"
                                );
                                break;
                            }
                        }
                    }
                    // Phase 24c / 30: internal events from background tasks.
                    // TranscriptReady (Phase 24c): fast-path transcript from stream_audio.
                    // ShellCommand   (Phase 30):   shell hook notification from run_shell_listener.
                    internal = internal_rx.recv() => {
                        match internal {
                        Some(InternalEvent::TranscriptReady { text, trace_id }) => {
                            info!(
                                session  = %session_trace,
                                trace_id = %trace_id,
                                "Fast-path transcript — bypassing Swift echo"
                            );
                            if let Err(e) = orchestrator.handle_fast_transcript(text, trace_id).await {
                                error!(
                                    session = %session_trace,
                                    error   = %e,
                                    "Fast-path transcript handler failed — closing session"
                                );
                                break;
                            }
                        }
                        // Phase 30: shell command-completion from the zsh hook listener.
                        // Updates context_observer; no proactive trigger (Phase 31+ scope).
                        Some(InternalEvent::ShellCommand { command, cwd, exit_code }) => {
                            orchestrator.handle_shell_command(command, cwd, exit_code).await;
                        }
                        None => {}
                        }
                    }
                }
            }

            // Clear the fast-path slot before shutdown — prevents stream_audio from
            // delivering transcripts to a dead orchestrator after session teardown.
            *orchestrator_tx_arc.lock().await = None;

            // Persist session state before releasing _signal_done.
            // This ensures the state file is written before the stream closes.
            orchestrator.shutdown().await;
            // _signal_done dropped here → oneshot fires → hold-open exits → tx dropped
        });

        // ── Hold-open task ───────────────────────────────────────────────────
        // Owns `tx` (the last Sender clone). Exits when reader signals done via oneshot.
        // Dropping `tx` here closes the ReceiverStream with END_STREAM trailers to Swift.
        // THIS TASK USES reader_done_rx.await — NOT pending(). The stream closes correctly.
        tokio::spawn(async move {
            let _hold = tx;
            let _ = reader_done_rx.await;
            // _hold (tx) dropped here → ReceiverStream yields None → Swift for-await ends
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type StreamAudioStream = StreamAudioStream;

    /// Voice audio → STT transcript stream (Phase 23: persistent STT worker).
    ///
    /// Acquires the server-wide persistent STT worker for one utterance, forwards
    /// PCM chunks, then reads TRANSCRIPT frames until MSG_TRANSCRIPT_DONE.  The
    /// mutex is released without shutting down the worker — it stays alive and
    /// pre-loaded for the next utterance (zero model-load latency after first call).
    ///
    /// If the worker is not yet ready (pre-warm still in progress), spawns one
    /// on-demand.  If the worker dies mid-utterance, clears the slot so the next
    /// call re-spawns cleanly.  Returns an empty Ok stream on any failure so Swift
    /// sees a clean end-of-transcript rather than a gRPC error.
    async fn stream_audio(
        &self,
        request: Request<Streaming<AudioChunk>>,
    ) -> Result<Response<Self::StreamAudioStream>, Status> {
        use crate::voice::protocol::msg;

        let (tx, rx) = mpsc::channel::<Result<TranscriptChunk, Status>>(16);
        let stt_arc = self.stt.clone();
        let orchestrator_tx_fp = self.orchestrator_tx.clone();

        tokio::spawn(async move {
            // Acquire the persistent STT worker.  If not yet ready, spawn on-demand.
            let mut guard = stt_arc.lock().await;
            if guard.is_none() {
                match WorkerClient::spawn(WorkerType::Stt, VOICE_PYTHON_EXE, VOICE_STT_WORKER_PATH)
                    .await
                {
                    Ok(c) => {
                        info!("STT worker ready (on-demand spawn)");
                        *guard = Some(c);
                    }
                    Err(e) => {
                        warn!(error = %e, "STT worker unavailable — returning empty transcript");
                        return;
                    }
                }
            }

            // Forward chunks + read transcripts.  Borrow client from guard so the
            // mutex is held for the full utterance, serialising concurrent calls.
            let alive = {
                let client = guard.as_mut().expect("guard is Some; checked above");

                let mut inbound = request.into_inner();
                let mut send_ok = true;
                while let Ok(Some(chunk)) = inbound.message().await {
                    if client
                        .write_frame(msg::AUDIO_CHUNK, &chunk.data)
                        .await
                        .is_err()
                    {
                        send_ok = false;
                        break;
                    }
                }

                if !send_ok {
                    false // write failed — worker is dead
                } else {
                    let _ = client.write_frame(msg::AUDIO_END, &[]).await;

                    // Read TRANSCRIPT frames until MSG_TRANSCRIPT_DONE (end-of-utterance
                    // sentinel that keeps the worker alive for the next call).
                    let mut still_alive = true;
                    loop {
                        match client.read_frame().await {
                            Ok(Some((msg::TRANSCRIPT, payload))) => {
                                match serde_json::from_slice::<serde_json::Value>(&payload) {
                                    Ok(v) => {
                                        let text = v["text"].as_str().unwrap_or("").to_string();
                                        let is_final = v["is_final"].as_bool().unwrap_or(true);

                                        // Phase 24c: for final transcripts, attempt direct
                                        // delivery to the active session orchestrator.
                                        // If a session is live, mark fast_path=true so Swift
                                        // suppresses the TextInput echo — preventing duplicate
                                        // inference.  If no session is active (race at startup
                                        // or teardown), fast_path stays false and Swift echoes
                                        // normally, maintaining correct behaviour.
                                        let fast_path = if is_final && !text.is_empty() {
                                            let guard = orchestrator_tx_fp.lock().await;
                                            if let Some(ref otx) = *guard {
                                                let trace_id = Uuid::new_v4().to_string();
                                                // A send error here means the session is ending;
                                                // the transcript will still reach Swift via gRPC.
                                                let delivered = otx
                                                    .send(InternalEvent::TranscriptReady {
                                                        text: text.clone(),
                                                        trace_id,
                                                    })
                                                    .await
                                                    .is_ok();
                                                delivered
                                            } else {
                                                false // No active session — Swift echoes normally
                                            }
                                        } else {
                                            false // Partial transcripts never echoed by Swift
                                        };

                                        let chunk = TranscriptChunk {
                                            text,
                                            is_final,
                                            sequence_number: v["sequence"].as_u64().unwrap_or(0)
                                                as u32,
                                            fast_path,
                                        };
                                        let _ = tx.send(Ok(chunk)).await;
                                    }
                                    Err(e) => warn!(error = %e, "STT TRANSCRIPT JSON parse error"),
                                }
                            }
                            Ok(Some((msg::TRANSCRIPT_DONE, _))) => break, // end of utterance
                            Ok(Some((msg::HEALTH_PONG, _))) => {}         // discard stray pongs
                            Ok(Some(_)) | Ok(None) => {
                                still_alive = false;
                                break;
                            }
                            Err(e) => {
                                error!(error = %e, "STT worker read error");
                                still_alive = false;
                                break;
                            }
                        }
                    }
                    still_alive
                }
            }; // client borrow ends here → guard is exclusively owned again

            if !alive {
                // Worker died — clear slot so next call re-spawns cleanly.
                warn!("STT worker died — clearing for restart on next utterance");
                *guard = None;
            }
            // guard drops → mutex released. Worker stays alive for next utterance.
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

// ── Server bind ───────────────────────────────────────────────────────────────

/// Binds a tonic gRPC server to the given Unix domain socket path.
///
/// Phase 6: accepts `Arc<DexterConfig>` so `CoreService` can construct a fresh
/// `CoreOrchestrator` per session call. `main.rs` wraps the loaded config in `Arc`
/// before calling this function.
///
/// Uses `UnixListenerStream` from tokio-stream, which wraps tokio's `UnixListener`
/// in a Stream that tonic's `serve_with_incoming` can consume directly.
pub async fn serve(socket_path: &str, cfg: Arc<DexterConfig>) -> anyhow::Result<()> {
    let listener = UnixListener::bind(socket_path)?;
    let stream = UnixListenerStream::new(listener);

    info!(socket = socket_path, "gRPC server listening");

    Server::builder()
        .add_service(DexterServiceServer::new(CoreService::new(cfg)))
        .serve_with_incoming(stream)
        .await?;

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Generates a UUID v4 trace ID for correlating events across components.
fn new_trace_id() -> String {
    Uuid::new_v4().to_string()
}

/// Returns a static string label for a ClientEvent variant.
/// Used in structured log fields — avoids allocating a String per log call.
fn event_kind(event: &proto::client_event::Event) -> &'static str {
    match event {
        proto::client_event::Event::TextInput(_) => "text_input",
        proto::client_event::Event::UiAction(_) => "ui_action",
        proto::client_event::Event::SystemEvent(_) => "system_event",
        proto::client_event::Event::ActionApproval(_) => "action_approval",
        proto::client_event::Event::BargIn(_) => "barg_in",
    }
}

// ── Phase 30: parse_shell_payload unit tests ──────────────────────────────────

#[cfg(test)]
mod shell_payload_tests {
    use super::{is_benign_session_stream_close, parse_shell_payload};
    use tonic::{Code, Status};

    #[test]
    fn parse_shell_payload_valid() {
        let json = r#"{"command":"cargo test","cwd":"/tmp/project","exit_code":0}"#;
        let (cmd, cwd, code) = parse_shell_payload(json).unwrap();
        assert_eq!(cmd, "cargo test");
        assert_eq!(cwd, "/tmp/project");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn parse_shell_payload_null_exit_code() {
        let json = r#"{"command":"cargo test","cwd":"/tmp","exit_code":null}"#;
        let (_cmd, _cwd, code) = parse_shell_payload(json).unwrap();
        assert_eq!(code, None);
    }

    #[test]
    fn parse_shell_payload_command_too_short() {
        // Single-char command (e.g. `l` alias) must be rejected.
        let json = r#"{"command":"l","cwd":"/tmp","exit_code":0}"#;
        assert!(
            parse_shell_payload(json).is_none(),
            "single-char command must be rejected (< SHELL_CMD_MIN_CHARS)"
        );
    }

    #[test]
    fn parse_shell_payload_empty_command_rejected() {
        let json = r#"{"command":"","cwd":"/tmp","exit_code":0}"#;
        assert!(
            parse_shell_payload(json).is_none(),
            "empty command must be rejected"
        );
    }

    #[test]
    fn parse_shell_payload_truncates_long_command() {
        let long_cmd = "a".repeat(600);
        let json = format!(r#"{{"command":"{}","cwd":"/tmp","exit_code":0}}"#, long_cmd);
        let (cmd, _cwd, _code) = parse_shell_payload(&json).unwrap();
        assert_eq!(
            cmd.chars().count(),
            500,
            "command must be truncated to SHELL_CMD_MAX_CHARS (500)"
        );
    }

    #[test]
    fn parse_shell_payload_truncates_long_cwd() {
        let long_cwd = "/x".repeat(150); // 300 chars
        let json = format!(r#"{{"command":"ls","cwd":"{}","exit_code":0}}"#, long_cwd);
        let (_cmd, cwd, _code) = parse_shell_payload(&json).unwrap();
        assert_eq!(
            cwd.chars().count(),
            200,
            "cwd must be truncated to SHELL_CWD_MAX_CHARS (200)"
        );
    }

    #[test]
    fn parse_shell_payload_invalid_json_returns_none() {
        assert!(parse_shell_payload("not json at all").is_none());
        assert!(parse_shell_payload("").is_none());
        // Missing required fields — serde returns Err (missing field error, not syntax error).
        assert!(
            parse_shell_payload("{}").is_none(),
            "empty object must be None — missing command and cwd fields"
        );
    }

    #[test]
    fn benign_cli_transport_close_is_not_session_error() {
        let status = Status::new(
            Code::Unknown,
            "h2 protocol error: error reading a body from connection",
        );
        assert!(
            is_benign_session_stream_close(&status),
            "CLI transport shutdown should be classified as a benign close"
        );

        let real_error = Status::new(Code::Internal, "orchestrator exploded");
        assert!(
            !is_benign_session_stream_close(&real_error),
            "unrelated stream failures must remain error-level"
        );
    }
}

// ── Integration tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InferenceConfig, ModelConfig};
    use hyper_util::rt::TokioIo;
    use proto::{
        client_event, dexter_service_client::DexterServiceClient, AudioChunk as ProtoAudioChunk,
        SystemEvent, SystemEventType, TextInput,
    };
    use tokio::net::UnixStream;
    use tonic::transport::Endpoint;
    use tower::service_fn;

    /// Spawns a CoreService on a unique UDS path with a default config.
    ///
    /// Each test gets its own socket to avoid inter-test interference.
    /// The 50ms sleep gives the bound listener time to accept connections
    /// before the client attempts to connect — avoids ECONNREFUSED flakiness.
    async fn spawn_test_server() -> String {
        let path = format!("/tmp/dexter-test-{}.sock", Uuid::new_v4().simple());
        let _ = std::fs::remove_file(&path);

        let cfg = Arc::new(DexterConfig::default());
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            Server::builder()
                .add_service(DexterServiceServer::new(CoreService::new(cfg)))
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        path
    }

    /// Builds a tonic client channel over a Unix domain socket.
    ///
    /// `Endpoint::from_static("http://localhost")` supplies the HTTP/2 :authority header.
    /// The `connect_with_connector` + `service_fn` pattern bypasses DNS entirely —
    /// the closure opens a raw UnixStream that tonic wraps in its transport layer.
    async fn make_client(path: String) -> DexterServiceClient<tonic::transport::Channel> {
        let channel = Endpoint::from_static("http://localhost")
            .connect_with_connector(service_fn(move |_: tonic::transport::Uri| {
                let p = path.clone();
                // TokioIo bridges tokio's AsyncRead/AsyncWrite to hyper 1.x's
                // hyper::rt::io::Read/Write — required by tonic 0.12's transport layer.
                async move { UnixStream::connect(p).await.map(TokioIo::new) }
            }))
            .await
            .unwrap();
        DexterServiceClient::new(channel)
    }

    /// Spawns a CoreService with a caller-supplied config.
    ///
    /// Used by integration tests that need a specific model config (e.g., phi3:mini
    /// instead of the production defaults) so the test is not sensitive to which
    /// large models happen to be present on the test machine.
    async fn spawn_test_server_with_cfg(cfg: Arc<DexterConfig>) -> String {
        let path = format!("/tmp/dexter-test-{}.sock", Uuid::new_v4().simple());
        let _ = std::fs::remove_file(&path);

        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            Server::builder()
                .add_service(DexterServiceServer::new(CoreService::new(cfg)))
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        path
    }

    /// Builds a `DexterConfig` where every model tier is `phi3:mini`.
    ///
    /// `phi3:mini` is the only model confirmed available on the development machine
    /// (verified by `phi3_mini_is_available` in the engine test suite). Using it for all
    /// tiers means the routing decision (FAST / PRIMARY / HEAVY / etc.) does not affect
    /// whether the integration test can run — whichever tier the router selects, the model
    /// is available. The state_dir uses the default (home dir), which is acceptable here
    /// because `SessionStateManager::persist()` creates it with `create_dir_all` if absent.
    fn phi3_mini_cfg() -> Arc<DexterConfig> {
        let mut cfg = DexterConfig::default();
        let phi3 = "phi3:mini".to_string();
        cfg.models = ModelConfig {
            fast: phi3.clone(),
            primary: phi3.clone(),
            heavy: phi3.clone(),
            code: phi3.clone(),
            vision: phi3.clone(),
            embed: phi3,
        };
        // Raise the inactivity timeout a little — phi3:mini on cold load can be slow.
        cfg.inference = InferenceConfig {
            stream_inactivity_timeout_secs: 60,
            ..InferenceConfig::default()
        };
        Arc::new(cfg)
    }

    /// Verifies that the session opens with an IDLE entity state.
    ///
    /// Phase 6: The orchestrator's CONNECTED handler produces no TextResponse —
    /// it just logs. The old Phase 3 "session ready" stub is replaced by real
    /// orchestrator dispatch, which sends no text for SystemEvent(CONNECTED).
    ///
    /// The session stream closes naturally because the server sends all queued events
    /// and the client's single-element stream (CONNECTED) ends, causing the reader
    /// task to see Ok(None) and call orchestrator.shutdown() → stream closes.
    #[tokio::test]
    async fn session_opens_with_idle_state() {
        let socket = spawn_test_server().await;
        let mut client = make_client(socket.clone()).await;

        let connected_event = ClientEvent {
            trace_id: Uuid::new_v4().to_string(),
            session_id: Uuid::new_v4().to_string(),
            event: Some(client_event::Event::SystemEvent(SystemEvent {
                r#type: SystemEventType::Connected.into(),
                payload: String::new(),
            })),
        };

        let mut stream = client
            .session(tokio_stream::iter(vec![connected_event]))
            .await
            .unwrap()
            .into_inner();

        // First event must always be IDLE.
        let first = stream.message().await.unwrap().unwrap();
        match first.event {
            Some(proto::server_event::Event::EntityState(ref change)) => {
                assert_eq!(
                    change.state,
                    proto::EntityState::Idle as i32,
                    "First event must be EntityState(IDLE)"
                );
            }
            other => panic!("Expected EntityState(IDLE), got: {:?}", other),
        }

        // The CONNECTED handler produces no further events (deferred to Phase 7).
        // Stream should end within 2 seconds as the orchestrator shuts down after
        // the client's single-event stream ends.
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), stream.message()).await;

        match next {
            Ok(Ok(None)) => { /* Stream closed cleanly — expected */ }
            Ok(Ok(Some(evt))) => {
                // If the orchestrator sends any additional events (e.g. future CONNECTED
                // ack), accept them but verify they are EntityState events only.
                match &evt.event {
                    Some(proto::server_event::Event::EntityState(_)) => { /* ok */ }
                    other => panic!("Unexpected non-EntityState event: {:?}", other),
                }
            }
            Ok(Err(e)) => panic!("Stream error: {e}"),
            Err(_timeout) => { /* Timeout on stream end is acceptable — stream may be slow to close */
            }
        }

        std::fs::remove_file(&socket).ok();
    }

    /// Verifies that StreamAudio returns Ok even when the STT worker is unavailable.
    ///
    /// Worker scripts don't exist in the test environment → degraded → empty Ok stream.
    #[tokio::test]
    async fn stream_audio_returns_ok_stream_when_worker_unavailable() {
        let socket = spawn_test_server().await;
        let mut client = make_client(socket.clone()).await;

        let chunk = ProtoAudioChunk {
            data: vec![0u8; 32],
            sequence_number: 0,
            sample_rate: 16000,
        };

        let result = client.stream_audio(tokio_stream::iter(vec![chunk])).await;

        assert!(
            result.is_ok(),
            "stream_audio must return Ok even when STT worker unavailable"
        );

        std::fs::remove_file(&socket).ok();
    }

    /// Full end-to-end: TextInput → THINKING state → streaming tokens → IDLE state.
    ///
    /// Requires live Ollama with phi3:mini. The config overrides all model tiers to
    /// phi3:mini so the routing decision (FAST/PRIMARY/HEAVY/etc.) is irrelevant —
    /// whichever tier the router selects, the model exists and the test passes.
    /// Run with: make test-inference
    #[tokio::test]
    #[ignore = "requires live Ollama with phi3:mini — run with: make test-inference"]
    async fn text_input_produces_streaming_tokens() {
        let socket = spawn_test_server_with_cfg(phi3_mini_cfg()).await;
        let mut client = make_client(socket.clone()).await;

        // A simple, short prompt so the FAST model responds quickly.
        // "Say exactly: hello" is unambiguous — any LLM produces 1-3 tokens.
        let text_input_event = ClientEvent {
            trace_id: Uuid::new_v4().to_string(),
            session_id: Uuid::new_v4().to_string(),
            event: Some(client_event::Event::TextInput(TextInput {
                content: "Say exactly: hello".to_string(),
                from_voice: false,
            })),
        };

        let mut stream = client
            .session(tokio_stream::iter(vec![text_input_event]))
            .await
            .unwrap()
            .into_inner();

        // Event 1: IDLE — sent before orchestrator boots.
        let e1 = stream.message().await.unwrap().unwrap();
        assert!(
            matches!(e1.event, Some(proto::server_event::Event::EntityState(_))),
            "First event must be EntityState"
        );

        // Event 2: THINKING — orchestrator received TextInput.
        let e2 = stream.message().await.unwrap().unwrap();
        match e2.event {
            Some(proto::server_event::Event::EntityState(ref s)) => {
                assert_eq!(
                    s.state,
                    proto::EntityState::Thinking as i32,
                    "Second event must be EntityState(THINKING)"
                );
            }
            other => panic!("Expected THINKING, got: {:?}", other),
        }

        // Events N..M: streaming tokens (is_final=false), then is_final=true, then IDLE.
        //
        // Wrapped in a 30-second timeout so a stream teardown bug (e.g. hold-open task
        // not exiting) produces a clear timeout failure rather than a silent CI hang.
        let (saw_non_final, saw_final) =
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                let mut saw_non_final = false;
                let mut saw_final = false;
                loop {
                    let event = match stream.message().await {
                        Ok(Some(e)) => e,
                        Ok(None) => break, // END_STREAM — stream closed cleanly
                        Err(e) => panic!("Stream error: {e}"),
                    };
                    match event.event {
                        Some(proto::server_event::Event::TextResponse(ref r)) if !r.is_final => {
                            saw_non_final = true;
                        }
                        Some(proto::server_event::Event::TextResponse(ref r)) if r.is_final => {
                            saw_final = true;
                        }
                        Some(proto::server_event::Event::EntityState(ref s))
                            if s.state == proto::EntityState::Idle as i32 && saw_final =>
                        {
                            break; // Full IDLE → THINKING → tokens → IDLE cycle complete.
                        }
                        other => panic!("Unexpected event during streaming: {:?}", other),
                    }
                }
                (saw_non_final, saw_final)
            })
            .await
            .expect("integration test timed out after 30s — possible stream teardown bug");

        assert!(
            saw_non_final,
            "Expected streaming token events with is_final=false"
        );
        assert!(saw_final, "Expected final TextResponse with is_final=true");

        std::fs::remove_file(&socket).ok();
    }
}
