use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
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
    action::{
        audit::{recent_action_receipts, ActionAuditReceipt},
        ActionResult,
    },
    action_diagnostic::{build_action_diagnostic, ActionDiagnosticInput},
    action_evidence::{format_failed_action_evidence_block, format_success_action_evidence_block},
    ambient::{AmbientEvent, AmbientEventStore, AmbientSeverity},
    config::{resolve_config_path, DexterConfig},
    constants::{
        BROWSER_WORKER_HEALTH_INTERVAL_SECS, CORE_VERSION, SHELL_SOCKET_PATH, VOICE_PYTHON_EXE,
        VOICE_STT_WORKER_PATH, VOICE_WORKER_HEALTH_INTERVAL_SECS,
    },
    diagnostics::{self, DiskHealthSnapshot},
    orchestrator::{CoreOrchestrator, GenerationResult, SharedDaemonState},
    voice::{worker_client::WorkerClient, WorkerType},
};

// Pull the generated proto types into scope.
pub mod proto {
    tonic::include_proto!("dexter.v1");
}

use proto::{
    dexter_service_server::{DexterService, DexterServiceServer},
    AcknowledgeAmbientEventsRequest, AcknowledgeAmbientEventsResponse, ActionDiagnosticRequest,
    ActionDiagnosticResponse, ActionHistoryRequest, ActionHistoryResponse, ActionReceipt,
    AmbientEvent as AmbientEventProto, AmbientHistoryRequest, AmbientHistoryResponse,
    AmbientInboxRequest, AmbientInboxResponse, AudioChunk, ClientEvent, DiskHealth, EntityState,
    EntityStateChange, HealthRequest, HealthResponse, PingRequest, PingResponse, RestartComponent,
    RestartComponentRequest, RestartComponentResponse, ServerEvent, TranscriptChunk,
};

fn is_benign_session_stream_close(status: &Status) -> bool {
    let message = status.message();
    status.code() == tonic::Code::Unknown
        && (message.contains("h2 protocol error: error reading a body from connection")
            || message.contains("operation was canceled")
            || message.contains("stream closed"))
}

fn nonempty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn ambient_event_proto(event: AmbientEvent, trace_id: &str) -> AmbientEventProto {
    AmbientEventProto {
        event_id: event.event_id,
        timestamp: event.timestamp,
        source: event.source,
        kind: event.kind,
        severity: event.severity.as_str().to_string(),
        title: event.title,
        summary: event.summary,
        status: event.status.as_str().to_string(),
        payload_json: serde_json::to_string(&event.payload).unwrap_or_else(|error| {
            warn!(
                trace_id = %trace_id,
                error = %error,
                "Ambient event payload could not be serialized for RPC"
            );
            "{}".to_string()
        }),
    }
}

fn restart_component_label(component: RestartComponent) -> &'static str {
    match component {
        RestartComponent::Stt => "stt",
        RestartComponent::Tts => "tts",
        RestartComponent::Browser => "browser",
        RestartComponent::Unspecified => "unspecified",
    }
}

fn action_diagnostic_health_warnings(health: &HealthResponse) -> Vec<String> {
    if health.status == "ready" {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    if !health.degraded_components.is_empty() {
        warnings.push(format!(
            "Degraded components: {}",
            health.degraded_components.join(", ")
        ));
    }
    for (name, status) in [
        ("STT worker", health.stt_worker.as_str()),
        ("TTS worker", health.tts_worker.as_str()),
        ("browser worker", health.browser_worker.as_str()),
    ] {
        if matches!(status, "degraded" | "failed" | "unavailable") {
            warnings.push(format!("{name}: {status}"));
        }
    }
    if warnings.is_empty() {
        warnings.push(format!("Health status: {}", health.status));
    }
    warnings
}

fn latest_action_summary_markdown(receipts: &[ActionAuditReceipt]) -> String {
    let Some(receipt) = receipts.first() else {
        return "- No recent action receipt was found.\n".to_string();
    };

    if receipt.outcome == "executed" {
        format_success_action_evidence_block(
            receipt,
            "The latest audited action executed successfully.",
        )
    } else {
        format_failed_action_evidence_block(receipt)
    }
}

// ── Startup health summary ───────────────────────────────────────────────────

const STARTUP_HEALTH_STT_WAIT_SECS: u64 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComponentStartupStatus {
    Ready,
    Degraded,
    Pending,
}

impl ComponentStartupStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Pending => "pending",
        }
    }

    fn is_ready(self) -> bool {
        self == Self::Ready
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupHealthSnapshot {
    fast_model: String,
    primary_model: String,
    embed_model: String,
    fast_model_warm: bool,
    primary_model_warm: bool,
    embed_model_warm: bool,
    startup_warmup_complete: bool,
    stt_worker: ComponentStartupStatus,
    tts_worker: ComponentStartupStatus,
    browser_worker: ComponentStartupStatus,
    browser_worker_detail: String,
    browser_worker_recovery_hint: String,
    disk: Vec<DiskHealthSnapshot>,
}

impl StartupHealthSnapshot {
    fn from_runtime(
        cfg: &DexterConfig,
        shared: &SharedDaemonState,
        stt_worker: ComponentStartupStatus,
    ) -> Self {
        let browser_worker = worker_startup_status(
            shared.browser.is_available(),
            shared.startup_warmup_complete.load(Ordering::SeqCst),
        );
        let browser_diagnostic = shared.browser.last_failure();
        let (browser_worker_detail, browser_worker_recovery_hint) = if browser_worker.is_ready() {
            (String::new(), String::new())
        } else if let Some(diagnostic) = browser_diagnostic {
            (
                format!("{}: {}", diagnostic.kind.as_str(), diagnostic.detail),
                diagnostic.recovery_hint.to_string(),
            )
        } else {
            (String::new(), String::new())
        };

        Self {
            fast_model: cfg.models.fast.clone(),
            primary_model: cfg.models.primary.clone(),
            embed_model: cfg.models.embed.clone(),
            fast_model_warm: shared.fast_model_warm.load(Ordering::SeqCst),
            primary_model_warm: shared.primary_model_warm.load(Ordering::SeqCst),
            embed_model_warm: shared.embed_model_warm.load(Ordering::SeqCst),
            startup_warmup_complete: shared.startup_warmup_complete.load(Ordering::SeqCst),
            stt_worker,
            tts_worker: worker_startup_status(
                shared.voice.is_tts_available(),
                shared.startup_warmup_complete.load(Ordering::SeqCst),
            ),
            browser_worker,
            browser_worker_detail,
            browser_worker_recovery_hint,
            disk: diagnostics::collect_operator_disk_health(&cfg.core.state_dir),
        }
    }

    fn overall_status(&self) -> &'static str {
        if self.has_degraded_components() {
            "degraded"
        } else if self.has_pending_components() {
            "pending"
        } else {
            "ready"
        }
    }

    fn degraded_components(&self) -> Vec<String> {
        let mut components = Vec::new();
        if self.fast_model_status() != ComponentStartupStatus::Ready {
            components.push("fast_model".to_string());
        }
        if self.primary_model_status() != ComponentStartupStatus::Ready {
            components.push("primary_model".to_string());
        }
        if self.embed_model_status() != ComponentStartupStatus::Ready {
            components.push("embed_model".to_string());
        }
        if !self.stt_worker.is_ready() {
            components.push("stt_worker".to_string());
        }
        if !self.tts_worker.is_ready() {
            components.push("tts_worker".to_string());
        }
        if !self.browser_worker.is_ready() {
            components.push("browser_worker".to_string());
        }
        components.extend(diagnostics::disk_degraded_components(&self.disk));
        components
    }

    fn has_degraded_components(&self) -> bool {
        self.fast_model_status() == ComponentStartupStatus::Degraded
            || self.primary_model_status() == ComponentStartupStatus::Degraded
            || self.embed_model_status() == ComponentStartupStatus::Degraded
            || self.stt_worker == ComponentStartupStatus::Degraded
            || self.tts_worker == ComponentStartupStatus::Degraded
            || self.browser_worker == ComponentStartupStatus::Degraded
            || self.disk.iter().any(|disk| !disk.status.is_ready())
    }

    fn has_pending_components(&self) -> bool {
        self.fast_model_status() == ComponentStartupStatus::Pending
            || self.primary_model_status() == ComponentStartupStatus::Pending
            || self.embed_model_status() == ComponentStartupStatus::Pending
            || self.stt_worker == ComponentStartupStatus::Pending
            || self.tts_worker == ComponentStartupStatus::Pending
            || self.browser_worker == ComponentStartupStatus::Pending
    }

    fn fast_model_status(&self) -> ComponentStartupStatus {
        model_startup_status(self.fast_model_warm, self.startup_warmup_complete)
    }

    fn primary_model_status(&self) -> ComponentStartupStatus {
        model_startup_status(self.primary_model_warm, self.startup_warmup_complete)
    }

    fn embed_model_status(&self) -> ComponentStartupStatus {
        model_startup_status(self.embed_model_warm, self.startup_warmup_complete)
    }

    fn degraded_components_label(&self) -> String {
        let components = self.degraded_components();
        if components.is_empty() {
            "none".to_string()
        } else {
            components.join(",")
        }
    }

    fn into_health_response(
        self,
        trace_id: String,
        cfg: &DexterConfig,
        config_path: String,
        residency: crate::system::residency::ResidencyStatus,
    ) -> HealthResponse {
        let status = self.overall_status().to_string();
        let degraded_components = self.degraded_components();
        HealthResponse {
            trace_id,
            core_version: CORE_VERSION.to_string(),
            status,
            degraded_components,
            socket: cfg.core.socket_path.clone(),
            shell_socket: SHELL_SOCKET_PATH.to_string(),
            config_path,
            state_dir: cfg.core.state_dir.display().to_string(),
            personality_path: cfg.core.personality_path.clone(),
            ollama_url: cfg.inference.ollama_base_url.clone(),
            fast_model: self.fast_model,
            primary_model: self.primary_model,
            embed_model: self.embed_model,
            fast_model_warm: self.fast_model_warm,
            primary_model_warm: self.primary_model_warm,
            embed_model_warm: self.embed_model_warm,
            stt_worker: self.stt_worker.as_str().to_string(),
            tts_worker: self.tts_worker.as_str().to_string(),
            browser_worker: self.browser_worker.as_str().to_string(),
            browser_worker_detail: self.browser_worker_detail,
            browser_worker_recovery_hint: self.browser_worker_recovery_hint,
            disk: self.disk.into_iter().map(disk_health_proto).collect(),
            operator_context_markdown: String::new(),
            residency_mode: cfg.residency.mode.as_str().to_string(),
            primary_residency_pinned: residency.primary_pinned,
            primary_residency_wired_bytes: residency.primary_wired_bytes as u64,
            residency_lock_poisoned: residency.lock_poisoned,
        }
    }
}

fn disk_health_proto(snapshot: DiskHealthSnapshot) -> DiskHealth {
    DiskHealth {
        name: snapshot.name,
        path: snapshot.path,
        status: snapshot.status.as_str().to_string(),
        available_bytes: snapshot.available_bytes,
        total_bytes: snapshot.total_bytes,
        detail: snapshot.detail,
    }
}

fn worker_startup_status(is_ready: bool, startup_warmup_complete: bool) -> ComponentStartupStatus {
    if is_ready {
        ComponentStartupStatus::Ready
    } else if startup_warmup_complete {
        ComponentStartupStatus::Degraded
    } else {
        ComponentStartupStatus::Pending
    }
}

fn model_startup_status(is_warm: bool, startup_warmup_complete: bool) -> ComponentStartupStatus {
    if is_warm {
        ComponentStartupStatus::Ready
    } else if startup_warmup_complete {
        ComponentStartupStatus::Degraded
    } else {
        ComponentStartupStatus::Pending
    }
}

fn stt_startup_status(
    stt_ready: &AtomicBool,
    stt_prewarm_complete: &AtomicBool,
) -> ComponentStartupStatus {
    if stt_ready.load(Ordering::SeqCst) {
        ComponentStartupStatus::Ready
    } else if stt_prewarm_complete.load(Ordering::SeqCst) {
        ComponentStartupStatus::Degraded
    } else {
        ComponentStartupStatus::Pending
    }
}

async fn log_startup_health_summary(
    cfg: Arc<DexterConfig>,
    shared: SharedDaemonState,
    stt_ready: Arc<AtomicBool>,
    stt_prewarm_complete: Arc<AtomicBool>,
) {
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(STARTUP_HEALTH_STT_WAIT_SECS);
    while !stt_prewarm_complete.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let stt_worker = stt_startup_status(&stt_ready, &stt_prewarm_complete);
    let snapshot = StartupHealthSnapshot::from_runtime(&cfg, &shared, stt_worker);
    let status = snapshot.overall_status();
    let degraded_components = snapshot.degraded_components_label();
    let disk_degraded_components = diagnostics::disk_degraded_label(&snapshot.disk);
    let config_path = resolve_config_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|e| format!("unresolved: {e}"));

    // Residency status — operator-visible "is the weight pin armed?" health line.
    let residency = shared.residency.status();
    info!(
        mode = cfg.residency.mode.as_str(),
        primary_pinned = residency.primary_pinned,
        primary_wired_gb = residency.primary_wired_bytes as f64 / 1_073_741_824.0,
        lock_poisoned = residency.lock_poisoned,
        "Residency status"
    );

    if status == "ready" {
        info!(
            status = %status,
            degraded_components = %degraded_components,
            version = CORE_VERSION,
            socket = %cfg.core.socket_path,
            shell_socket = SHELL_SOCKET_PATH,
            config_path = %config_path,
            state_dir = %cfg.core.state_dir.display(),
            personality_path = %cfg.core.personality_path,
            ollama_url = %cfg.inference.ollama_base_url,
            fast_model = %snapshot.fast_model,
            primary_model = %snapshot.primary_model,
            embed_model = %snapshot.embed_model,
            fast_model_warm = snapshot.fast_model_warm,
            primary_model_warm = snapshot.primary_model_warm,
            embed_model_warm = snapshot.embed_model_warm,
            stt_worker = snapshot.stt_worker.as_str(),
            tts_worker = snapshot.tts_worker.as_str(),
            browser_worker = snapshot.browser_worker.as_str(),
            disk_degraded_components = %disk_degraded_components,
            "Dexter startup health summary"
        );
    } else if status == "pending" {
        info!(
            status = %status,
            degraded_components = %degraded_components,
            version = CORE_VERSION,
            socket = %cfg.core.socket_path,
            shell_socket = SHELL_SOCKET_PATH,
            config_path = %config_path,
            state_dir = %cfg.core.state_dir.display(),
            personality_path = %cfg.core.personality_path,
            ollama_url = %cfg.inference.ollama_base_url,
            fast_model = %snapshot.fast_model,
            primary_model = %snapshot.primary_model,
            embed_model = %snapshot.embed_model,
            fast_model_warm = snapshot.fast_model_warm,
            primary_model_warm = snapshot.primary_model_warm,
            embed_model_warm = snapshot.embed_model_warm,
            stt_worker = snapshot.stt_worker.as_str(),
            tts_worker = snapshot.tts_worker.as_str(),
            browser_worker = snapshot.browser_worker.as_str(),
            disk_degraded_components = %disk_degraded_components,
            "Dexter startup health summary — warmup still pending"
        );
    } else {
        warn!(
            status = %status,
            degraded_components = %degraded_components,
            version = CORE_VERSION,
            socket = %cfg.core.socket_path,
            shell_socket = SHELL_SOCKET_PATH,
            config_path = %config_path,
            state_dir = %cfg.core.state_dir.display(),
            personality_path = %cfg.core.personality_path,
            ollama_url = %cfg.inference.ollama_base_url,
            fast_model = %snapshot.fast_model,
            primary_model = %snapshot.primary_model,
            embed_model = %snapshot.embed_model,
            fast_model_warm = snapshot.fast_model_warm,
            primary_model_warm = snapshot.primary_model_warm,
            embed_model_warm = snapshot.embed_model_warm,
            stt_worker = snapshot.stt_worker.as_str(),
            tts_worker = snapshot.tts_worker.as_str(),
            browser_worker = snapshot.browser_worker.as_str(),
            disk_degraded_components = %disk_degraded_components,
            "Dexter startup health summary — degraded components present"
        );
    }
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
    /// STT prewarm readiness for health diagnostics. Kept outside the worker mutex so
    /// `Health()` can report status without blocking on an active utterance.
    stt_ready: Arc<AtomicBool>,
    stt_prewarm_complete: Arc<AtomicBool>,
    ambient_store: AmbientEventStore,
    last_ambient_health_status: StdMutex<Option<String>>,
}

impl CoreService {
    pub fn new(cfg: Arc<DexterConfig>) -> Self {
        let stt: Arc<Mutex<Option<WorkerClient>>> = Arc::new(Mutex::new(None));
        let stt_ready = Arc::new(AtomicBool::new(false));
        let stt_prewarm_complete = Arc::new(AtomicBool::new(false));
        // Pre-warm the STT worker in the background — model load takes ~8 s.
        // stream_audio() falls back to on-demand spawn if this hasn't completed.
        let stt_warm = stt.clone();
        let stt_ready_warm = stt_ready.clone();
        let stt_prewarm_complete_warm = stt_prewarm_complete.clone();
        tokio::spawn(async move {
            match WorkerClient::spawn(WorkerType::Stt, VOICE_PYTHON_EXE, VOICE_STT_WORKER_PATH)
                .await
            {
                Ok(client) => {
                    *stt_warm.lock().await = Some(client);
                    stt_ready_warm.store(true, Ordering::SeqCst);
                    info!("STT worker pre-warmed and ready");
                }
                Err(e) => warn!(error = %e, "STT pre-warm failed — will spawn on first utterance"),
            }
            stt_prewarm_complete_warm.store(true, Ordering::SeqCst);
        });

        // Phase 38c: construct daemon-lifetime shared state and spawn the
        // startup warmup task BEFORE any session can connect. New sessions
        // inherit clones of this state and skip warmup entirely.
        let shared = crate::orchestrator::SharedDaemonState::new_degraded();
        let shared_for_warmup = shared.clone();
        let cfg_for_warmup = cfg.clone();
        let stt_ready_for_summary = stt_ready.clone();
        let stt_prewarm_complete_for_summary = stt_prewarm_complete.clone();
        tokio::spawn(async move {
            shared_for_warmup
                .run_startup_warmup(cfg_for_warmup.clone())
                .await;
            log_startup_health_summary(
                cfg_for_warmup,
                shared_for_warmup,
                stt_ready_for_summary,
                stt_prewarm_complete_for_summary,
            )
            .await;
        });

        let ambient_store = AmbientEventStore::new(&cfg.core.state_dir);
        match ambient_store.ensure_default_triggers() {
            Ok(installed) if !installed.is_empty() => {
                info!(
                    installed = installed.len(),
                    "Ambient default triggers installed"
                );
            }
            Ok(_) => {}
            Err(error) => {
                warn!(error = %error, "Ambient default triggers could not be installed");
            }
        }
        if let Err(error) = ambient_store.record_event_and_evaluate(
            "daemon",
            "daemon_started",
            AmbientSeverity::Info,
            "Dexter daemon started",
            "Dexter core started and is preparing workers, models, and IPC.",
            serde_json::json!({
                "socket_path": cfg.core.socket_path,
                "state_dir": cfg.core.state_dir.display().to_string()
            }),
        ) {
            warn!(error = %error, "Ambient daemon-start event could not be recorded");
        }

        let service = Self {
            cfg,
            stt,
            orchestrator_tx: Arc::new(Mutex::new(None)),
            shared,
            stt_ready,
            stt_prewarm_complete,
            ambient_store,
            last_ambient_health_status: StdMutex::new(None),
        };

        // Phase 30: spawn shell context listener. Accepts one-shot connections from the
        // zsh integration hook at SHELL_SOCKET_PATH and forwards parsed events to the
        // active session via orchestrator_tx. Non-fatal if bind fails (shell context
        // is degraded but the service still starts).
        let shell_tx = service.orchestrator_tx.clone();
        tokio::spawn(run_shell_listener(shell_tx));

        service
    }

    fn health_response_for_trace(&self, trace_id: String) -> HealthResponse {
        let stt_worker = stt_startup_status(&self.stt_ready, &self.stt_prewarm_complete);
        let snapshot = StartupHealthSnapshot::from_runtime(&self.cfg, &self.shared, stt_worker);
        let config_path = resolve_config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|e| format!("unresolved: {e}"));
        let mut health = snapshot.into_health_response(
            trace_id,
            &self.cfg,
            config_path,
            self.shared.residency.status(),
        );
        health.operator_context_markdown = self.shared.operator_context_markdown();
        self.record_health_transition(&health);
        health
    }

    fn record_health_transition(&self, health: &HealthResponse) {
        let status = health.status.trim();
        if status.is_empty() {
            return;
        }

        let should_record = match self.last_ambient_health_status.lock() {
            Ok(mut guard) => {
                if guard.as_deref() == Some(status) {
                    false
                } else {
                    *guard = Some(status.to_string());
                    true
                }
            }
            Err(error) => {
                warn!(
                    error = %error,
                    "Ambient health transition lock poisoned — event not recorded"
                );
                false
            }
        };

        if !should_record {
            return;
        }

        let severity = match status {
            "ready" => AmbientSeverity::Info,
            "pending" => AmbientSeverity::Warn,
            "degraded" => AmbientSeverity::Critical,
            _ => AmbientSeverity::Warn,
        };
        let title = format!("Dexter health {status}");
        let summary = if health.degraded_components.is_empty() {
            format!("Daemon health changed to {status}.")
        } else {
            format!(
                "Daemon health changed to {status}; attention components: {}.",
                health.degraded_components.join(", ")
            )
        };

        if let Err(error) = self.ambient_store.record_event_and_evaluate(
            "health",
            "health_status_changed",
            severity,
            title,
            summary,
            serde_json::json!({
                "status": health.status,
                "degraded_components": health.degraded_components,
                "fast_model": health.fast_model,
                "primary_model": health.primary_model,
                "embed_model": health.embed_model,
                "stt_worker": health.stt_worker,
                "tts_worker": health.tts_worker,
                "browser_worker": health.browser_worker
            }),
        ) {
            warn!(error = %error, "Ambient health transition event could not be recorded");
        }
    }

    fn record_component_restart_event(
        &self,
        component: RestartComponent,
        success: bool,
        message: &str,
        trace_id: &str,
    ) {
        let component_name = restart_component_label(component);
        let severity = if success {
            AmbientSeverity::Info
        } else {
            AmbientSeverity::Warn
        };
        let title = if success {
            format!("{component_name} restarted")
        } else {
            format!("{component_name} restart failed")
        };

        if let Err(error) = self.ambient_store.record_event_and_evaluate(
            "operator",
            "component_restarted",
            severity,
            title,
            message.to_string(),
            serde_json::json!({
                "component": component_name,
                "success": success,
                "trace_id": trace_id
            }),
        ) {
            warn!(error = %error, "Ambient component restart event could not be recorded");
        }
    }

    async fn restart_stt_now(&self) -> bool {
        self.stt_ready.store(false, Ordering::SeqCst);
        self.stt_prewarm_complete.store(false, Ordering::SeqCst);

        let existing = self.stt.lock().await.take();
        if let Some(client) = existing {
            client.shutdown().await;
        }

        match WorkerClient::spawn(WorkerType::Stt, VOICE_PYTHON_EXE, VOICE_STT_WORKER_PATH).await {
            Ok(client) => {
                *self.stt.lock().await = Some(client);
                self.stt_ready.store(true, Ordering::SeqCst);
                self.stt_prewarm_complete.store(true, Ordering::SeqCst);
                info!("STT worker restarted by operator request");
                true
            }
            Err(e) => {
                self.stt_prewarm_complete.store(true, Ordering::SeqCst);
                warn!(error = %e, "STT worker restart failed by operator request");
                false
            }
        }
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

    async fn health(
        &self,
        request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        let trace_id = request.into_inner().trace_id;
        let health = self.health_response_for_trace(trace_id.clone());

        info!(
            trace_id = %trace_id,
            status = %health.status,
            degraded_components = %health.degraded_components.join(","),
            "Health snapshot requested"
        );

        Ok(Response::new(health))
    }

    async fn action_history(
        &self,
        request: Request<ActionHistoryRequest>,
    ) -> Result<Response<ActionHistoryResponse>, Status> {
        let req = request.into_inner();
        let trace_id = req.trace_id;
        let limit = if req.limit == 0 {
            20
        } else {
            req.limit.min(100)
        } as usize;

        let (audit_log_path, receipts) = recent_action_receipts(&self.cfg.core.state_dir, limit)
            .map_err(|e| {
                error!(
                    trace_id = %trace_id,
                    error = %e,
                    "Action history read failed"
                );
                Status::internal(format!("action history unavailable: {e}"))
            })?;
        let receipt_count = receipts.len();
        let latest_action_summary_markdown = latest_action_summary_markdown(&receipts);
        let receipts = receipts
            .into_iter()
            .map(|receipt| ActionReceipt {
                action_id: receipt.action_id,
                action_type: receipt.action_type,
                category: receipt.category,
                description: receipt.description,
                outcome: receipt.outcome,
                summary: receipt.summary,
                audit_log_path: audit_log_path.display().to_string(),
            })
            .collect();

        info!(
            trace_id = %trace_id,
            limit,
            receipt_count,
            audit_log_path = %audit_log_path.display(),
            "Action history requested"
        );

        Ok(Response::new(ActionHistoryResponse {
            trace_id,
            audit_log_path: audit_log_path.display().to_string(),
            receipts,
            latest_action_summary_markdown,
        }))
    }

    async fn ambient_history(
        &self,
        request: Request<AmbientHistoryRequest>,
    ) -> Result<Response<AmbientHistoryResponse>, Status> {
        let req = request.into_inner();
        let trace_id = req.trace_id;
        let limit = if req.limit == 0 {
            20
        } else {
            req.limit.min(100)
        } as usize;

        let (event_log_path, events) = self.ambient_store.recent_events(limit).map_err(|e| {
            error!(
                trace_id = %trace_id,
                error = %e,
                "Ambient history read failed"
            );
            Status::internal(format!("ambient history unavailable: {e}"))
        })?;
        let event_count = events.len();
        let events = events
            .into_iter()
            .map(|event| ambient_event_proto(event, &trace_id))
            .collect();

        info!(
            trace_id = %trace_id,
            limit,
            event_count,
            event_log_path = %event_log_path.display(),
            "Ambient history requested"
        );

        Ok(Response::new(AmbientHistoryResponse {
            trace_id,
            event_log_path: event_log_path.display().to_string(),
            events,
        }))
    }

    async fn ambient_inbox(
        &self,
        request: Request<AmbientInboxRequest>,
    ) -> Result<Response<AmbientInboxResponse>, Status> {
        let req = request.into_inner();
        let trace_id = req.trace_id;
        let limit = if req.limit == 0 { 5 } else { req.limit.min(50) } as usize;

        let (event_log_path, events) =
            self.ambient_store
                .unread_trigger_matches(limit)
                .map_err(|e| {
                    error!(
                        trace_id = %trace_id,
                        error = %e,
                        "Ambient inbox read failed"
                    );
                    Status::internal(format!("ambient inbox unavailable: {e}"))
                })?;
        let event_count = events.len();
        let events = events
            .into_iter()
            .map(|event| ambient_event_proto(event, &trace_id))
            .collect();
        let acknowledgement_path = self.ambient_store.acknowledgements_path();

        info!(
            trace_id = %trace_id,
            limit,
            event_count,
            event_log_path = %event_log_path.display(),
            acknowledgement_path = %acknowledgement_path.display(),
            "Ambient inbox requested"
        );

        Ok(Response::new(AmbientInboxResponse {
            trace_id,
            event_log_path: event_log_path.display().to_string(),
            acknowledgement_path: acknowledgement_path.display().to_string(),
            events,
        }))
    }

    async fn acknowledge_ambient_events(
        &self,
        request: Request<AcknowledgeAmbientEventsRequest>,
    ) -> Result<Response<AcknowledgeAmbientEventsResponse>, Status> {
        let req = request.into_inner();
        let trace_id = req.trace_id;
        let requested_count = req.event_ids.len();
        let (acknowledgement_path, newly_acknowledged_count) = self
            .ambient_store
            .acknowledge_events(&req.event_ids)
            .map_err(|e| {
                error!(
                    trace_id = %trace_id,
                    error = %e,
                    requested_count,
                    "Ambient acknowledgement failed"
                );
                Status::internal(format!("ambient acknowledgement unavailable: {e}"))
            })?;

        info!(
            trace_id = %trace_id,
            requested_count,
            newly_acknowledged_count,
            acknowledgement_path = %acknowledgement_path.display(),
            "Ambient events acknowledged"
        );

        Ok(Response::new(AcknowledgeAmbientEventsResponse {
            trace_id,
            acknowledgement_path: acknowledgement_path.display().to_string(),
            newly_acknowledged_count: newly_acknowledged_count as u32,
        }))
    }

    async fn action_diagnostic(
        &self,
        request: Request<ActionDiagnosticRequest>,
    ) -> Result<Response<ActionDiagnosticResponse>, Status> {
        let req = request.into_inner();
        let trace_id = req.trace_id;
        let limit = if req.limit == 0 {
            3
        } else {
            req.limit.min(100)
        } as usize;
        let health = self.health_response_for_trace(trace_id.clone());
        let health_warnings = action_diagnostic_health_warnings(&health);

        let report = build_action_diagnostic(ActionDiagnosticInput {
            state_dir: &self.cfg.core.state_dir,
            limit,
            current_user_text: nonempty_string(req.current_user_text),
            current_assistant_text: nonempty_string(req.current_assistant_text),
            health_warnings,
            only_if_clue: req.only_if_clue,
            ignore_action_receipts: req.ignore_action_receipts,
        })
        .map_err(|e| {
            error!(
                trace_id = %trace_id,
                error = %e,
                "Action diagnostic failed"
            );
            Status::internal(format!("action diagnostic unavailable: {e}"))
        })?;

        let receipt_count = report.receipts.len();
        let receipts = report
            .receipts
            .into_iter()
            .map(|receipt| ActionReceipt {
                action_id: receipt.action_id,
                action_type: receipt.action_type,
                category: receipt.category,
                description: receipt.description,
                outcome: receipt.outcome,
                summary: receipt.summary,
                audit_log_path: report.audit_log_path.display().to_string(),
            })
            .collect();

        info!(
            trace_id = %trace_id,
            limit,
            receipt_count,
            has_session_clue = report.has_session_clue,
            has_diagnostic = report.has_diagnostic,
            cause = %report.cause,
            audit_log_path = %report.audit_log_path.display(),
            "Action diagnostic requested"
        );

        Ok(Response::new(ActionDiagnosticResponse {
            trace_id,
            markdown: report.markdown,
            cause: report.cause,
            audit_log_path: report.audit_log_path.display().to_string(),
            has_session_clue: report.has_session_clue,
            has_diagnostic: report.has_diagnostic,
            receipts,
        }))
    }

    async fn restart_component(
        &self,
        request: Request<RestartComponentRequest>,
    ) -> Result<Response<RestartComponentResponse>, Status> {
        let req = request.into_inner();
        let trace_id = req.trace_id;
        let component =
            RestartComponent::try_from(req.component).unwrap_or(RestartComponent::Unspecified);

        info!(
            trace_id = %trace_id,
            component = ?component,
            "Component restart requested"
        );

        let (success, message) = match component {
            RestartComponent::Stt => {
                let success = self.restart_stt_now().await;
                (
                    success,
                    if success {
                        "STT worker restarted".to_string()
                    } else {
                        "STT worker restart failed".to_string()
                    },
                )
            }
            RestartComponent::Tts => {
                let success = self.shared.voice.restart_tts_now().await;
                (
                    success,
                    if success {
                        "TTS worker restarted".to_string()
                    } else {
                        "TTS worker restart failed".to_string()
                    },
                )
            }
            RestartComponent::Browser => {
                let success = self.shared.browser.restart_now().await;
                (
                    success,
                    if success {
                        "Browser worker restarted".to_string()
                    } else {
                        "Browser worker restart failed".to_string()
                    },
                )
            }
            RestartComponent::Unspecified => {
                return Err(Status::invalid_argument(
                    "restart_component requires stt, tts, or browser",
                ));
            }
        };

        self.record_component_restart_event(component, success, &message, &trace_id);
        let health = self.health_response_for_trace(trace_id.clone());
        info!(
            trace_id = %trace_id,
            component = ?component,
            success,
            health_status = %health.status,
            "Component restart complete"
        );

        Ok(Response::new(RestartComponentResponse {
            trace_id,
            component: component as i32,
            success,
            message,
            health: Some(health),
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
        let stt_ready = self.stt_ready.clone();
        let stt_prewarm_complete = self.stt_prewarm_complete.clone();

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
                        stt_ready.store(true, Ordering::SeqCst);
                        stt_prewarm_complete.store(true, Ordering::SeqCst);
                    }
                    Err(e) => {
                        stt_prewarm_complete.store(true, Ordering::SeqCst);
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
                stt_ready.store(false, Ordering::SeqCst);
                stt_prewarm_complete.store(true, Ordering::SeqCst);
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
mod startup_health_tests {
    use crate::action::audit::ActionAuditReceipt;
    use crate::diagnostics::{DiskHealthSnapshot, DiskStatus};

    use super::{
        latest_action_summary_markdown, stt_startup_status, worker_startup_status,
        ComponentStartupStatus, DexterConfig, StartupHealthSnapshot,
    };
    use std::sync::atomic::AtomicBool;

    #[test]
    fn component_status_labels_are_stable() {
        assert_eq!(ComponentStartupStatus::Ready.as_str(), "ready");
        assert_eq!(ComponentStartupStatus::Degraded.as_str(), "degraded");
        assert_eq!(ComponentStartupStatus::Pending.as_str(), "pending");
    }

    #[test]
    fn stt_startup_status_distinguishes_pending_from_degraded() {
        let ready = AtomicBool::new(false);
        let complete = AtomicBool::new(false);
        assert_eq!(
            stt_startup_status(&ready, &complete),
            ComponentStartupStatus::Pending
        );

        complete.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            stt_startup_status(&ready, &complete),
            ComponentStartupStatus::Degraded
        );

        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            stt_startup_status(&ready, &complete),
            ComponentStartupStatus::Ready
        );
    }

    #[test]
    fn worker_startup_status_distinguishes_pending_from_degraded() {
        assert_eq!(
            worker_startup_status(false, false),
            ComponentStartupStatus::Pending
        );
        assert_eq!(
            worker_startup_status(false, true),
            ComponentStartupStatus::Degraded
        );
        assert_eq!(
            worker_startup_status(true, false),
            ComponentStartupStatus::Ready
        );
    }

    #[test]
    fn startup_health_snapshot_reports_degraded_components() {
        let snapshot = StartupHealthSnapshot {
            fast_model: "fast".to_string(),
            primary_model: "primary".to_string(),
            embed_model: "embed".to_string(),
            fast_model_warm: true,
            primary_model_warm: false,
            embed_model_warm: true,
            startup_warmup_complete: true,
            stt_worker: ComponentStartupStatus::Ready,
            tts_worker: ComponentStartupStatus::Degraded,
            browser_worker: ComponentStartupStatus::Ready,
            browser_worker_detail: String::new(),
            browser_worker_recovery_hint: String::new(),
            disk: Vec::new(),
        };

        assert_eq!(snapshot.overall_status(), "degraded");
        assert_eq!(
            snapshot.degraded_components(),
            vec!["primary_model".to_string(), "tts_worker".to_string()]
        );
        assert_eq!(
            snapshot.degraded_components_label(),
            "primary_model,tts_worker"
        );
    }

    #[test]
    fn startup_health_snapshot_ready_when_no_components_degraded() {
        let snapshot = StartupHealthSnapshot {
            fast_model: "fast".to_string(),
            primary_model: "primary".to_string(),
            embed_model: "embed".to_string(),
            fast_model_warm: true,
            primary_model_warm: true,
            embed_model_warm: true,
            startup_warmup_complete: true,
            stt_worker: ComponentStartupStatus::Ready,
            tts_worker: ComponentStartupStatus::Ready,
            browser_worker: ComponentStartupStatus::Ready,
            browser_worker_detail: String::new(),
            browser_worker_recovery_hint: String::new(),
            disk: Vec::new(),
        };

        assert_eq!(snapshot.overall_status(), "ready");
        assert!(snapshot.degraded_components().is_empty());
        assert_eq!(snapshot.degraded_components_label(), "none");
    }

    #[test]
    fn startup_health_snapshot_reports_pending_before_warmup_attempt_finishes() {
        let snapshot = StartupHealthSnapshot {
            fast_model: "fast".to_string(),
            primary_model: "primary".to_string(),
            embed_model: "embed".to_string(),
            fast_model_warm: false,
            primary_model_warm: false,
            embed_model_warm: false,
            startup_warmup_complete: false,
            stt_worker: ComponentStartupStatus::Pending,
            tts_worker: ComponentStartupStatus::Ready,
            browser_worker: ComponentStartupStatus::Ready,
            browser_worker_detail: String::new(),
            browser_worker_recovery_hint: String::new(),
            disk: Vec::new(),
        };

        assert_eq!(snapshot.overall_status(), "pending");
        assert!(!snapshot.has_degraded_components());
        assert!(snapshot.has_pending_components());
        assert_eq!(
            snapshot.degraded_components(),
            vec![
                "fast_model".to_string(),
                "primary_model".to_string(),
                "embed_model".to_string(),
                "stt_worker".to_string(),
            ]
        );
    }

    #[test]
    fn startup_health_snapshot_includes_disk_pressure_as_degraded() {
        let snapshot = StartupHealthSnapshot {
            fast_model: "fast".to_string(),
            primary_model: "primary".to_string(),
            embed_model: "embed".to_string(),
            fast_model_warm: true,
            primary_model_warm: true,
            embed_model_warm: true,
            startup_warmup_complete: true,
            stt_worker: ComponentStartupStatus::Ready,
            tts_worker: ComponentStartupStatus::Ready,
            browser_worker: ComponentStartupStatus::Ready,
            browser_worker_detail: String::new(),
            browser_worker_recovery_hint: String::new(),
            disk: vec![DiskHealthSnapshot {
                name: "workspace".to_string(),
                path: "/Users/jason/Developer/Dex".to_string(),
                status: DiskStatus::Warn,
                available_bytes: 1024 * 1024 * 1024,
                total_bytes: 100 * 1024 * 1024 * 1024,
                detail: "below warning threshold".to_string(),
            }],
        };

        assert_eq!(snapshot.overall_status(), "degraded");
        assert_eq!(
            snapshot.degraded_components(),
            vec!["disk:workspace".to_string()]
        );
        assert_eq!(snapshot.degraded_components_label(), "disk:workspace");
    }

    #[test]
    fn startup_health_snapshot_builds_health_response() {
        let snapshot = StartupHealthSnapshot {
            fast_model: "fast".to_string(),
            primary_model: "primary".to_string(),
            embed_model: "embed".to_string(),
            fast_model_warm: true,
            primary_model_warm: true,
            embed_model_warm: false,
            startup_warmup_complete: true,
            stt_worker: ComponentStartupStatus::Ready,
            tts_worker: ComponentStartupStatus::Ready,
            browser_worker: ComponentStartupStatus::Degraded,
            browser_worker_detail: "browser_launch_failed: Executable doesn't exist".to_string(),
            browser_worker_recovery_hint:
                "Install Playwright Chromium, then restart the browser worker.".to_string(),
            disk: Vec::new(),
        };
        let cfg = DexterConfig::default();

        let response = snapshot.into_health_response(
            "trace-123".to_string(),
            &cfg,
            "/Users/jason/.dexter/config.toml".to_string(),
            crate::system::residency::ResidencyStatus {
                primary_pinned: true,
                primary_wired_bytes: 123,
                lock_poisoned: false,
            },
        );

        assert_eq!(response.trace_id, "trace-123");
        assert_eq!(response.status, "degraded");
        assert_eq!(
            response.degraded_components,
            vec!["embed_model".to_string(), "browser_worker".to_string()]
        );
        assert_eq!(response.embed_model, "embed");
        assert_eq!(response.browser_worker, "degraded");
        assert_eq!(
            response.browser_worker_detail,
            "browser_launch_failed: Executable doesn't exist"
        );
        assert!(response
            .browser_worker_recovery_hint
            .contains("Install Playwright Chromium"));
        assert_eq!(response.config_path, "/Users/jason/.dexter/config.toml");
        assert_eq!(response.residency_mode, "pin_keepalive");
        assert!(response.primary_residency_pinned);
        assert_eq!(response.primary_residency_wired_bytes, 123);
        assert!(!response.residency_lock_poisoned);
    }

    #[test]
    fn latest_action_summary_markdown_formats_success_receipt() {
        let receipts = vec![ActionAuditReceipt {
            action_id: "act-1".to_string(),
            action_type: "shell".to_string(),
            category: "safe".to_string(),
            description: "Run: echo hi".to_string(),
            outcome: "executed".to_string(),
            summary: "Succeeded: hi".to_string(),
        }];

        let markdown = latest_action_summary_markdown(&receipts);

        assert!(markdown.contains("The latest audited action executed successfully."));
        assert!(markdown.contains("Evidence: Succeeded: hi"));
        assert!(markdown.contains("Target: Run: echo hi"));
    }

    #[test]
    fn latest_action_summary_markdown_formats_failed_receipt() {
        let receipts = vec![ActionAuditReceipt {
            action_id: "act-2".to_string(),
            action_type: "message_send".to_string(),
            category: "cautious".to_string(),
            description: "Send iMessage to: Jason".to_string(),
            outcome: "failed".to_string(),
            summary:
                "Failed: message_send actions must be resolved by the orchestrator before execution"
                    .to_string(),
        }];

        let markdown = latest_action_summary_markdown(&receipts);

        assert!(markdown.contains("raw message_send action was blocked"));
        assert!(markdown.contains("Evidence: Failed: message_send actions"));
        assert!(markdown.contains("Target: Send iMessage to: Jason"));
        assert!(markdown.contains("Next step: Ask again using the recipient's exact Contacts name"));
    }

    #[test]
    fn latest_action_summary_markdown_handles_empty_history() {
        assert_eq!(
            latest_action_summary_markdown(&[]),
            "- No recent action receipt was found.\n"
        );
    }
}

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
