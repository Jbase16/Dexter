/// Tracing subscriber initialization for the Dexter core daemon.
///
/// Called once in `main` as the very first action, before any other component.
/// Extracting this from `main.rs` makes it reusable — Phase 6's orchestrator
/// can reset log spans on session restart without touching `main`.
///
/// Format selection:
/// - `LogFormat::Json`   → structured JSON (log aggregation, file piping, production)
/// - `LogFormat::Pretty` → ANSI-colored human-readable (interactive dev sessions)
/// - `LogFormat::Auto`   → JSON when stdout is not a TTY, pretty when it is
///
/// TTY detection: `std::io::IsTerminal` from stdlib (stable since Rust 1.70).
/// The `atty` crate is intentionally excluded — this is now in stdlib, adding an
/// external dependency for it would be pure overhead.
///
/// Log level: from `config.logging.level`, parsed into `EnvFilter`. The `RUST_LOG`
/// environment variable takes precedence if set — this is standard Rust convention
/// and allows per-component filtering during debugging without modifying config.
use anyhow::Result;
use tracing_subscriber::{fmt, EnvFilter};

use crate::config::LoggingConfig;

/// Initialize the global tracing subscriber.
///
/// Must be called exactly once, before any `tracing::info!` / `tracing::debug!`
/// etc. calls. Subsequent calls would panic (tracing enforces a single global
/// subscriber). Never panics itself — returns `anyhow::Result` so the caller
/// can decide how to handle initialization failure.
///
/// `RUST_LOG` env var overrides `config.logging.level` if set, following the
/// standard `tracing_subscriber` convention for per-component log filtering
/// (e.g. `RUST_LOG=dexter_core=trace,tonic=debug`).
pub fn init(config: &LoggingConfig) -> Result<()> {
    // `EnvFilter::try_from_default_env()` reads `RUST_LOG`. If absent, fall back
    // to the config-specified level applied to all targets (not crate-scoped —
    // the operator controls that via `RUST_LOG` if they need granular control).
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(config.level.as_str()));

    if config.use_json() {
        // JSON format: one JSON object per log line, with timestamp, level,
        // target, and all structured fields. Suitable for:
        // - Piping to log aggregators (Datadog, Loki, CloudWatch)
        // - `make run-core > core.log 2>&1` during development
        // - Any non-interactive context where stdout is not a TTY
        fmt()
            .json()
            .with_env_filter(filter)
            .with_target(true)
            .with_current_span(false) // spans not yet used; avoids empty span noise
            .try_init()
            .map_err(|e| anyhow::anyhow!("Failed to initialize JSON tracing subscriber: {}", e))?;
    } else {
        // Pretty format: ANSI-colored, human-readable output.
        // Suitable for interactive `cargo run` / `make run-core` sessions in a terminal.
        fmt()
            .pretty()
            .with_env_filter(filter)
            .with_target(true)
            .try_init()
            .map_err(|e| {
                anyhow::anyhow!("Failed to initialize pretty tracing subscriber: {}", e)
            })?;
    }

    Ok(())
}
