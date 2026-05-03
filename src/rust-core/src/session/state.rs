/// Session state persistence and bootstrap for the Dexter core.
///
/// ## Purpose
///
/// `SessionStateManager` owns an in-progress session's conversation history and
/// persists it to a JSON file at `~/.dexter/state/` when the session ends. The JSON
/// files are audit/debug artifacts and can be inspected via `load_latest()`, but the
/// orchestrator deliberately does not replay prior transcripts into new live prompts.
/// Cross-session context belongs in retrieval/memory where it can be relevance-ranked
/// and framed explicitly as prior-session reference material.
///
/// ## File naming
///
/// Each session writes: `session_{YYYYMMDD_HHMMSS}_{uuid8}.json`
/// The symlink `latest.json` is updated atomically (remove + recreate) to point to the
/// most recent file. Load order: `load_latest()` reads the symlink, resolves its target,
/// and deserializes the JSON. If the symlink is absent, returns `None`.
///
/// ## Schema versioning
///
/// Every file carries `schema_version: "1.0"`. When the schema changes, increment
/// `SESSION_STATE_SCHEMA_VERSION` in `constants.rs`. `load_latest()` currently deserializes
/// without checking the version — add a version guard when a breaking migration is needed.
use std::{
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{
    config::ModelConfig,
    constants::{SESSION_FILENAME_PREFIX, SESSION_LATEST_SYMLINK, SESSION_STATE_SCHEMA_VERSION},
};

// ── Persisted schema types ────────────────────────────────────────────────────

/// A single conversation turn in the persisted session file.
///
/// Role values follow the Ollama/OpenAI convention: `"user"` | `"assistant"`.
/// System messages are not persisted — the orchestrator injects the personality
/// system prompt fresh on every session from the YAML profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub role: String,
    pub content: String,
}

/// Build phase metadata captured at session end.
///
/// Values are written from constants or config at persist time. The `current`
/// and `next_planned` fields are filled with hardcoded Phase 6 strings during
/// Phase 6 — Phase 7 will update them when it implements its own state writing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPhaseInfo {
    pub current: String,
    pub completed_components: Vec<String>,
    pub next_planned: String,
    pub notes: String,
}

/// Snapshot of active Ollama model tags captured from `ModelConfig` at persist time.
///
/// Written verbatim from the operator's config. If the operator changes model tags
/// between sessions, the next session's orchestrator reads the new tags from `ModelConfig`
/// (not from the persisted snapshot), so this field is informational only — it shows
/// what was in use when the session ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfigSnapshot {
    pub fast: String,
    pub primary: String,
    pub heavy: String,
    pub code: String,
    pub vision: String,
    pub embedding: String,
}

impl ModelConfigSnapshot {
    fn from_config(cfg: &ModelConfig) -> Self {
        Self {
            fast: cfg.fast.clone(),
            primary: cfg.primary.clone(),
            heavy: cfg.heavy.clone(),
            code: cfg.code.clone(),
            vision: cfg.vision.clone(),
            embedding: cfg.embed.clone(),
        }
    }
}

/// Environment info captured at persist time.
///
/// These are informational fields — they record what was running when the session
/// ended. Ollama version is not probed here (would require an async call); the field
/// is left as a placeholder that Phase 9 or the bootstrap script can fill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub macos_version: String,
    pub rust_version: String,
}

impl EnvironmentInfo {
    fn capture() -> Self {
        Self {
            // `std::env::consts::OS` returns "macos" — combine with the actual version
            // from `sw_vers` would require a subprocess. We capture what's cheaply
            // available at compile time for now; Phase 15 can add a runtime probe.
            macos_version: std::env::consts::OS.to_string(),
            rust_version: env!("CARGO_PKG_RUST_VERSION", "unknown").to_string(),
        }
    }
}

/// The full session state schema written to and read from session JSON files.
///
/// Matches the schema specified in `IMPLEMENTATION_PLAN.md § 2.2.13`.
/// Flat serde serialization — no nested arrays of arrays or special encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub schema_version: String,
    pub session_id: String,
    pub session_start: String,       // ISO8601 UTC
    pub session_end: Option<String>, // None until persist() is called
    pub build_phase: BuildPhaseInfo,
    pub model_config: ModelConfigSnapshot,
    pub conversation_history: Vec<HistoryEntry>,
    pub architectural_decisions: Vec<serde_json::Value>,
    pub open_questions: Vec<serde_json::Value>,
    pub environment: EnvironmentInfo,
}

// ── SessionStateManager ───────────────────────────────────────────────────────

/// Runtime manager for a single in-progress session.
///
/// Constructed by `CoreOrchestrator::new()` when a gRPC `Session()` call opens.
/// Accumulates conversation turns via `push_turn()`. Consumes itself via `persist()`
/// when the session ends — writes the JSON file and updates the `latest.json` symlink.
pub struct SessionStateManager {
    state_dir: PathBuf,
    session_id: String,
    session_start: DateTime<Utc>,
    history: Vec<HistoryEntry>,
    model_config: ModelConfigSnapshot,
}

impl SessionStateManager {
    /// Construct a new session manager for a just-opened gRPC session.
    ///
    /// Does not touch the filesystem — the state directory is assumed to exist
    /// (created at daemon startup in `main.rs` via `config::ensure_state_dir()`).
    pub fn new(state_dir: &Path, session_id: &str, model_cfg: &ModelConfig) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            session_id: session_id.to_string(),
            session_start: Utc::now(),
            history: Vec::new(),
            model_config: ModelConfigSnapshot::from_config(model_cfg),
        }
    }

    /// Load the most recent session state from `{state_dir}/latest.json`.
    ///
    /// Returns `None` if:
    /// - The symlink does not exist (first ever session, or state dir was cleared)
    /// - The symlink target cannot be read (e.g., deleted file after symlink creation)
    /// - The JSON cannot be deserialized (schema mismatch or corruption)
    ///
    /// All failure modes are logged as warnings rather than errors — a missing or
    /// unreadable session file is not fatal; the daemon starts fresh.
    pub fn load_latest(state_dir: &Path) -> Option<SessionState> {
        let symlink_path = state_dir.join(SESSION_LATEST_SYMLINK);

        if !symlink_path.exists() {
            return None;
        }

        let content = match std::fs::read_to_string(&symlink_path) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    path = %symlink_path.display(),
                    error = %e,
                    "Failed to read latest session file — starting fresh"
                );
                return None;
            }
        };

        match serde_json::from_str::<SessionState>(&content) {
            Ok(state) => Some(state),
            Err(e) => {
                warn!(
                    path = %symlink_path.display(),
                    error = %e,
                    "Failed to deserialize latest session — starting fresh"
                );
                None
            }
        }
    }

    /// Append one conversation turn.
    ///
    /// Role must be `"user"` or `"assistant"`. System messages are not stored here —
    /// the personality system prompt is re-injected fresh by `PersonalityLayer` on
    /// every session from the YAML profile.
    pub fn push_turn(&mut self, role: &str, content: &str) {
        self.history.push(HistoryEntry {
            role: role.to_string(),
            content: content.to_string(),
        });
    }

    /// Return a reference to the accumulated conversation history.
    ///
    /// In production, history is serialized into the session JSON via `persist()`.
    /// Exposed for inspection in tests and future Phase 7 context-window management.
    #[allow(dead_code)] // Phase 7 — context observer may inspect live history for windowing
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// Consume the manager, write the session JSON file, and update the symlink.
    ///
    /// File written: `{state_dir}/session_{YYYYMMDD_HHMMSS}_{uuid8}.json`
    /// Symlink updated: `{state_dir}/latest.json` → written file
    ///
    /// The symlink update is not atomic — it removes the old symlink then creates a
    /// new one. In the extremely unlikely event of a crash between these two operations,
    /// the symlink will be absent and the next startup will find no previous session
    /// (safe graceful degradation). The session JSON file is always written first.
    pub fn persist(self) -> Result<PathBuf, SessionError> {
        let session_end = Utc::now();

        // Build the filename: session_{YYYYMMDD_HHMMSS}_{first 8 chars of session_id}.json
        let uuid8 = &self.session_id[..self.session_id.len().min(8)];
        let filename = format!(
            "{}{}{}.json",
            SESSION_FILENAME_PREFIX,
            self.session_start.format("%Y%m%d_%H%M%S"),
            uuid8,
        );
        let file_path = self.state_dir.join(&filename);

        let state = SessionState {
            schema_version: SESSION_STATE_SCHEMA_VERSION.to_string(),
            session_id: self.session_id,
            session_start: self.session_start.to_rfc3339(),
            session_end: Some(session_end.to_rfc3339()),
            build_phase: BuildPhaseInfo {
                current: "Phase 6 — Rust Orchestrator + Session State".to_string(),
                completed_components: vec![
                    "Foundation".to_string(),
                    "IPC Contract".to_string(),
                    "InferenceEngine".to_string(),
                    "ModelRouter".to_string(),
                    "PersonalityLayer".to_string(),
                    "CoreOrchestrator".to_string(),
                    "SessionStateManager".to_string(),
                ],
                next_planned: "Phase 7 — Context Observer".to_string(),
                notes: String::new(),
            },
            model_config: self.model_config,
            conversation_history: self.history,
            architectural_decisions: vec![],
            open_questions: vec![],
            environment: EnvironmentInfo::capture(),
        };

        // Serialize and write the session file.
        let json = serde_json::to_string_pretty(&state).map_err(SessionError::Serialize)?;
        std::fs::write(&file_path, json).map_err(SessionError::Io)?;

        // Update the latest.json symlink: remove old → create new.
        // Removal is best-effort (may not exist on first session).
        let symlink_path = self.state_dir.join(SESSION_LATEST_SYMLINK);
        if symlink_path.exists() || symlink_path.symlink_metadata().is_ok() {
            std::fs::remove_file(&symlink_path).map_err(SessionError::Io)?;
        }
        symlink(&file_path, &symlink_path).map_err(SessionError::Io)?;

        Ok(file_path)
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur during session state persistence or loading.
#[derive(Debug)]
pub enum SessionError {
    /// An I/O error occurred while reading or writing session files.
    Io(std::io::Error),
    /// JSON serialization or deserialization failed.
    Serialize(serde_json::Error),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "Session I/O error: {e}"),
            SessionError::Serialize(e) => write!(f, "Session serialization error: {e}"),
        }
    }
}

impl std::error::Error for SessionError {}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_model_config() -> ModelConfig {
        ModelConfig::default()
    }

    fn make_manager(dir: &Path) -> SessionStateManager {
        SessionStateManager::new(
            dir,
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            &default_model_config(),
        )
    }

    #[test]
    fn persist_creates_file_and_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        let mut mgr = make_manager(dir);
        mgr.push_turn("user", "hello");
        mgr.push_turn("assistant", "hi there");

        let written_path = mgr.persist().expect("persist should succeed");

        // Session file must exist.
        assert!(written_path.exists(), "session file must be written");

        // File must be within the state directory.
        assert_eq!(written_path.parent().unwrap(), dir);

        // File name must start with the prefix.
        let file_name = written_path.file_name().unwrap().to_str().unwrap();
        assert!(
            file_name.starts_with("session_"),
            "filename must start with 'session_'"
        );

        // Symlink must exist and point to the session file.
        let symlink_path = dir.join("latest.json");
        assert!(symlink_path.exists(), "latest.json symlink must exist");

        // Read through the symlink — content must be valid SessionState JSON.
        let content = std::fs::read_to_string(&symlink_path).expect("read symlink");
        let state: SessionState = serde_json::from_str(&content).expect("deserialize");
        assert_eq!(state.schema_version, "1.0");
        assert_eq!(state.conversation_history.len(), 2);
    }

    #[test]
    fn load_latest_returns_none_when_no_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // State dir exists but latest.json does not.
        let result = SessionStateManager::load_latest(tmp.path());
        assert!(result.is_none(), "should return None when no latest.json");
    }

    #[test]
    fn load_latest_returns_previous_history() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        // Persist a session with two turns.
        let mut mgr = make_manager(dir);
        mgr.push_turn("user", "what is 2+2?");
        mgr.push_turn("assistant", "4");
        mgr.persist().expect("persist");

        // Load it back.
        let state = SessionStateManager::load_latest(dir).expect("should have previous session");
        assert_eq!(state.conversation_history.len(), 2);
        assert_eq!(state.conversation_history[0].role, "user");
        assert_eq!(state.conversation_history[0].content, "what is 2+2?");
        assert_eq!(state.conversation_history[1].role, "assistant");
        assert_eq!(state.conversation_history[1].content, "4");
    }

    #[test]
    fn push_turn_accumulates_history_in_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut mgr = make_manager(tmp.path());

        mgr.push_turn("user", "first");
        mgr.push_turn("assistant", "second");
        mgr.push_turn("user", "third");

        let history = mgr.history();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "first");
        assert_eq!(history[1].content, "second");
        assert_eq!(history[2].content, "third");
    }

    #[test]
    fn persist_writes_iso8601_timestamps() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = make_manager(tmp.path());
        mgr.persist().expect("persist");

        let symlink_path = tmp.path().join("latest.json");
        let content = std::fs::read_to_string(&symlink_path).unwrap();
        let state: SessionState = serde_json::from_str(&content).unwrap();

        // Both timestamps must be parseable as RFC3339/ISO8601.
        DateTime::parse_from_rfc3339(&state.session_start)
            .expect("session_start must be valid ISO8601");
        let end = state
            .session_end
            .expect("session_end must be set after persist");
        DateTime::parse_from_rfc3339(&end).expect("session_end must be valid ISO8601");
    }

    #[test]
    fn symlink_is_updated_on_second_persist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        // First session.
        let mut mgr1 = make_manager(dir);
        mgr1.push_turn("user", "first session");
        let path1 = mgr1.persist().expect("persist 1");

        // Second session (different session_id).
        let mut mgr2 = SessionStateManager::new(
            dir,
            "99887766-5544-3322-1100-aabbccddeeff",
            &default_model_config(),
        );
        mgr2.push_turn("user", "second session");
        let path2 = mgr2.persist().expect("persist 2");

        assert_ne!(path1, path2, "each session should produce a distinct file");

        // latest.json must now point to the second session.
        let symlink_content = std::fs::read_to_string(dir.join("latest.json")).unwrap();
        let latest: SessionState = serde_json::from_str(&symlink_content).unwrap();
        assert_eq!(latest.conversation_history[0].content, "second session");
    }

    #[test]
    fn load_latest_returns_none_when_symlink_target_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        // Create a dangling symlink by pointing to a nonexistent file.
        let symlink_path = dir.join("latest.json");
        symlink("/nonexistent/ghost_session.json", &symlink_path).unwrap();

        // Should log a warning and return None, not panic.
        let result = SessionStateManager::load_latest(dir);
        assert!(result.is_none(), "dangling symlink should produce None");
    }
}
