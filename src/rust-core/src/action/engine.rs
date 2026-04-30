/// ActionEngine — submits, gates, and executes system actions.
///
/// ## Lifecycle
///
/// ```text
/// handle_text_input finds action block
///   └─ ActionEngine::submit(spec, trace_id)
///        ├─ SAFE / CAUTIOUS  → execute_and_log → ActionOutcome::Completed
///        └─ DESTRUCTIVE      → store in pending_actions
///                            → ActionOutcome::PendingApproval
///                                  │
///                                  ▼ (Swift shows confirmation dialog)
///                            handle_action_approval
///                              └─ ActionEngine::resolve(action_id, approved)
///                                   ├─ approved=true  → execute_and_log → Completed
///                                   └─ approved=false → log rejected   → Rejected
/// ```
///
/// ## Shutdown
///
/// `drain_pending_on_shutdown()` must be called before the engine is dropped.
/// Any pending actions are written to the audit log with `outcome="rejected"` and
/// `operator_approved=null` (session ended before the operator responded).
use std::{collections::HashMap, path::PathBuf, sync::Arc};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    browser::BrowserCoordinator,
    constants::{ACTION_APPLESCRIPT_TIMEOUT_SECS, ACTION_DEFAULT_TIMEOUT_SECS, ACTION_DOWNLOAD_TIMEOUT_SECS, BROWSER_WORKER_RESULT_TIMEOUT_SECS},
    ipc::proto::ActionCategory,
};

use super::{
    audit::{AuditEntry, AuditLog},
    executor,
    policy::PolicyEngine,
};

// ── BrowserActionKind ─────────────────────────────────────────────────────────

/// Browser sub-action, embedded as a flattened field inside ActionSpec::Browser.
///
/// Uses internally-tagged serde with the "action" field as discriminant.
/// When flattened into ActionSpec::Browser, produces the model's expected flat JSON:
///   {"type":"browser","action":"navigate","url":"https://...","rationale":"..."}
///
/// Without #[serde(flatten)] on the field, serde would require a nested object:
///   {"type":"browser","action":{"action":"navigate","url":"..."}}   ← wrong
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrowserActionKind {
    Navigate   { url: String },
    Click      { selector: String },
    Type       { selector: String, text: String },
    Extract    { selector: Option<String> },   // None = full page body text
    Screenshot,
}

// ── ActionSpec ────────────────────────────────────────────────────────────────

/// The action a model has requested Dexter to take.
///
/// Deserializes from the JSON embedded inside `<dexter:action>...</dexter:action>`
/// using internally-tagged serde (`"type"` field as discriminant).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionSpec {
    Shell {
        args:              Vec<String>,
        working_dir:       Option<PathBuf>,
        rationale:         Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
    FileRead {
        path: PathBuf,
    },
    FileWrite {
        path:              PathBuf,
        content:           String,
        #[serde(default)]
        create_dirs:       bool,
        #[allow(dead_code)] // Phase 9+: injected into context alongside FileWrite audit entry
        rationale:         Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
    AppleScript {
        script:    String,
        rationale: Option<String>,
    },
    Browser {
        // #[serde(flatten)] merges BrowserActionKind's tag ("action") and variant
        // fields into the parent object level so the model's flat JSON round-trips:
        //   {"type":"browser","action":"navigate","url":"..."}
        #[serde(flatten)]
        action:            BrowserActionKind,
        rationale:         Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
}

// ── ActionOutcome ─────────────────────────────────────────────────────────────

/// What the engine did (or intends to do) with an ActionSpec.
#[derive(Debug)]
pub enum ActionOutcome {
    /// Action executed (SAFE or CAUTIOUS approved). `output` is stdout/content.
    /// `rewritten_to` carries the display form of the normalized BSD command when
    /// the model generated a GNU-syntax shell command that was transparently rewritten
    /// before execution (e.g. `ps -eo %mem,cmd` → `ps -Acro pid,pmem,comm`). The
    /// orchestrator injects this into the tool result so the model reports the correct
    /// command when asked "what command did you use?" — without it, the model reads its
    /// own conversation history and reports the original (wrong) GNU form it generated.
    Completed {
        action_id:    String,
        output:       String,
        rewritten_to: Option<String>,
    },
    /// DESTRUCTIVE action stored; caller must emit `ActionRequest` to the operator.
    PendingApproval {
        action_id:   String,
        description: String,
        category:    ActionCategory,
    },
    /// Operator rejected, execution failed, or session ended. Logged; no execution.
    /// `error` carries the failure reason so the model can reason about what went wrong.
    Rejected {
        action_id: String,
        error:     String,
    },
}

// ── PendingAction ─────────────────────────────────────────────────────────────

struct PendingAction {
    spec:         ActionSpec,
    #[allow(dead_code)] // trace_id reserved for Phase 9+ context injection
    trace_id:     String,
    #[allow(dead_code)] // submitted_at reserved for Phase 9+ timeout enforcement
    submitted_at: chrono::DateTime<Utc>,
}

// ── ActionEngine ──────────────────────────────────────────────────────────────

pub struct ActionEngine {
    #[allow(dead_code)] // stored for test assertions; AuditLog owns its own path reference
    state_dir:       PathBuf,
    audit:           Arc<tokio::sync::Mutex<AuditLog>>,
    pending_actions: HashMap<String, PendingAction>,
    browser:         BrowserCoordinator,  // Phase 14 — start_browser() called by server.rs
}

/// Returns the appropriate shell timeout for a given args list.
///
/// Long-running download tools (yt-dlp, curl, wget, ffmpeg) need up to 300s.
/// All other commands use the 30s default. Detection is by binary stem so
/// absolute paths like `/opt/homebrew/bin/yt-dlp` are handled correctly.
fn shell_timeout(args: &[String]) -> u64 {
    const DOWNLOAD_TOOLS: &[&str] = &["yt-dlp", "curl", "wget", "ffmpeg", "ffprobe"];
    let bin = args.first().map(|a| {
        std::path::Path::new(a)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(a.as_str())
    }).unwrap_or("");
    if DOWNLOAD_TOOLS.iter().any(|t| *t == bin) {
        ACTION_DOWNLOAD_TIMEOUT_SECS
    } else {
        ACTION_DEFAULT_TIMEOUT_SECS
    }
}

impl ActionEngine {
    /// Create a new engine writing its audit log to `{state_dir}/audit.jsonl`.
    ///
    /// Phase 38c: takes a `BrowserCoordinator` parameter so the daemon-lifetime
    /// instance owned by `CoreService` can be shared across all sessions
    /// instead of each session spawning its own chromium subprocess (which
    /// caused PRIMARY page reclamation on every reconnect — see Phase 38c
    /// motivation in MEMORY.md). Tests can pass `BrowserCoordinator::new_degraded()`
    /// for an isolated coordinator.
    ///
    /// The audit file is not created until the first action is taken.
    pub fn new(state_dir: &std::path::Path, browser: BrowserCoordinator) -> Self {
        Self {
            state_dir:       state_dir.to_path_buf(),
            audit:           AuditLog::new_shared(state_dir),
            pending_actions: HashMap::new(),
            browser,
        }
    }

    /// Create an `ExecutorHandle` that can be sent to a spawned task for
    /// background action execution (Phase 24).
    ///
    /// The handle shares the audit log and browser coordinator via `Arc`.
    /// Multiple handles can exist concurrently — serialization is handled
    /// by the `Mutex` on the audit log and the browser coordinator's
    /// internal `Arc<Mutex<WorkerClient>>`.
    pub fn executor_handle(&self) -> ExecutorHandle {
        ExecutorHandle {
            audit:   self.audit.clone(),
            browser: self.browser.clone(),
        }
    }

    // Phase 38c: `start_browser` removed — the browser worker is now spawned
    // at daemon startup via `SharedDaemonState::run_startup_warmup`. The
    // shared `BrowserCoordinator` is passed into `ActionEngine::new` and is
    // already started by the time any session's ActionEngine sees it.

    /// Health-check + conditional restart. Called from CoreOrchestrator health-check timer.
    pub async fn browser_health_check(&self) {
        self.browser.health_check_and_restart().await;
    }

    /// True when the browser worker has permanently exceeded restart limits.
    /// Delegated from CoreOrchestrator — `browser` is private to this module.
    pub fn is_browser_permanently_degraded(&self) -> bool {
        self.browser.is_permanently_degraded()
    }

    /// Shutdown browser worker. Phase 38c: no longer called from
    /// `CoreOrchestrator::shutdown` because the browser worker is now shared
    /// across all sessions; per-session shutdown would kill workers used
    /// elsewhere. Retained for the future graceful-shutdown path where
    /// `CoreService` (or main.rs SIGINT handler) calls it explicitly to
    /// send a clean SHUTDOWN frame before the daemon exits.
    #[allow(dead_code)] // Phase 38c: preserved for future graceful-shutdown wiring
    pub async fn shutdown_browser(&mut self) {
        self.browser.shutdown().await;
    }

    /// Submit an ActionSpec for execution.
    ///
    /// - SAFE / CAUTIOUS → execute immediately → return `Completed`
    /// - DESTRUCTIVE → store in `pending_actions` → return `PendingApproval`
    ///   (caller must emit `ActionRequest` ServerEvent to Swift; execution follows
    ///   only when `resolve()` is called with `approved=true`)
    pub async fn submit(
        &mut self,
        spec:     ActionSpec,
        trace_id: &str,
    ) -> ActionOutcome {
        let action_id = Uuid::new_v4().to_string();
        let category  = PolicyEngine::classify(&spec);

        if category == ActionCategory::Destructive {
            let description = Self::describe(&spec);
            info!(
                action_id = %action_id,
                category  = "destructive",
                %description,
                "Action requires operator approval — pending"
            );
            self.pending_actions.insert(
                action_id.clone(),
                PendingAction {
                    spec,
                    trace_id:     trace_id.to_string(),
                    submitted_at: Utc::now(),
                },
            );
            return ActionOutcome::PendingApproval { action_id, description, category };
        }

        // SAFE or CAUTIOUS — execute immediately.
        self.execute_and_log(&action_id, &spec, category, None).await
    }

    /// Resolve a pending DESTRUCTIVE action after the operator responds.
    ///
    /// Returns `Rejected` (with a `warn!`) if `action_id` is not found in
    /// `pending_actions`. This is harmless — it covers stale approvals from a
    /// prior session that survived a reconnect, or duplicate approval messages.
    pub async fn resolve(
        &mut self,
        action_id:     &str,
        approved:      bool,
        operator_note: &str,
    ) -> ActionOutcome {
        let pending = match self.pending_actions.remove(action_id) {
            Some(p) => p,
            None    => {
                warn!(
                    action_id = %action_id,
                    "ActionApproval for unknown action_id — stale or duplicate; ignoring"
                );
                return ActionOutcome::Rejected {
                    action_id: action_id.to_string(),
                    error:     "stale or duplicate approval — action_id not found".to_string(),
                };
            }
        };

        if !approved {
            info!(
                action_id = %action_id,
                note      = %operator_note,
                "Action rejected by operator"
            );
            let category = PolicyEngine::classify(&pending.spec);
            let entry = AuditEntry {
                timestamp:         Utc::now().to_rfc3339(),
                action_id,
                r#type:            Self::type_str(&pending.spec),
                category:          Self::category_str(category),
                spec_json:         Self::spec_to_audit_json(&pending.spec),
                outcome:           "rejected",
                exit_code:         None,
                output_preview:    None,
                error:             None,
                duration_ms:       None,
                operator_approved: Some(false),
            };
            let guard = self.audit.lock().await;
            if let Err(e) = guard.append(&entry) {
                error!(action_id = %action_id, error = %e, "Audit log append failed");
            }
            return ActionOutcome::Rejected {
                action_id: action_id.to_string(),
                error:     "operator rejected the action".to_string(),
            };
        }

        info!(action_id = %action_id, "Operator approved DESTRUCTIVE action — executing");
        let category = PolicyEngine::classify(&pending.spec);
        self.execute_and_log(action_id, &pending.spec, category, Some(true)).await
    }

    /// Write audit entries for any remaining pending actions and clear the map.
    ///
    /// Called from `CoreOrchestrator::shutdown()` (which takes `mut self`).
    /// Each surviving pending action is logged with `outcome="rejected"` and
    /// `operator_approved=null` — session ended before the operator responded.
    pub async fn drain_pending_on_shutdown(&mut self) {
        let guard = self.audit.lock().await;
        for (action_id, pending) in self.pending_actions.drain() {
            let category = PolicyEngine::classify(&pending.spec);
            let entry = AuditEntry {
                timestamp:         Utc::now().to_rfc3339(),
                action_id:         &action_id,
                r#type:            Self::type_str(&pending.spec),
                category:          Self::category_str(category),
                spec_json:         Self::spec_to_audit_json(&pending.spec),
                outcome:           "rejected",
                exit_code:         None,
                output_preview:    None,
                error:             Some("session ended before operator responded".to_string()),
                duration_ms:       None,
                operator_approved: None, // null = session ended, not an explicit rejection
            };
            if let Err(e) = guard.append(&entry) {
                error!(
                    action_id = %action_id,
                    error     = %e,
                    "Audit log append failed during shutdown drain"
                );
            }
        }
    }

    /// Round 3 / T0.4: approve every pending action in response to a typed
    /// affirmative ("yes", "ok", "do it", ...) during ALERT.
    ///
    /// In practice `pending_actions` holds at most one entry — PolicyEngine
    /// serialises DESTRUCTIVE actions behind approval, so a new one cannot be
    /// spawned while an older one is still pending. The `_all` suffix documents
    /// the invariant ("drain whatever is there") rather than implying a batch
    /// queue.
    ///
    /// Returns the list of `ActionOutcome`s so the orchestrator can inspect
    /// individual outcomes if it ever needs to (currently it just relies on
    /// the shared audit log for post-hoc inspection).
    pub async fn approve_all_pending(&mut self) -> Vec<ActionOutcome> {
        let ids: Vec<String> = self.pending_actions.keys().cloned().collect();
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            outcomes.push(self.resolve(&id, true, "typed-approval").await);
        }
        outcomes
    }

    /// Round 3 / T0.4: reject every pending action in response to a typed
    /// negative ("no", "cancel", ...) during ALERT. Symmetric with
    /// `approve_all_pending`; see that function's doc for the single-element
    /// invariant.
    pub async fn reject_all_pending(&mut self) -> Vec<ActionOutcome> {
        let ids: Vec<String> = self.pending_actions.keys().cloned().collect();
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            outcomes.push(self.resolve(&id, false, "typed-denial").await);
        }
        outcomes
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Execute `spec` and write an audit log entry. Returns `Completed` on success,
    /// `Rejected` on execution failure (the audit entry is written regardless).
    ///
    /// `operator_approved`: `None` for SAFE/CAUTIOUS (no gate); `Some(true)` for
    /// approved DESTRUCTIVE actions.
    async fn execute_and_log(
        &self,
        action_id:         &str,
        spec:              &ActionSpec,
        category:          ActionCategory,
        operator_approved: Option<bool>,
    ) -> ActionOutcome {
        let result  = match spec {
            ActionSpec::Shell { args, working_dir, .. } => {
                executor::execute_shell(args, working_dir.as_ref(), shell_timeout(args)).await
            }
            ActionSpec::FileRead { path } => {
                executor::execute_file_read(path).await
            }
            ActionSpec::FileWrite { path, content, create_dirs, .. } => {
                executor::execute_file_write(path, content, *create_dirs).await
            }
            ActionSpec::AppleScript { script, .. } => {
                executor::execute_applescript(script, ACTION_APPLESCRIPT_TIMEOUT_SECS).await
            }
            ActionSpec::Browser { action, .. } => {
                executor::execute_browser(&self.browser, action, BROWSER_WORKER_RESULT_TIMEOUT_SECS).await
            }
        };

        let outcome_str: &'static str = if result.success {
            "success"
        } else if result.error.contains("timed out") {
            "timeout"
        } else {
            "failure"
        };

        let output_preview = if result.output.is_empty() {
            None
        } else {
            Some(AuditLog::preview(&result.output))
        };

        let entry = AuditEntry {
            timestamp:         Utc::now().to_rfc3339(),
            action_id,
            r#type:            Self::type_str(spec),
            category:          Self::category_str(category),
            spec_json:         Self::spec_to_audit_json(spec),
            outcome:           outcome_str,
            exit_code:         result.exit_code,
            output_preview,
            error:             if result.error.is_empty() { None } else { Some(result.error.clone()) },
            duration_ms:       Some(result.duration_ms),
            operator_approved,
        };

        {
            let guard = self.audit.lock().await;
            if let Err(e) = guard.append(&entry) {
                error!(action_id = %action_id, error = %e, "Audit log append failed");
            }
        }

        if result.success {
            // This is the approval-path execution (handle_approval calls execute directly
            // with the stored ActionSpec). No normalization annotation needed here because
            // the approval dialog already showed the operator the action spec.
            ActionOutcome::Completed {
                action_id:    action_id.to_string(),
                output:       result.output,
                rewritten_to: None,
            }
        } else {
            // Execution failure is logged. Return Rejected with the actual error
            // so the orchestrator can surface it to the model and operator.
            let error = if !result.error.is_empty() && !result.output.is_empty() {
                format!("{}\n{}", result.error, result.output)
            } else if !result.error.is_empty() {
                result.error
            } else if !result.output.is_empty() {
                result.output
            } else {
                format!("command failed (exit code: {})", result.exit_code.unwrap_or(-1))
            };
            ActionOutcome::Rejected { action_id: action_id.to_string(), error }
        }
    }

    /// Human-readable action description for `ActionRequest.description` in the UI.
    fn describe(spec: &ActionSpec) -> String {
        match spec {
            ActionSpec::Shell { args, .. } => {
                format!("Run: {}", args.join(" "))
            }
            ActionSpec::FileRead { path } => {
                format!("Read: {}", path.display())
            }
            ActionSpec::FileWrite { path, .. } => {
                format!("Write: {}", path.display())
            }
            ActionSpec::AppleScript { script, .. } => {
                let preview: String = script.chars().take(80).collect();
                format!("AppleScript: {}", preview)
            }
            ActionSpec::Browser { action, .. } => match action {
                BrowserActionKind::Navigate { url }      => format!("Browser navigate: {url}"),
                BrowserActionKind::Click { selector }    => format!("Browser click: {selector}"),
                BrowserActionKind::Type { selector, .. } => format!("Browser type into: {selector}"),
                BrowserActionKind::Extract { selector }  => format!(
                    "Browser extract: {}",
                    selector.as_deref().unwrap_or("<page>")
                ),
                BrowserActionKind::Screenshot            => "Browser screenshot".to_string(),
            },
        }
    }

    /// Phase 24: pub(crate) so ExecutorHandle can use it for audit entries.
    pub(crate) fn type_str(spec: &ActionSpec) -> &'static str {
        match spec {
            ActionSpec::Shell { .. }       => "shell",
            ActionSpec::FileRead { .. }    => "file_read",
            ActionSpec::FileWrite { .. }   => "file_write",
            ActionSpec::AppleScript { .. } => "applescript",
            ActionSpec::Browser { .. }     => "browser",
        }
    }

    pub(crate) fn category_str(cat: ActionCategory) -> &'static str {
        match cat {
            ActionCategory::Safe        => "safe",
            ActionCategory::Cautious    => "cautious",
            ActionCategory::Destructive => "destructive",
            _                           => "unspecified",
        }
    }

    /// Produce an audit-safe JSON representation of the ActionSpec.
    ///
    /// `FileWrite.content` and `Browser::Type.text` are redacted — file contents
    /// can be megabytes, and typed text may be passwords or credentials. The audit
    /// log records intent, not data.
    pub(crate) fn spec_to_audit_json(spec: &ActionSpec) -> serde_json::Value {
        match spec {
            ActionSpec::FileWrite { path, content, create_dirs, .. } => {
                serde_json::json!({
                    "path":        path,
                    "content":     format!("<{} bytes omitted>", content.len()),
                    "create_dirs": create_dirs,
                })
            }
            ActionSpec::Shell { args, working_dir, rationale, .. } => {
                serde_json::json!({
                    "args":        args,
                    "working_dir": working_dir,
                    "rationale":   rationale,
                })
            }
            ActionSpec::FileRead { path } => {
                serde_json::json!({ "path": path })
            }
            ActionSpec::AppleScript { rationale, .. } => {
                // Do not log the script body — it may contain keystrokes or credentials.
                serde_json::json!({ "rationale": rationale })
            }
            ActionSpec::Browser { action, rationale, .. } => {
                let action_detail = match action {
                    BrowserActionKind::Navigate { url }     => serde_json::json!({"action":"navigate","url":url}),
                    BrowserActionKind::Click { selector }   => serde_json::json!({"action":"click","selector":selector}),
                    BrowserActionKind::Type { selector, .. } => {
                        // text is omitted from audit — may be a password or credential.
                        serde_json::json!({"action":"type","selector":selector,"text":"<omitted>"})
                    }
                    BrowserActionKind::Extract { selector } => serde_json::json!({"action":"extract","selector":selector}),
                    BrowserActionKind::Screenshot           => serde_json::json!({"action":"screenshot"}),
                };
                serde_json::json!({ "browser": action_detail, "rationale": rationale })
            }
        }
    }

    /// Return the path to the audit log (used for startup logging).
    #[allow(dead_code)]
    pub async fn audit_log_path(&self) -> PathBuf {
        self.audit.lock().await.path().to_path_buf()
    }
}

// ── ActionResult ─────────────────────────────────────────────────────────────

/// Delivered via `action_rx` when a background action task completes (Phase 24).
#[derive(Debug)]
pub struct ActionResult {
    pub action_id: String,
    pub outcome:   ActionOutcome,
    pub trace_id:  String,
}

// ── ExecutorHandle ───────────────────────────────────────────────────────────

/// Clone-able execution context for running actions in spawned background tasks.
///
/// Phase 24: decouples action execution from the orchestrator event loop.
/// Each spawned action task gets its own `ExecutorHandle` clone. Serialization
/// is handled by the `Mutex` on the audit log and the browser coordinator's
/// internal `Arc<Mutex<WorkerClient>>`.
#[derive(Clone)]
pub struct ExecutorHandle {
    audit:   Arc<tokio::sync::Mutex<AuditLog>>,
    browser: BrowserCoordinator,
}

impl ExecutorHandle {
    /// Execute an action spec and return the outcome.
    ///
    /// Called from a spawned background task. Writes an audit entry on
    /// completion regardless of success or failure.
    pub async fn execute(
        &self,
        action_id: &str,
        spec:      &ActionSpec,
        category:  ActionCategory,
        operator_approved: Option<bool>,
    ) -> ActionOutcome {
        // For Shell actions: compute the display form of the normalized BSD command
        // BEFORE execution. If the model generated GNU-style ps flags, the executor
        // transparently rewrites them to a BSD pipeline. Without this annotation the
        // model reads its own conversation history (which has the original GNU form)
        // and incorrectly reports that command when asked "what did you run?".
        let rewritten_to: Option<String> = if let ActionSpec::Shell { args, .. } = spec {
            executor::describe_normalized_shell_command(args)
        } else {
            None
        };

        let result  = match spec {
            ActionSpec::Shell { args, working_dir, .. } => {
                executor::execute_shell(args, working_dir.as_ref(), shell_timeout(args)).await
            }
            ActionSpec::FileRead { path } => {
                executor::execute_file_read(path).await
            }
            ActionSpec::FileWrite { path, content, create_dirs, .. } => {
                executor::execute_file_write(path, content, *create_dirs).await
            }
            ActionSpec::AppleScript { script, .. } => {
                executor::execute_applescript(script, ACTION_APPLESCRIPT_TIMEOUT_SECS).await
            }
            ActionSpec::Browser { action, .. } => {
                executor::execute_browser(&self.browser, action, BROWSER_WORKER_RESULT_TIMEOUT_SECS).await
            }
        };

        let outcome_str: &'static str = if result.success {
            "success"
        } else if result.error.contains("timed out") {
            "timeout"
        } else {
            "failure"
        };

        let output_preview = if result.output.is_empty() {
            None
        } else {
            Some(AuditLog::preview(&result.output))
        };

        let entry = AuditEntry {
            timestamp:         Utc::now().to_rfc3339(),
            action_id,
            r#type:            ActionEngine::type_str(spec),
            category:          ActionEngine::category_str(category),
            spec_json:         ActionEngine::spec_to_audit_json(spec),
            outcome:           outcome_str,
            exit_code:         result.exit_code,
            output_preview,
            error:             if result.error.is_empty() { None } else { Some(result.error.clone()) },
            duration_ms:       Some(result.duration_ms),
            operator_approved,
        };

        {
            let guard = self.audit.lock().await;
            if let Err(e) = guard.append(&entry) {
                error!(action_id = %action_id, error = %e, "Audit log append failed (background)");
            }
        }

        if result.success {
            ActionOutcome::Completed {
                action_id:    action_id.to_string(),
                output:       result.output,
                rewritten_to,   // Some("ps -Acro pid,pmem,comm") when GNU ps was normalized; None otherwise
            }
        } else {
            // Combine stderr error and any stdout output so the model sees
            // what actually went wrong (e.g. "yt-dlp: ERROR: 404 Not Found").
            let error = if !result.error.is_empty() && !result.output.is_empty() {
                format!("{}\n{}", result.error, result.output)
            } else if !result.error.is_empty() {
                result.error
            } else if !result.output.is_empty() {
                result.output
            } else {
                format!("command failed (exit code: {})", result.exit_code.unwrap_or(-1))
            };
            ActionOutcome::Rejected { action_id: action_id.to_string(), error }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_safe_shell() -> ActionSpec {
        ActionSpec::Shell {
            args:              vec!["echo".to_string(), "engine-test".to_string()],
            working_dir:       None,
            rationale:         None,
            category_override: None,
        }
    }

    fn make_destructive_shell() -> ActionSpec {
        ActionSpec::Shell {
            args:              vec!["rm".to_string(), "-rf".to_string(), "/tmp/dexter-engine-test".to_string()],
            working_dir:       None,
            rationale:         None,
            category_override: None,
        }
    }

    fn make_cautious_file_write(tmp: &std::path::Path) -> ActionSpec {
        ActionSpec::FileWrite {
            path:              tmp.join("engine-out.txt"),
            content:           "engine test content".to_string(),
            create_dirs:       false,
            rationale:         None,
            category_override: None,
        }
    }

    #[test]
    fn engine_new_creates_correctly() {
        let tmp = tempdir().unwrap();
        let engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());
        assert_eq!(engine.state_dir, tmp.path());
        assert!(engine.pending_actions.is_empty());
    }

    #[tokio::test]
    async fn submit_safe_shell_executes_immediately() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());
        let outcome = engine.submit(make_safe_shell(), "trace-001").await;
        match outcome {
            ActionOutcome::Completed { output, .. } => {
                assert!(output.contains("engine-test"), "output: {output}");
            }
            other => panic!("expected Completed, got: {other:?}"),
        }
        // Audit log must exist after execution.
        assert!(engine.audit.lock().await.path().exists(), "audit log must be created after first action");
    }

    #[tokio::test]
    async fn submit_cautious_file_write_executes_immediately() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());
        let spec = make_cautious_file_write(tmp.path());

        let outcome = engine.submit(spec, "trace-002").await;
        assert!(matches!(outcome, ActionOutcome::Completed { .. }), "got: {outcome:?}");
    }

    #[tokio::test]
    async fn submit_destructive_stores_pending() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());

        let outcome = engine.submit(make_destructive_shell(), "trace-003").await;
        match outcome {
            ActionOutcome::PendingApproval { .. } => {
                assert_eq!(engine.pending_actions.len(), 1, "must be stored in pending_actions");
            }
            other => panic!("expected PendingApproval, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_approved_removes_from_pending_and_executes() {
        // The destructive spec used here (`rm -rf /tmp/dexter-engine-test-resolved`)
        // references a path that doesn't exist, so rm exits non-zero. The engine
        // returns Rejected (execution failure) rather than Completed — but the key
        // invariant tested is that the pending_actions map is drained correctly.
        // Use a SAFE spec with a forced DESTRUCTIVE override to get PendingApproval
        // but then execute safely.
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());

        // Use echo with destructive override so it pends but executes safely when approved.
        let spec = ActionSpec::Shell {
            args:              vec!["echo".to_string(), "approved-test".to_string()],
            working_dir:       None,
            rationale:         None,
            category_override: Some("destructive".to_string()),
        };

        let outcome = engine.submit(spec, "trace-004").await;
        let action_id = match outcome {
            ActionOutcome::PendingApproval { action_id, .. } => action_id,
            other => panic!("expected PendingApproval, got: {other:?}"),
        };
        assert_eq!(engine.pending_actions.len(), 1);

        let resolved = engine.resolve(&action_id, true, "approved by test").await;
        assert!(
            matches!(resolved, ActionOutcome::Completed { .. }),
            "approved echo should complete: {resolved:?}"
        );
        assert_eq!(engine.pending_actions.len(), 0, "must be removed from pending_actions");
    }

    #[tokio::test]
    async fn resolve_rejected_removes_from_pending() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());

        let outcome = engine.submit(make_destructive_shell(), "trace-005").await;
        let action_id = match outcome {
            ActionOutcome::PendingApproval { action_id, .. } => action_id,
            other => panic!("expected PendingApproval, got: {other:?}"),
        };

        let resolved = engine.resolve(&action_id, false, "rejected by test").await;
        assert!(matches!(resolved, ActionOutcome::Rejected { .. }), "got: {resolved:?}");
        assert_eq!(engine.pending_actions.len(), 0);
    }

    #[tokio::test]
    async fn resolve_unknown_action_id_returns_rejected() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());

        let outcome = engine.resolve("completely-unknown-uuid", true, "").await;
        // Must return Rejected without panicking.
        assert!(matches!(outcome, ActionOutcome::Rejected { .. }));
        // No crash, no state corruption.
        assert_eq!(engine.pending_actions.len(), 0);
    }
}
