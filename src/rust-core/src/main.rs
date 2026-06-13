mod action;
mod action_diagnostic;
mod action_evidence;
mod ambient;
mod browser;
mod config;
mod constants;
mod context_observer;
mod diagnostics;
mod humor;
mod inference;
mod ipc;
mod logging;
mod memory;
mod operator_context;
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
    // ── Residency proof / pinner roles ────────────────────────────────────────
    //
    // Handled before anything else so they run as lightweight, self-contained
    // processes (no gRPC, no Ollama dependency). These exercise the production
    // `system::residency` module:
    //   --prove-residency [model]      run the cross-process pinning proof and exit
    //   --residency-pin-child <p> <s>  (hidden) pin a blob and hold it for the proof
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "--residency-pin-child") {
            let path = args.get(pos + 1).cloned().unwrap_or_default();
            let secs = args
                .get(pos + 2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(600);
            system::residency::run_pin_child(&path, secs);
            return Ok(());
        }
        if let Some(pos) = args.iter().position(|a| a == "--prove-residency") {
            let model = args.get(pos + 1).filter(|s| !s.starts_with('-')).cloned();
            system::residency::run_proof(model)?;
            return Ok(());
        }
    }

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

    // Phase 40: disk pressure is now an explicit startup diagnostic. It never
    // blocks daemon startup, but it surfaces the exact failure class that can
    // make session persistence, worker caches, or local builds fail later.
    let startup_disk = diagnostics::collect_operator_disk_health(&cfg.core.state_dir);
    diagnostics::log_disk_health(&startup_disk);

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
    // against an explicit shutdown-signal future so SIGINT (Ctrl+C) and SIGTERM
    // both run the daemon cleanup path.
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
        signal = shutdown_signal() => {
            info!(signal, "Shutdown signal received — daemon exiting");
        }
    }

    cleanup_socket_file_on_exit(&cfg.core.socket_path, "gRPC socket");
    cleanup_socket_file_on_exit(constants::SHELL_SOCKET_PATH, "shell context socket");

    Ok(())
}

async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    warn!(
                        error = %error,
                        "Failed to install SIGTERM handler; falling back to Ctrl+C-only shutdown"
                    );
                    return wait_for_ctrl_c().await;
                }
            };

        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                match result {
                    Ok(()) => "SIGINT",
                    Err(error) => {
                        warn!(error = %error, "Ctrl+C signal listener failed");
                        "signal-listener-error"
                    }
                }
            }
            _ = sigterm.recv() => "SIGTERM",
        }
    }

    #[cfg(not(unix))]
    {
        wait_for_ctrl_c().await
    }
}

async fn wait_for_ctrl_c() -> &'static str {
    match tokio::signal::ctrl_c().await {
        Ok(()) => "SIGINT",
        Err(error) => {
            warn!(error = %error, "Ctrl+C signal listener failed");
            "signal-listener-error"
        }
    }
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

fn cleanup_socket_file_on_exit(path: &str, label: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => info!(path, label, "Removed socket file on daemon exit"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            warn!(path, label, error = %error, "Could not remove socket file on daemon exit")
        }
    }
}
