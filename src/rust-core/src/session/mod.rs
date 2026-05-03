/// Session state persistence for the Dexter core.
///
/// `SessionStateManager` owns an in-progress session's runtime state and persists it
/// to disk as a JSON file when the session ends gracefully. On next startup, the
/// orchestrator loads the most recent session via `SessionStateManager::load_latest()`
/// to bootstrap `ConversationContext` from prior conversation history without needing
/// to ask the operator for context.
///
/// The state directory layout is:
///   `~/.dexter/state/session_{YYYYMMDD_HHMMSS}_{uuid8}.json`  ← individual session files
///   `~/.dexter/state/latest.json`                              ← symlink to most recent
pub mod state;

// SessionError and SessionState are public API consumed by load_latest() callers and future
// phases — suppress the unused-import warning since they're re-exported, not used internally.
#[allow(unused_imports)]
pub use state::{SessionError, SessionState, SessionStateManager};
