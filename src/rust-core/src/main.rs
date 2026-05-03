mod action;
mod browser;
mod config;
mod constants;
mod context_observer;
mod inference;
mod ipc;
mod logging;
mod memory;
mod orchestrator;
mod personality;
mod proactive;
mod retrieval;
mod session;
mod system;
mod voice;

use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info, warn};

use session::SessionStateManager;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging with defaults before loading config.
    //
    // This solves the config-logging bootstrap problem: config.rs uses `tracing::error!`
    // to report malformed TOML, which requires the subscriber to already be initialized.
    // Defaults (Auto format = JSON when not a TTY, INFO level) are correct for all
    // production use and most development use. The operator's `[logging]` config section
    // takes effect from the first log line onward — not from before config is loaded.
    //
    // Tracing enforces a single global subscriber, so there is no re-initialization
    // after config load. If an operator needs a different log level during startup they
    // can set RUST_LOG (which overrides config via EnvFilter::try_from_default_env).
    logging::init(&config::LoggingConfig::default())?;

    // Load config after logging is initialized so malformed-TOML errors are captured.
    // config::load() handles all three cases: absent (defaults), valid, malformed (exit 1).
    // Wrap in Arc immediately — CoreService in ipc::server holds an Arc<DexterConfig>
    // so sessions can be constructed cheaply without cloning the full struct.
    let cfg = Arc::new(config::load()?);

    info!(
        version     = constants::CORE_VERSION,
        socket      = %cfg.core.socket_path,
        config_dir  = %dirs::home_dir()
            .unwrap_or_default()
            .join(".dexter")
            .display(),
        // Log the operator's configured log level so they know if it differs from what's
        // active. Logging is initialized with defaults before config load (bootstrap
        // ordering) and cannot be re-initialized. Operator can override via RUST_LOG.
        configured_log_level = cfg.logging.level.as_str(),
        "Dexter core starting"
    );

    // Create ~/.dexter/state/ (or the configured path) before anything writes to it.
    // Idempotent — no-op if the directory already exists.
    config::ensure_state_dir(&cfg.core.state_dir)?;
    info!(path = %cfg.core.state_dir.display(), "State directory ready");

    // ── Session bootstrap log ─────────────────────────────────────────────────
    //
    // Load the previous session state and log key fields so the operator can see
    // at a glance where the last session ended. This is observability only; the
    // orchestrator intentionally starts each live conversation with a fresh prompt
    // instead of replaying raw prior transcripts.
    if let Some(prev) = SessionStateManager::load_latest(&cfg.core.state_dir) {
        info!(
            prev_session_id = %prev.session_id,
            prev_phase      = %prev.build_phase.current,
            prev_turns      = prev.conversation_history.len(),
            "Previous session state found"
        );
    }

    // ── Inference engine startup health check ────────────────────────────────
    //
    // Construct the engine and probe Ollama reachability. The engine is dropped
    // after the health check — `CoreOrchestrator::new()` reconstructs it per session.
    // We do NOT fail hard if Ollama is unreachable: the daemon must be able to bind
    // the gRPC socket even if Ollama is not yet running. The operator (or the
    // orchestrator) can start Ollama later. The health check result is surfaced as a
    // structured log field so operators immediately see the Ollama status in startup logs.
    match inference::InferenceEngine::new(cfg.inference.clone()) {
        Err(e) => {
            warn!(error = %e, "Failed to construct InferenceEngine — Ollama may be misconfigured");
        }
        Ok(engine) => {
            match engine.list_available_models().await {
                Ok(models) => {
                    info!(
                        ollama_reachable = true,
                        model_count      = models.len(),
                        ollama_url       = %cfg.inference.ollama_base_url,
                        "Ollama reachable at startup"
                    );
                }
                Err(e) => {
                    warn!(
                        ollama_reachable = false,
                        ollama_url       = %cfg.inference.ollama_base_url,
                        error            = %e,
                        "Ollama not reachable at startup — inference will fail until Ollama is running"
                    );
                }
            }
            // Engine dropped here — InferenceEngine::new() is cheap so CoreOrchestrator
            // reconstructs it from cfg.inference when the first session opens.
        }
    }

    // ── Personality layer startup check ──────────────────────────────────────
    //
    // Load the personality profile and log the result. Missing or malformed YAML
    // falls back to built-in defaults — the daemon does not fail to start because
    // the personality file is absent. CoreOrchestrator loads it afresh per session.
    {
        // Phase 38 / Codex finding [33]: honor the operator's configured
        // personality_path from ~/.dexter/config.toml. Previously this used the
        // compile-time constant directly, silently ignoring the config knob.
        let layer = personality::PersonalityLayer::load_or_default_from(&cfg.core.personality_path);
        info!(
            personality_name    = %layer.profile().name,
            lora_adapter_loaded = layer.profile().lora_adapter_path.is_some(),
            personality_path    = %cfg.core.personality_path,
            "Personality layer ready"
        );
    }

    // Clean up stale socket from a previous crash, or bail if another instance is live.
    cleanup_stale_socket(&cfg.core.socket_path).await?;

    // ── Graceful shutdown via signal race ─────────────────────────────────────
    //
    // `ipc::serve()` blocks until the server exits or is interrupted. We race it
    // against `ctrl_c()` so a SIGINT (Ctrl+C) or SIGTERM gracefully stops the daemon.
    //
    // On signal receipt: the server future is dropped, which closes the listening socket.
    // The live session task sees inbound.message() return Err or Ok(None) on the next
    // await, exits its event loop, calls orchestrator.shutdown() to write session state,
    // and then drops _signal_done — causing the hold-open task to exit and close the
    // gRPC stream cleanly to the Swift client.
    //
    // State persistence happens in orchestrator.shutdown() inside the session task,
    // not here in main — so session state is always written before the daemon exits.
    tokio::select! {
        result = ipc::serve(&cfg.core.socket_path, cfg.clone()) => {
            if let Err(e) = result {
                error!(error = %e, "gRPC server exited with error");
            } else {
                info!("gRPC server exited normally");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown signal received (Ctrl+C / SIGTERM) — daemon exiting");
        }
    }

    Ok(())
}

/// Checks whether a stale socket file exists from a previous crash.
///
/// Strategy: attempt a real connection rather than just checking file existence.
/// A live socket answers; a stale one refuses. This is more reliable than a
/// file check, which would incorrectly treat a running instance as stale.
async fn cleanup_stale_socket(path: &str) -> Result<()> {
    if std::path::Path::new(path).exists() {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => {
                // Connection succeeded — another core instance is running.
                anyhow::bail!(
                    "Another Dexter core is already running at {}. \
                     Stop it before starting a new instance.",
                    path
                );
            }
            Err(_) => {
                // Connection refused — socket file is stale from a crash.
                std::fs::remove_file(path)?;
                info!(path, "Removed stale socket file");
            }
        }
    }
    Ok(())
}
