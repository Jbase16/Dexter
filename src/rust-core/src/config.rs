/// Runtime configuration loader for the Dexter core daemon.
///
/// Loads operator configuration from `~/.dexter/config.toml`. If the file is absent,
/// validated defaults are used and the absence is logged. If the file is present but
/// malformed, the process exits with a structured error — bad config is never silently
/// swallowed, because an operator who wrote a config file intends for it to be used.
///
/// Design decision — `impl Default` over `#[serde(default = "fn")]`:
/// Putting defaults in `impl Default` means they are testable in isolation without
/// invoking the TOML parser at all. A unit test can assert
/// `DexterConfig::default().models.fast == "qwen3:8b"` without touching the filesystem.
/// `#[serde(default)]` on each struct tells serde to call `Default::default()` when
/// the section is absent from the file, delegating to the same `impl Default`.
use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, error, info};

use crate::constants::{
    DEXTER_CONFIG_FILENAME, DEXTER_STATE_DIR, OLLAMA_BASE_URL, PERSONALITY_CONFIG_PATH, SOCKET_PATH,
};

// ── Top-level config ──────────────────────────────────────────────────────────

/// Complete operator configuration for the Dexter core.
///
/// All fields are optional in `config.toml` — absent fields fall back to defaults.
/// See `impl Default` for each sub-struct for the canonical default values.
#[derive(Debug, Deserialize)]
pub struct DexterConfig {
    #[serde(default)]
    pub core: CoreConfig,
    /// Ollama model identifiers for each inference tier. Consumed by Phase 4 InferenceEngine.
    #[serde(default)]
    #[allow(dead_code)] // read by inference::engine — warning clears in step 6
    pub models: ModelConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Ollama connection parameters and streaming behaviour. Consumed by Phase 4 InferenceEngine.
    #[serde(default)]
    #[allow(dead_code)] // read by inference::InferenceEngine::new() — warning clears in step 8
    pub inference: InferenceConfig,
    /// Runtime behavioural tuning. Phase 17 — proactive observation parameters.
    #[serde(default)]
    pub behavior: BehaviorConfig,
    /// Global activation hotkey parameters. Phase 18 — pushed to Swift via ConfigSync.
    #[serde(default)]
    pub hotkey: HotkeyConfig,
}

impl Default for DexterConfig {
    fn default() -> Self {
        Self {
            core: CoreConfig::default(),
            models: ModelConfig::default(),
            logging: LoggingConfig::default(),
            inference: InferenceConfig::default(),
            behavior: BehaviorConfig::default(),
            hotkey: HotkeyConfig::default(),
        }
    }
}

// ── [core] ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CoreConfig {
    /// Unix domain socket path. Overridable to use a non-default socket during
    /// integration tests (e.g., `/tmp/dexter-test.sock`) without recompiling.
    pub socket_path: String,
    /// Absolute path to the state directory. Defaults to `{home_dir}/.dexter/state`.
    /// Computed from `DEXTER_STATE_DIR` constant plus `home_dir()` if absent from config.
    pub state_dir: PathBuf,
    /// Path to the operator personality YAML profile.
    ///
    /// Loaded at startup by `main.rs` via `PersonalityLayer::load_or_default_from`,
    /// and again per-session by `CoreOrchestrator::new`. Defaults to
    /// `PERSONALITY_CONFIG_PATH` when omitted from `~/.dexter/config.toml`.
    /// Missing/malformed files fall back to built-in defaults rather than failing
    /// startup. Phase 38 / Codex finding [33] wired this through after it spent
    /// several phases as a `#[allow(dead_code)]` ghost knob.
    pub personality_path: String,
}

impl Default for CoreConfig {
    fn default() -> Self {
        // home_dir() returns None only in extremely unusual environments (no HOME var,
        // no passwd entry). Fall back to /tmp/.dexter/state rather than panic — the
        // daemon can still start, and the operator will notice the unusual path in logs.
        let state_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(DEXTER_STATE_DIR);

        Self {
            socket_path: SOCKET_PATH.to_string(),
            state_dir,
            personality_path: PERSONALITY_CONFIG_PATH.to_string(),
        }
    }
}

// ── [models] ──────────────────────────────────────────────────────────────────

/// Ollama model identifiers for each inference tier.
///
/// These are the Ollama model tags as they appear in `ollama list`. The InferenceEngine
/// uses these values to route requests to the correct model by name.
///
/// Per-field `#[serde(default = "fn")]` is required (not just struct-level `#[serde(default)]`)
/// because `String::default()` returns `""`, not the actual model name. When the operator
/// writes a partial `[models]` section, each absent field falls back to its named default
/// rather than an empty string.
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // fields read by inference::engine — warning clears in step 6
pub struct ModelConfig {
    #[serde(default = "default_model_fast")]
    pub fast: String,
    #[serde(default = "default_model_primary")]
    pub primary: String,
    #[serde(default = "default_model_heavy")]
    pub heavy: String,
    #[serde(default = "default_model_code")]
    pub code: String,
    #[serde(default = "default_model_vision")]
    pub vision: String,
    #[serde(default = "default_model_embed")]
    pub embed: String,
}

// Per-field serde default helpers. Must be plain functions (not closures, not methods)
// because `#[serde(default = "...")]` requires a function path.
// Defaults must match the authoritative model list in MEMORY.md and the
// models that the operator actually has pulled in Ollama. A default that
// references an unpulled model causes a 404 at warmup AND every subsequent
// query routed to that tier (see Phase 36 H1 follow-up, 2026-04-17).
//
// Phase 37 (2026-04-17): PRIMARY upgraded to gemma4:26b (MoE, 3.8B active per
// token — ~4B-class inference speed with ~97% of 31B dense quality). VISION
// aliased to the same model because Gemma 4 is natively multimodal. HEAVY stays
// on deepseek-r1:32b deliberately — DeepSeek's inherent-uncensored reasoning is
// hard to replace, and the benchmark gap only matters on rare chat+complexity=3
// escalations.
fn default_model_fast() -> String {
    "qwen3:8b".to_string()
}
fn default_model_primary() -> String {
    "gemma4:26b".to_string()
}
fn default_model_heavy() -> String {
    "deepseek-r1:32b".to_string()
}
fn default_model_code() -> String {
    "deepseek-coder-v2:16b".to_string()
}
fn default_model_vision() -> String {
    "gemma4:26b".to_string()
}
fn default_model_embed() -> String {
    "mxbai-embed-large".to_string()
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            fast: default_model_fast(),
            primary: default_model_primary(),
            heavy: default_model_heavy(),
            code: default_model_code(),
            vision: default_model_vision(),
            embed: default_model_embed(),
        }
    }
}

// ── [logging] ─────────────────────────────────────────────────────────────────

/// Log output format selection.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// JSON structured output — suitable for log aggregation and file piping.
    Json,
    /// Human-readable pretty output with ANSI colors.
    Pretty,
    /// Detect automatically: JSON when stdout is not a TTY, pretty when it is.
    Auto,
}

/// Tracing log level as a string, matching `tracing_subscriber::EnvFilter` syntax.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Returns the string form consumed by `EnvFilter`.
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    pub level: LogLevel,
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            format: LogFormat::Auto,
        }
    }
}

impl LoggingConfig {
    /// Returns true if the effective format is JSON (either explicit or auto-detected).
    ///
    /// Auto-detection: JSON when stdout is not a TTY (piped to file, CI, log aggregator),
    /// pretty when stdout is a TTY (interactive terminal session).
    /// Uses `std::io::IsTerminal` from stdlib (stable since Rust 1.70) — no atty crate.
    pub fn use_json(&self) -> bool {
        match self.format {
            LogFormat::Json => true,
            LogFormat::Pretty => false,
            LogFormat::Auto => !std::io::stdout().is_terminal(),
        }
    }
}

// ── [inference] ───────────────────────────────────────────────────────────────

/// Ollama connection parameters and streaming behaviour.
///
/// Two timeout fields with distinct, non-overlapping responsibilities:
/// - `request_timeout_secs`          — applied per-request on non-streaming calls
///   (embed, list, unload, pull). Not applied to `generate_stream`.
/// - `stream_inactivity_timeout_secs` — inactivity window for streaming generation.
///   Wraps each individual `.next()` call; resets on every received byte chunk.
///   Fires only when Ollama stops sending entirely (hung connection, crashed process).
///   This is the correct timeout primitive for streaming — total-request timeout is not,
///   because a deepseek-r1:32b response can legitimately stream for several minutes.
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // fields read by inference::InferenceEngine::new() — warning clears in step 8
pub struct InferenceConfig {
    /// Base URL of the Ollama HTTP API.
    /// Localhost HTTP only; TLS is never needed for a local Ollama instance.
    #[serde(default = "default_inference_ollama_base_url")]
    pub ollama_base_url: String,

    /// Timeout for non-streaming requests (embed, list, unload, pull).
    /// Applied via `RequestBuilder::timeout` at each call site. NOT used by `generate_stream`.
    #[serde(default = "default_inference_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// TCP connect timeout for all Ollama requests.
    #[serde(default = "default_inference_connect_timeout_secs")]
    pub connect_timeout_secs: u64,

    /// Maximum seconds of silence before a streaming generation is aborted.
    /// Resets on every received byte chunk — a model generating tokens every 500ms
    /// for 10 minutes will never fire this. Only fires when Ollama goes silent entirely.
    #[serde(default = "default_inference_stream_inactivity_timeout_secs")]
    pub stream_inactivity_timeout_secs: u64,

    /// If true, pull missing models automatically on first use.
    /// Default false — silently pulling 5–20GB is never the right default behaviour.
    #[serde(default)]
    pub auto_pull_missing_models: bool,
}

fn default_inference_ollama_base_url() -> String {
    OLLAMA_BASE_URL.to_string()
}
fn default_inference_request_timeout_secs() -> u64 {
    60
}
fn default_inference_connect_timeout_secs() -> u64 {
    5
}
fn default_inference_stream_inactivity_timeout_secs() -> u64 {
    30
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            ollama_base_url: default_inference_ollama_base_url(),
            request_timeout_secs: default_inference_request_timeout_secs(),
            connect_timeout_secs: default_inference_connect_timeout_secs(),
            stream_inactivity_timeout_secs: default_inference_stream_inactivity_timeout_secs(),
            auto_pull_missing_models: false,
        }
    }
}

// ── [behavior] ────────────────────────────────────────────────────────────────

/// Runtime behavioural tuning for Dexter's proactive observation system.
///
/// Phase 17: Controls whether and how often Dexter initiates unprompted ambient
/// observations when the operator's context changes (e.g., switches app).
/// Phase 18: Adds per-bundle exclusion list for proactive observations.
///
/// All fields have safe defaults — omitting `[behavior]` from `config.toml` produces
/// a sensible experience (proactive enabled, 90s interval, 30s startup grace).
///
/// Example config.toml section:
/// ```toml
/// [behavior]
/// proactive_enabled = true
/// proactive_interval_secs = 120      # seconds between proactive observations
/// proactive_startup_grace_secs = 30  # silence window after session start
/// proactive_excluded_bundles = ["com.agilebits.onepassword-osx", "com.apple.keychainaccess"]
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct BehaviorConfig {
    /// Whether Dexter initiates unprompted observations on context changes.
    ///
    /// Set to `false` to make Dexter purely reactive (only responds when asked).
    #[serde(default = "default_behavior_proactive_enabled")]
    pub proactive_enabled: bool,

    /// Minimum seconds between consecutive proactive observations.
    ///
    /// After firing, Dexter will not fire again for at least this many seconds.
    /// Prevents rapid-fire observations when the operator switches apps quickly.
    /// Default: 90 seconds (comfortable ambient presence without interruption).
    #[serde(default = "default_behavior_proactive_interval_secs")]
    pub proactive_interval_secs: u64,

    /// Silence window at session start (seconds).
    ///
    /// Dexter will not fire proactively until this many seconds after the session
    /// opens. Lets the operator settle in before Dexter starts commenting.
    /// Default: 30 seconds.
    #[serde(default = "default_behavior_proactive_startup_grace_secs")]
    pub proactive_startup_grace_secs: u64,

    /// Bundle IDs exempt from proactive observations.
    ///
    /// Phase 18 — Gate 6 in `ProactiveEngine::should_fire()`. Apps listed here
    /// never trigger a proactive ambient observation regardless of other gates.
    ///
    /// Bundle IDs are locale-invariant stable identifiers (e.g.
    /// `"com.agilebits.onepassword-osx"`) — unlike app names which can vary
    /// by locale and user rename. Use `mdls -name kMDItemCFBundleIdentifier /path/to.app`
    /// to find the bundle ID for a given app.
    ///
    /// Default: empty list (all apps eligible, Phase 17 parity).
    #[serde(default)]
    pub proactive_excluded_bundles: Vec<String>,

    /// Phase 37.9 / T8: operator's own iMessage handle for self-send requests.
    ///
    /// When the operator says "text myself", "send me …", "message myself"
    /// etc., Dexter resolves the recipient to THIS handle via a Rust-level
    /// intercept — never via LLM-generated Contacts-lookup AppleScript.
    ///
    /// Why: live-smoke T8 revealed the LLM confabulated an 855-prefix number
    /// for a "text myself" request after two syntax-erroring Contacts lookups.
    /// The terminal-workflow short-circuit then falsely confirmed "Sent." — to
    /// a stranger. Keeping the self-handle as operator config removes any LLM
    /// latitude on who "me" is.
    ///
    /// Format: E.164 phone (`"+15551234567"`) or iMessage-enabled email
    /// (`"user@example.com"`). When unset and the operator requests a self-send,
    /// the orchestrator REJECTS the action with a HUD hint — no fallback.
    ///
    /// Default: None (self-send will reject until configured).
    ///
    /// Example config.toml:
    /// ```toml
    /// [behavior]
    /// operator_self_handle = "+15551234567"
    /// ```
    #[serde(default)]
    pub operator_self_handle: Option<String>,

    /// Phase 37.9 / T8: nicknames the operator uses for themselves in
    /// third-person self-reference. Used ONLY for intent matching in the
    /// self-send intercept — never as message addressees.
    ///
    /// Example: an operator named Jason might refer to themselves as "jay" in
    /// voice dictation ("text jay my grocery list"). Without this, the
    /// intercept only fires on literal self-pronouns ("me", "myself"); with
    /// it, `"jay"` in the recipient slot also resolves to `operator_self_handle`.
    ///
    /// Matching is case-insensitive and whole-word. Empty by default.
    #[serde(default)]
    pub operator_self_aliases: Vec<String>,
}

fn default_behavior_proactive_enabled() -> bool {
    true
}
fn default_behavior_proactive_interval_secs() -> u64 {
    90
}
fn default_behavior_proactive_startup_grace_secs() -> u64 {
    30
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            proactive_enabled: default_behavior_proactive_enabled(),
            proactive_interval_secs: default_behavior_proactive_interval_secs(),
            proactive_startup_grace_secs: default_behavior_proactive_startup_grace_secs(),
            proactive_excluded_bundles: vec![],
            operator_self_handle: None,
            operator_self_aliases: vec![],
        }
    }
}

// ── [hotkey] ──────────────────────────────────────────────────────────────────

/// Global activation hotkey parameters.
///
/// Phase 18: Configurable hotkey replaces the Phase 16 hardcoded Ctrl+Shift+Space.
/// Defaults to Ctrl+Shift+Space (keyCode 49) — identical to the Phase 16
/// hardcoded value. Operators who add no `[hotkey]` section get unchanged behavior.
/// Changes take effect at next session start — pushed to Swift via `ConfigSync`.
///
/// Example config.toml section:
/// ```toml
/// [hotkey]
/// key_code = 49   # kVK_Space
/// ctrl     = true
/// shift    = false  # Ctrl+Space only
/// cmd      = false
/// option   = false
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct HotkeyConfig {
    /// macOS virtual key code. Default 49 = kVK_Space.
    /// See `<Carbon/Carbon.h>` HIToolbox/Events.h for the full table.
    #[serde(default = "default_hotkey_key_code")]
    pub key_code: u32,
    /// Require the Control modifier. Default true.
    #[serde(default = "default_hotkey_ctrl")]
    pub ctrl: bool,
    /// Require the Shift modifier. Default true.
    #[serde(default = "default_hotkey_shift")]
    pub shift: bool,
    /// Require the Command modifier. Default false.
    #[serde(default = "default_hotkey_cmd")]
    pub cmd: bool,
    /// Require the Option (Alt) modifier. Default false.
    #[serde(default = "default_hotkey_option")]
    pub option: bool,
}

fn default_hotkey_key_code() -> u32 {
    49
}
fn default_hotkey_ctrl() -> bool {
    true
}
fn default_hotkey_shift() -> bool {
    true
}
fn default_hotkey_cmd() -> bool {
    false
}
fn default_hotkey_option() -> bool {
    false
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key_code: default_hotkey_key_code(),
            ctrl: default_hotkey_ctrl(),
            shift: default_hotkey_shift(),
            cmd: default_hotkey_cmd(),
            option: default_hotkey_option(),
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Load operator configuration from `~/.dexter/config.toml`.
///
/// Behavior contract:
/// - File absent      → defaults used; logged at INFO
/// - File present, valid   → config loaded; logged at DEBUG
/// - File present, malformed → logged at ERROR with parse detail; `process::exit(1)`
/// - Any field absent in file → that field's default is used
///
/// Callers should treat the returned `DexterConfig` as the single source of truth
/// for all runtime-configurable values.
pub fn load() -> Result<DexterConfig> {
    let config_path = resolve_config_path()?;

    if !config_path.exists() {
        info!(
            path = %config_path.display(),
            "No config at path — using defaults"
        );
        return Ok(DexterConfig::default());
    }

    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file at {}", config_path.display()))?;

    match toml::from_str::<DexterConfig>(&raw) {
        Ok(cfg) => {
            debug!(
                path = %config_path.display(),
                "Config loaded from path"
            );
            Ok(cfg)
        }
        Err(e) => {
            // Log at ERROR with structured detail then exit cleanly.
            // `anyhow::bail!` here would produce an unwrap-style backtrace — not what
            // operators see when their config file has a typo. A clean error message
            // followed by exit(1) is the correct UX for a daemon configuration error.
            error!(
                path   = %config_path.display(),
                error  = %e,
                "Config file is present but contains malformed TOML — fix or remove it"
            );
            std::process::exit(1);
        }
    }
}

/// Creates `~/.dexter/state/` (or the configured path) if it does not already exist.
///
/// Called from `main` after config is loaded, before the server binds. Idempotent.
/// Lives in `config.rs` rather than a standalone module because it is config-adjacent:
/// the path comes from `CoreConfig.state_dir`, which is only known after config load.
pub fn ensure_state_dir(state_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(state_dir).with_context(|| {
        format!(
            "Failed to create state directory at {}",
            state_dir.display()
        )
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn resolve_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context(
        "Cannot resolve home directory — HOME environment variable unset and no passwd entry found",
    )?;
    Ok(home.join(".dexter").join(DEXTER_CONFIG_FILENAME))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_models_are_correct() {
        let cfg = ModelConfig::default();
        assert_eq!(cfg.fast, "qwen3:8b");
        assert_eq!(cfg.primary, "gemma4:26b");
        assert_eq!(cfg.heavy, "deepseek-r1:32b");
        assert_eq!(cfg.code, "deepseek-coder-v2:16b");
        assert_eq!(cfg.vision, "gemma4:26b");
        assert_eq!(cfg.embed, "mxbai-embed-large");
    }

    #[test]
    fn default_vision_is_aliased_to_primary() {
        // Phase 37: Gemma 4 is natively multimodal, so Vision aliases to PRIMARY
        // by default. ModelId::Vision.unload_after_use() relies on this equality
        // to avoid evicting the warm PRIMARY after a vision query.
        let cfg = ModelConfig::default();
        assert_eq!(
            cfg.vision, cfg.primary,
            "default Vision must alias to PRIMARY to keep multimodal unification"
        );
    }

    #[test]
    fn default_core_socket_path_matches_constant() {
        let cfg = CoreConfig::default();
        assert_eq!(cfg.socket_path, SOCKET_PATH);
    }

    #[test]
    fn default_logging_level_is_info() {
        let cfg = LoggingConfig::default();
        assert_eq!(cfg.level, LogLevel::Info);
        assert_eq!(cfg.level.as_str(), "info");
    }

    #[test]
    fn default_logging_format_is_auto() {
        let cfg = LoggingConfig::default();
        assert_eq!(cfg.format, LogFormat::Auto);
    }

    #[test]
    fn toml_partial_override_uses_defaults_for_absent_fields() {
        // Only `[models]` fast is overridden — all other fields must be defaults.
        let toml = r#"
            [models]
            fast = "llama3.2:1b"
        "#;
        let cfg: DexterConfig = toml::from_str(toml).expect("valid TOML");
        assert_eq!(cfg.models.fast, "llama3.2:1b");
        assert_eq!(cfg.models.primary, "gemma4:26b"); // still default
        assert_eq!(cfg.core.socket_path, SOCKET_PATH); // still default
    }

    #[test]
    fn inference_default_url_matches_constant() {
        let cfg = InferenceConfig::default();
        assert_eq!(cfg.ollama_base_url, OLLAMA_BASE_URL);
    }

    #[test]
    fn inference_default_timeouts_are_sane() {
        let cfg = InferenceConfig::default();
        assert_eq!(cfg.request_timeout_secs, 60);
        assert_eq!(cfg.connect_timeout_secs, 5);
        assert_eq!(cfg.stream_inactivity_timeout_secs, 30);
    }

    #[test]
    fn inference_auto_pull_defaults_false() {
        let cfg = InferenceConfig::default();
        assert!(!cfg.auto_pull_missing_models);
    }

    #[test]
    fn behavior_defaults_are_correct() {
        let cfg = BehaviorConfig::default();
        assert!(cfg.proactive_enabled, "proactive must default to enabled");
        assert_eq!(cfg.proactive_interval_secs, 90, "default interval is 90s");
        assert_eq!(
            cfg.proactive_startup_grace_secs, 30,
            "default startup grace is 30s"
        );
    }

    #[test]
    fn behavior_partial_override_preserves_defaults() {
        // Only interval is overridden — enabled and grace must stay at defaults.
        let toml = r#"
            [behavior]
            proactive_interval_secs = 120
        "#;
        let cfg: DexterConfig = toml::from_str(toml).expect("valid TOML");
        assert!(
            cfg.behavior.proactive_enabled,
            "enabled must default to true"
        );
        assert_eq!(
            cfg.behavior.proactive_interval_secs, 120,
            "interval was overridden"
        );
        assert_eq!(
            cfg.behavior.proactive_startup_grace_secs, 30,
            "grace must stay at default"
        );
    }

    #[test]
    fn behavior_excluded_bundles_defaults_to_empty() {
        let cfg = BehaviorConfig::default();
        assert!(
            cfg.proactive_excluded_bundles.is_empty(),
            "excluded_bundles must default to empty (Phase 17 parity — no apps blocked)"
        );
    }

    // ── HotkeyConfig ─────────────────────────────────────────────────────────

    #[test]
    fn hotkey_config_defaults_are_correct() {
        let cfg = HotkeyConfig::default();
        assert_eq!(cfg.key_code, 49, "default key_code must be 49 (kVK_Space)");
        assert!(cfg.ctrl, "default ctrl must be true");
        assert!(cfg.shift, "default shift must be true");
        assert!(!cfg.cmd, "default cmd must be false");
        assert!(!cfg.option, "default option must be false");
    }

    #[test]
    fn hotkey_config_partial_override_preserves_defaults() {
        // Only cmd is overridden — all other fields must remain at their defaults.
        let toml = "[hotkey]\ncmd = true\n";
        let cfg: DexterConfig = toml::from_str(toml).expect("valid TOML");
        assert_eq!(cfg.hotkey.key_code, 49, "key_code must stay at default");
        assert!(cfg.hotkey.ctrl, "ctrl must stay at default");
        assert!(cfg.hotkey.shift, "shift must stay at default");
        assert!(cfg.hotkey.cmd, "cmd was overridden to true");
        assert!(!cfg.hotkey.option, "option must stay at default");
    }
}
