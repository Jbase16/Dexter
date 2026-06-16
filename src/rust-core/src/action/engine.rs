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
    ambient::{AmbientEventStore, AmbientSeverity},
    browser::BrowserCoordinator,
    constants::{
        ACTION_APPLESCRIPT_TIMEOUT_SECS, ACTION_APPROVAL_TIMEOUT_SECS, ACTION_DEFAULT_TIMEOUT_SECS,
        ACTION_DOWNLOAD_TIMEOUT_SECS, BROWSER_WORKER_RESULT_TIMEOUT_SECS,
    },
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
    Navigate { url: String },
    Click { selector: String },
    Type { selector: String, text: String },
    Extract { selector: Option<String> }, // None = full page body text
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
        args: Vec<String>,
        working_dir: Option<PathBuf>,
        rationale: Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
    FileRead {
        path: PathBuf,
    },
    FileWrite {
        path: PathBuf,
        content: String,
        #[serde(default)]
        create_dirs: bool,
        #[allow(dead_code)] // Phase 9+: injected into context alongside FileWrite audit entry
        rationale: Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
    AppleScript {
        script: String,
        rationale: Option<String>,
    },
    MessageSend {
        #[serde(alias = "recipient_name")]
        recipient: String,
        #[serde(alias = "message", alias = "text")]
        body: String,
        rationale: Option<String>,
    },
    WindowFocus {
        #[serde(alias = "app")]
        app_name: String,
        #[serde(default)]
        title_contains: Option<String>,
        rationale: Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
    WindowInspect {
        #[serde(default, alias = "app")]
        app_name: Option<String>,
        rationale: Option<String>,
    },
    UiSnapshot {
        #[serde(default, alias = "app")]
        app_name: Option<String>,
        #[serde(default)]
        max_depth: Option<u8>,
        rationale: Option<String>,
    },
    UiClick {
        #[serde(default, alias = "app")]
        app_name: Option<String>,
        #[serde(default)]
        role: Option<String>,
        label: String,
        #[serde(default)]
        max_depth: Option<u8>,
        rationale: Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
    UiType {
        #[serde(default, alias = "app")]
        app_name: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        label: Option<String>,
        text: String,
        #[serde(default)]
        max_depth: Option<u8>,
        rationale: Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
    Browser {
        // #[serde(flatten)] merges BrowserActionKind's tag ("action") and variant
        // fields into the parent object level so the model's flat JSON round-trips:
        //   {"type":"browser","action":"navigate","url":"..."}
        #[serde(flatten)]
        action: BrowserActionKind,
        rationale: Option<String>,
        #[serde(default)]
        category_override: Option<String>,
    },
    Shortcut {
        name: String,
        #[serde(default)]
        input_path: Option<PathBuf>,
        #[serde(default)]
        output_path: Option<PathBuf>,
        rationale: Option<String>,
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
        action_id: String,
        output: String,
        rewritten_to: Option<String>,
    },
    /// DESTRUCTIVE action stored; caller must emit `ActionRequest` to the operator.
    PendingApproval {
        action_id: String,
        description: String,
        category: ActionCategory,
        expires_at_unix_ms: u64,
        timeout_secs: u32,
    },
    /// Operator rejected, execution failed, or session ended. Logged; no execution.
    /// `error` carries the failure reason so the model can reason about what went wrong.
    Rejected { action_id: String, error: String },
}

// ── PendingAction ─────────────────────────────────────────────────────────────

struct PendingAction {
    spec: ActionSpec,
    #[allow(dead_code)] // trace_id reserved for Phase 9+ context injection
    trace_id: String,
    submitted_at: chrono::DateTime<Utc>,
    expires_at: chrono::DateTime<Utc>,
}

/// Operator-visible metadata for a pending action.
///
/// Kept separate from `ActionOutcome` so receipts can be emitted without
/// exposing `PendingAction` or re-parsing model output in the orchestrator.
#[derive(Debug, Clone)]
pub struct ActionReceiptMetadata {
    pub description: String,
    pub action_type: &'static str,
    pub category: &'static str,
}

// ── ActionEngine ──────────────────────────────────────────────────────────────

pub struct ActionEngine {
    #[allow(dead_code)] // stored for test assertions; AuditLog owns its own path reference
    state_dir: PathBuf,
    audit: Arc<tokio::sync::Mutex<AuditLog>>,
    ambient: AmbientEventStore,
    pending_actions: HashMap<String, PendingAction>,
    browser: BrowserCoordinator, // Phase 14 — start_browser() called by server.rs
}

/// Returns the appropriate shell timeout for a given args list.
///
/// Long-running download tools (yt-dlp, curl, wget, ffmpeg) need up to 300s.
/// All other commands use the 30s default. Detection is by binary stem so
/// absolute paths like `/opt/homebrew/bin/yt-dlp` are handled correctly.
fn shell_timeout(args: &[String]) -> u64 {
    const DOWNLOAD_TOOLS: &[&str] = &["yt-dlp", "curl", "wget", "ffmpeg", "ffprobe"];
    let bin = args
        .first()
        .map(|a| {
            std::path::Path::new(a)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(a.as_str())
        })
        .unwrap_or("");
    if DOWNLOAD_TOOLS.iter().any(|t| *t == bin) {
        ACTION_DOWNLOAD_TIMEOUT_SECS
    } else {
        ACTION_DEFAULT_TIMEOUT_SECS
    }
}

fn approval_timeout_secs() -> u64 {
    let Ok(raw) = std::env::var("DEXTER_ACTION_APPROVAL_TIMEOUT_SECS") else {
        return ACTION_APPROVAL_TIMEOUT_SECS;
    };
    let Ok(value) = raw.trim().parse::<u64>() else {
        return ACTION_APPROVAL_TIMEOUT_SECS;
    };
    if value == 0 {
        ACTION_APPROVAL_TIMEOUT_SECS
    } else {
        value.min(ACTION_APPROVAL_TIMEOUT_SECS)
    }
}

fn applescript_timeout_secs() -> u64 {
    let Ok(raw) = std::env::var("DEXTER_ACTION_APPLESCRIPT_TIMEOUT_SECS") else {
        return ACTION_APPLESCRIPT_TIMEOUT_SECS;
    };
    let Ok(value) = raw.trim().parse::<u64>() else {
        return ACTION_APPLESCRIPT_TIMEOUT_SECS;
    };
    if value == 0 {
        ACTION_APPLESCRIPT_TIMEOUT_SECS
    } else {
        value.min(ACTION_APPLESCRIPT_TIMEOUT_SECS)
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
            state_dir: state_dir.to_path_buf(),
            audit: AuditLog::new_shared(state_dir),
            ambient: AmbientEventStore::new(state_dir),
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
            audit: self.audit.clone(),
            ambient: self.ambient.clone(),
            browser: self.browser.clone(),
        }
    }

    /// Metadata needed to render a live action receipt after approval/denial.
    pub fn pending_receipt_metadata(&self, action_id: &str) -> Option<ActionReceiptMetadata> {
        self.pending_actions.get(action_id).map(|pending| {
            let category = PolicyEngine::classify(&pending.spec);
            ActionReceiptMetadata {
                description: Self::describe(&pending.spec),
                action_type: Self::type_str(&pending.spec),
                category: Self::category_str(category),
            }
        })
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
    pub async fn submit(&mut self, spec: ActionSpec, trace_id: &str) -> ActionOutcome {
        let action_id = Uuid::new_v4().to_string();
        let category = PolicyEngine::classify(&spec);

        if category == ActionCategory::Destructive {
            let description = Self::describe(&spec);
            let submitted_at = Utc::now();
            let timeout_secs = approval_timeout_secs();
            let expires_at = submitted_at + chrono::Duration::seconds(timeout_secs as i64);
            let expires_at_unix_ms = expires_at.timestamp_millis().max(0) as u64;
            info!(
                action_id = %action_id,
                category  = "destructive",
                %description,
                expires_at = %expires_at.to_rfc3339(),
                "Action requires operator approval — pending"
            );
            self.record_approval_requested_event(
                &action_id,
                &spec,
                category,
                &description,
                expires_at_unix_ms,
                timeout_secs as u32,
            );
            self.pending_actions.insert(
                action_id.clone(),
                PendingAction {
                    spec,
                    trace_id: trace_id.to_string(),
                    submitted_at,
                    expires_at,
                },
            );
            return ActionOutcome::PendingApproval {
                action_id,
                description,
                category,
                expires_at_unix_ms,
                timeout_secs: timeout_secs as u32,
            };
        }

        // SAFE or CAUTIOUS — execute immediately.
        self.execute_and_log(&action_id, &spec, category, None)
            .await
    }

    /// Resolve a pending DESTRUCTIVE action after the operator responds.
    ///
    /// Returns `Rejected` (with a `warn!`) if `action_id` is not found in
    /// `pending_actions`. This is harmless — it covers stale approvals from a
    /// prior session that survived a reconnect, or duplicate approval messages.
    pub async fn resolve(
        &mut self,
        action_id: &str,
        approved: bool,
        operator_note: &str,
    ) -> ActionOutcome {
        let pending = match self.pending_actions.remove(action_id) {
            Some(p) => p,
            None => {
                warn!(
                    action_id = %action_id,
                    "ActionApproval for unknown action_id — stale or duplicate; ignoring"
                );
                return ActionOutcome::Rejected {
                    action_id: action_id.to_string(),
                    error: "stale or duplicate approval — action_id not found".to_string(),
                };
            }
        };

        let now = Utc::now();
        if now > pending.expires_at {
            warn!(
                action_id = %action_id,
                submitted_at = %pending.submitted_at.to_rfc3339(),
                expires_at = %pending.expires_at.to_rfc3339(),
                note = %operator_note,
                "ActionApproval arrived after approval deadline — refusing execution"
            );
            let category = PolicyEngine::classify(&pending.spec);
            let entry = AuditEntry {
                timestamp: now.to_rfc3339(),
                action_id,
                r#type: Self::type_str(&pending.spec),
                category: Self::category_str(category),
                spec_json: Self::spec_to_audit_json(&pending.spec),
                outcome: "rejected",
                exit_code: None,
                output_preview: None,
                error: Some("approval expired before operator response".to_string()),
                duration_ms: None,
                operator_approved: Some(false),
            };
            let guard = self.audit.lock().await;
            if let Err(e) = guard.append(&entry) {
                error!(action_id = %action_id, error = %e, "Audit log append failed");
            }
            self.record_action_audit_event(&entry, &pending.spec);
            return ActionOutcome::Rejected {
                action_id: action_id.to_string(),
                error: "approval expired before operator response".to_string(),
            };
        }

        if !approved {
            info!(
                action_id = %action_id,
                note      = %operator_note,
                "Action rejected by operator"
            );
            let category = PolicyEngine::classify(&pending.spec);
            let entry = AuditEntry {
                timestamp: Utc::now().to_rfc3339(),
                action_id,
                r#type: Self::type_str(&pending.spec),
                category: Self::category_str(category),
                spec_json: Self::spec_to_audit_json(&pending.spec),
                outcome: "rejected",
                exit_code: None,
                output_preview: None,
                error: None,
                duration_ms: None,
                operator_approved: Some(false),
            };
            let guard = self.audit.lock().await;
            if let Err(e) = guard.append(&entry) {
                error!(action_id = %action_id, error = %e, "Audit log append failed");
            }
            self.record_action_audit_event(&entry, &pending.spec);
            return ActionOutcome::Rejected {
                action_id: action_id.to_string(),
                error: "operator rejected the action".to_string(),
            };
        }

        info!(action_id = %action_id, "Operator approved DESTRUCTIVE action — executing");
        let category = PolicyEngine::classify(&pending.spec);
        self.execute_and_log(action_id, &pending.spec, category, Some(true))
            .await
    }

    /// Write audit entries for any remaining pending actions and clear the map.
    ///
    /// Called from `CoreOrchestrator::shutdown()` (which takes `mut self`).
    /// Each surviving pending action is logged with `outcome="rejected"` and
    /// `operator_approved=null` — session ended before the operator responded.
    pub async fn drain_pending_on_shutdown(&mut self) {
        let ambient = self.ambient.clone();
        let guard = self.audit.lock().await;
        for (action_id, pending) in self.pending_actions.drain() {
            let category = PolicyEngine::classify(&pending.spec);
            let entry = AuditEntry {
                timestamp: Utc::now().to_rfc3339(),
                action_id: &action_id,
                r#type: Self::type_str(&pending.spec),
                category: Self::category_str(category),
                spec_json: Self::spec_to_audit_json(&pending.spec),
                outcome: "rejected",
                exit_code: None,
                output_preview: None,
                error: Some("session ended before operator responded".to_string()),
                duration_ms: None,
                operator_approved: None, // null = session ended, not an explicit rejection
            };
            if let Err(e) = guard.append(&entry) {
                error!(
                    action_id = %action_id,
                    error     = %e,
                    "Audit log append failed during shutdown drain"
                );
            }
            record_action_audit_event(&ambient, &entry, &pending.spec);
        }
    }

    /// Resolve every pending action while preserving receipt metadata captured
    /// before `resolve()` drains the pending map.
    pub async fn resolve_all_pending_with_receipts(
        &mut self,
        approved: bool,
        operator_note: &str,
    ) -> Vec<(ActionOutcome, ActionReceiptMetadata)> {
        let ids: Vec<String> = self.pending_actions.keys().cloned().collect();
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(metadata) = self.pending_receipt_metadata(&id) else {
                continue;
            };
            let outcome = self.resolve(&id, approved, operator_note).await;
            outcomes.push((outcome, metadata));
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
        action_id: &str,
        spec: &ActionSpec,
        category: ActionCategory,
        operator_approved: Option<bool>,
    ) -> ActionOutcome {
        let result = match spec {
            ActionSpec::Shell {
                args, working_dir, ..
            } => executor::execute_shell(args, working_dir.as_ref(), shell_timeout(args)).await,
            ActionSpec::FileRead { path } => executor::execute_file_read(path).await,
            ActionSpec::FileWrite {
                path,
                content,
                create_dirs,
                ..
            } => executor::execute_file_write(path, content, *create_dirs).await,
            ActionSpec::AppleScript { script, .. } => {
                executor::execute_applescript(script, applescript_timeout_secs()).await
            }
            ActionSpec::MessageSend { .. } => executor::ExecutionResult {
                success: false,
                output: String::new(),
                error: "message_send must be resolved by the orchestrator before execution"
                    .to_string(),
                exit_code: None,
                duration_ms: 0,
            },
            ActionSpec::WindowFocus {
                app_name,
                title_contains,
                ..
            } => {
                executor::execute_window_focus(
                    app_name,
                    title_contains.as_deref(),
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
            }
            ActionSpec::WindowInspect { app_name, .. } => {
                executor::execute_window_inspect(app_name.as_deref(), ACTION_DEFAULT_TIMEOUT_SECS)
                    .await
            }
            ActionSpec::UiSnapshot {
                app_name,
                max_depth,
                ..
            } => {
                executor::execute_ui_snapshot(
                    app_name.as_deref(),
                    *max_depth,
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
            }
            ActionSpec::UiClick {
                app_name,
                role,
                label,
                max_depth,
                ..
            } => {
                executor::execute_ui_click(
                    app_name.as_deref(),
                    role.as_deref(),
                    label,
                    *max_depth,
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
            }
            ActionSpec::UiType {
                app_name,
                role,
                label,
                text,
                max_depth,
                ..
            } => {
                executor::execute_ui_type(
                    app_name.as_deref(),
                    role.as_deref(),
                    label.as_deref(),
                    text,
                    *max_depth,
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
            }
            ActionSpec::Browser { action, .. } => {
                executor::execute_browser(&self.browser, action, BROWSER_WORKER_RESULT_TIMEOUT_SECS)
                    .await
            }
            ActionSpec::Shortcut {
                name,
                input_path,
                output_path,
                ..
            } => {
                executor::execute_shortcut(
                    name,
                    input_path.as_ref(),
                    output_path.as_ref(),
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
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
            timestamp: Utc::now().to_rfc3339(),
            action_id,
            r#type: Self::type_str(spec),
            category: Self::category_str(category),
            spec_json: Self::spec_to_audit_json(spec),
            outcome: outcome_str,
            exit_code: result.exit_code,
            output_preview,
            error: if result.error.is_empty() {
                None
            } else {
                Some(result.error.clone())
            },
            duration_ms: Some(result.duration_ms),
            operator_approved,
        };

        {
            let guard = self.audit.lock().await;
            if let Err(e) = guard.append(&entry) {
                error!(action_id = %action_id, error = %e, "Audit log append failed");
            }
        }
        self.record_action_audit_event(&entry, spec);

        if result.success {
            // This is the approval-path execution (handle_approval calls execute directly
            // with the stored ActionSpec). No normalization annotation needed here because
            // the approval dialog already showed the operator the action spec.
            ActionOutcome::Completed {
                action_id: action_id.to_string(),
                output: result.output,
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
                format!(
                    "command failed (exit code: {})",
                    result.exit_code.unwrap_or(-1)
                )
            };
            ActionOutcome::Rejected {
                action_id: action_id.to_string(),
                error,
            }
        }
    }

    /// Human-readable action description for `ActionRequest.description` in the UI.
    pub(crate) fn describe(spec: &ActionSpec) -> String {
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
            ActionSpec::AppleScript { script, rationale } => {
                if let Some(rationale) = rationale
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    return format!("AppleScript: {rationale}");
                }
                let preview: String = script.chars().take(80).collect();
                format!("AppleScript: {}", preview)
            }
            ActionSpec::MessageSend { recipient, .. } => {
                format!("Send iMessage to: {recipient}")
            }
            ActionSpec::WindowFocus {
                app_name,
                title_contains,
                ..
            } => match title_contains
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
            {
                Some(title) => format!("Focus window: {app_name} \"{title}\""),
                None => format!("Focus app: {app_name}"),
            },
            ActionSpec::WindowInspect { app_name, .. } => match app_name
                .as_deref()
                .map(str::trim)
                .filter(|app| !app.is_empty())
            {
                Some(app) => format!("Inspect windows: {app}"),
                None => "Inspect frontmost window".to_string(),
            },
            ActionSpec::UiSnapshot { app_name, .. } => match app_name
                .as_deref()
                .map(str::trim)
                .filter(|app| !app.is_empty())
            {
                Some(app) => format!("Snapshot UI: {app}"),
                None => "Snapshot frontmost UI".to_string(),
            },
            ActionSpec::UiClick {
                app_name,
                role,
                label,
                ..
            } => {
                let target = match app_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|app| !app.is_empty())
                {
                    Some(app) => app.to_string(),
                    None => "frontmost app".to_string(),
                };
                match role
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    Some(role) => format!("Click UI: {target} {role} \"{}\"", label.trim()),
                    None => format!("Click UI: {target} \"{}\"", label.trim()),
                }
            }
            ActionSpec::UiType {
                app_name,
                role,
                label,
                ..
            } => {
                let target = match app_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|app| !app.is_empty())
                {
                    Some(app) => app.to_string(),
                    None => "frontmost app".to_string(),
                };
                let control = match (
                    role.as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty()),
                    label
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty()),
                ) {
                    (Some(role), Some(label)) => format!("{role} \"{label}\""),
                    (Some(role), None) => role.to_string(),
                    (None, Some(label)) => format!("\"{label}\""),
                    (None, None) => "control".to_string(),
                };
                format!("Type into UI: {target} {control}")
            }
            ActionSpec::Browser { action, .. } => match action {
                BrowserActionKind::Navigate { url } => format!("Browser navigate: {url}"),
                BrowserActionKind::Click { selector } => format!("Browser click: {selector}"),
                BrowserActionKind::Type { selector, .. } => {
                    format!("Browser type into: {selector}")
                }
                BrowserActionKind::Extract { selector } => format!(
                    "Browser extract: {}",
                    selector.as_deref().unwrap_or("<page>")
                ),
                BrowserActionKind::Screenshot => "Browser screenshot".to_string(),
            },
            ActionSpec::Shortcut { name, .. } => format!("Run Shortcut: {name}"),
        }
    }

    /// Phase 24: pub(crate) so ExecutorHandle can use it for audit entries.
    pub(crate) fn type_str(spec: &ActionSpec) -> &'static str {
        match spec {
            ActionSpec::Shell { .. } => "shell",
            ActionSpec::FileRead { .. } => "file_read",
            ActionSpec::FileWrite { .. } => "file_write",
            ActionSpec::AppleScript { .. } => "applescript",
            ActionSpec::MessageSend { .. } => "message_send",
            ActionSpec::WindowFocus { .. } => "window_focus",
            ActionSpec::WindowInspect { .. } => "window_inspect",
            ActionSpec::UiSnapshot { .. } => "ui_snapshot",
            ActionSpec::UiClick { .. } => "ui_click",
            ActionSpec::UiType { .. } => "ui_type",
            ActionSpec::Browser { .. } => "browser",
            ActionSpec::Shortcut { .. } => "shortcut",
        }
    }

    pub(crate) fn category_str(cat: ActionCategory) -> &'static str {
        match cat {
            ActionCategory::Safe => "safe",
            ActionCategory::Cautious => "cautious",
            ActionCategory::Destructive => "destructive",
            _ => "unspecified",
        }
    }

    /// Produce an audit-safe JSON representation of the ActionSpec.
    ///
    /// `FileWrite.content` and `Browser::Type.text` are redacted — file contents
    /// can be megabytes, and typed text may be passwords or credentials. The audit
    /// log records intent, not data.
    pub(crate) fn spec_to_audit_json(spec: &ActionSpec) -> serde_json::Value {
        match spec {
            ActionSpec::FileWrite {
                path,
                content,
                create_dirs,
                ..
            } => {
                serde_json::json!({
                    "path":        path,
                    "content":     format!("<{} bytes omitted>", content.len()),
                    "create_dirs": create_dirs,
                })
            }
            ActionSpec::Shell {
                args,
                working_dir,
                rationale,
                ..
            } => {
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
            ActionSpec::MessageSend {
                recipient,
                body,
                rationale,
            } => {
                serde_json::json!({
                    "recipient": recipient,
                    "body": format!("<{} bytes omitted>", body.len()),
                    "rationale": rationale,
                })
            }
            ActionSpec::WindowFocus {
                app_name,
                title_contains,
                rationale,
                ..
            } => {
                serde_json::json!({
                    "app_name": app_name,
                    "title_contains": title_contains,
                    "rationale": rationale,
                })
            }
            ActionSpec::WindowInspect {
                app_name,
                rationale,
            } => {
                serde_json::json!({
                    "app_name": app_name,
                    "rationale": rationale,
                })
            }
            ActionSpec::UiSnapshot {
                app_name,
                max_depth,
                rationale,
            } => {
                serde_json::json!({
                    "app_name": app_name,
                    "max_depth": max_depth,
                    "rationale": rationale,
                })
            }
            ActionSpec::UiClick {
                app_name,
                role,
                label,
                max_depth,
                rationale,
                ..
            } => {
                serde_json::json!({
                    "app_name": app_name,
                    "role": role,
                    "label": label,
                    "max_depth": max_depth,
                    "rationale": rationale,
                })
            }
            ActionSpec::UiType {
                app_name,
                role,
                label,
                text,
                max_depth,
                rationale,
                ..
            } => {
                serde_json::json!({
                    "app_name": app_name,
                    "role": role,
                    "label": label,
                    "text": format!("<{} bytes omitted>", text.len()),
                    "max_depth": max_depth,
                    "rationale": rationale,
                })
            }
            ActionSpec::Browser {
                action, rationale, ..
            } => {
                let action_detail = match action {
                    BrowserActionKind::Navigate { url } => {
                        serde_json::json!({"action":"navigate","url":url})
                    }
                    BrowserActionKind::Click { selector } => {
                        serde_json::json!({"action":"click","selector":selector})
                    }
                    BrowserActionKind::Type { selector, .. } => {
                        // text is omitted from audit — may be a password or credential.
                        serde_json::json!({"action":"type","selector":selector,"text":"<omitted>"})
                    }
                    BrowserActionKind::Extract { selector } => {
                        serde_json::json!({"action":"extract","selector":selector})
                    }
                    BrowserActionKind::Screenshot => serde_json::json!({"action":"screenshot"}),
                };
                serde_json::json!({ "browser": action_detail, "rationale": rationale })
            }
            ActionSpec::Shortcut {
                name,
                input_path,
                output_path,
                rationale,
                ..
            } => {
                serde_json::json!({
                    "name": name,
                    "input_path": input_path,
                    "output_path": output_path,
                    "rationale": rationale,
                })
            }
        }
    }

    /// Return the path to the audit log (used for startup logging).
    #[allow(dead_code)]
    pub async fn audit_log_path(&self) -> PathBuf {
        self.audit.lock().await.path().to_path_buf()
    }

    fn record_approval_requested_event(
        &self,
        action_id: &str,
        spec: &ActionSpec,
        category: ActionCategory,
        description: &str,
        expires_at_unix_ms: u64,
        timeout_secs: u32,
    ) {
        if let Err(error) = self.ambient.record_event_and_evaluate(
            "action",
            "action_approval_requested",
            AmbientSeverity::Warn,
            "Action needs approval",
            format!("{description} is waiting for operator approval."),
            serde_json::json!({
                "action_id": action_id,
                "action_type": Self::type_str(spec),
                "category": Self::category_str(category),
                "description": Self::describe_for_ambient(spec),
                "expires_at_unix_ms": expires_at_unix_ms,
                "timeout_secs": timeout_secs
            }),
        ) {
            warn!(action_id = %action_id, error = %error, "Ambient action approval event failed");
        }
    }

    fn record_action_audit_event(&self, entry: &AuditEntry<'_>, spec: &ActionSpec) {
        record_action_audit_event(&self.ambient, entry, spec);
    }

    fn describe_for_ambient(spec: &ActionSpec) -> String {
        match spec {
            ActionSpec::AppleScript { rationale, .. } => rationale
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("AppleScript: {value}"))
                .unwrap_or_else(|| "AppleScript action".to_string()),
            ActionSpec::Browser {
                action: BrowserActionKind::Type { selector, .. },
                ..
            } => format!("Browser type into: {selector}"),
            _ => Self::describe(spec),
        }
    }
}

// ── ActionResult ─────────────────────────────────────────────────────────────

/// Delivered via `action_rx` when a background action task completes (Phase 24).
#[derive(Debug)]
pub struct ActionResult {
    pub action_id: String,
    pub action_type: String,
    pub category: String,
    /// Human-readable description of the attempted action.
    ///
    /// Background action results arrive after the model has already emitted the
    /// action block, so this copies the same sanitized wording used in approval
    /// prompts into the result path. The orchestrator uses it for deterministic
    /// operator-visible status messages without re-parsing the original spec.
    pub description: Option<String>,
    pub outcome: ActionOutcome,
    pub trace_id: String,
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
    audit: Arc<tokio::sync::Mutex<AuditLog>>,
    ambient: AmbientEventStore,
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
        spec: &ActionSpec,
        category: ActionCategory,
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

        let result = match spec {
            ActionSpec::Shell {
                args, working_dir, ..
            } => executor::execute_shell(args, working_dir.as_ref(), shell_timeout(args)).await,
            ActionSpec::FileRead { path } => executor::execute_file_read(path).await,
            ActionSpec::FileWrite {
                path,
                content,
                create_dirs,
                ..
            } => executor::execute_file_write(path, content, *create_dirs).await,
            ActionSpec::AppleScript { script, .. } => {
                executor::execute_applescript(script, applescript_timeout_secs()).await
            }
            ActionSpec::MessageSend { .. } => executor::ExecutionResult {
                success: false,
                output: String::new(),
                error: "message_send must be resolved by the orchestrator before execution"
                    .to_string(),
                exit_code: None,
                duration_ms: 0,
            },
            ActionSpec::WindowFocus {
                app_name,
                title_contains,
                ..
            } => {
                executor::execute_window_focus(
                    app_name,
                    title_contains.as_deref(),
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
            }
            ActionSpec::WindowInspect { app_name, .. } => {
                executor::execute_window_inspect(app_name.as_deref(), ACTION_DEFAULT_TIMEOUT_SECS)
                    .await
            }
            ActionSpec::UiSnapshot {
                app_name,
                max_depth,
                ..
            } => {
                executor::execute_ui_snapshot(
                    app_name.as_deref(),
                    *max_depth,
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
            }
            ActionSpec::UiClick {
                app_name,
                role,
                label,
                max_depth,
                ..
            } => {
                executor::execute_ui_click(
                    app_name.as_deref(),
                    role.as_deref(),
                    label,
                    *max_depth,
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
            }
            ActionSpec::UiType {
                app_name,
                role,
                label,
                text,
                max_depth,
                ..
            } => {
                executor::execute_ui_type(
                    app_name.as_deref(),
                    role.as_deref(),
                    label.as_deref(),
                    text,
                    *max_depth,
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
            }
            ActionSpec::Browser { action, .. } => {
                executor::execute_browser(&self.browser, action, BROWSER_WORKER_RESULT_TIMEOUT_SECS)
                    .await
            }
            ActionSpec::Shortcut {
                name,
                input_path,
                output_path,
                ..
            } => {
                executor::execute_shortcut(
                    name,
                    input_path.as_ref(),
                    output_path.as_ref(),
                    ACTION_DEFAULT_TIMEOUT_SECS,
                )
                .await
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
            timestamp: Utc::now().to_rfc3339(),
            action_id,
            r#type: ActionEngine::type_str(spec),
            category: ActionEngine::category_str(category),
            spec_json: ActionEngine::spec_to_audit_json(spec),
            outcome: outcome_str,
            exit_code: result.exit_code,
            output_preview,
            error: if result.error.is_empty() {
                None
            } else {
                Some(result.error.clone())
            },
            duration_ms: Some(result.duration_ms),
            operator_approved,
        };

        {
            let guard = self.audit.lock().await;
            if let Err(e) = guard.append(&entry) {
                error!(action_id = %action_id, error = %e, "Audit log append failed (background)");
            }
        }
        record_action_audit_event(&self.ambient, &entry, spec);

        if result.success {
            ActionOutcome::Completed {
                action_id: action_id.to_string(),
                output: result.output,
                rewritten_to, // Some("ps -Acro pid,pmem,comm") when GNU ps was normalized; None otherwise
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
                format!(
                    "command failed (exit code: {})",
                    result.exit_code.unwrap_or(-1)
                )
            };
            ActionOutcome::Rejected {
                action_id: action_id.to_string(),
                error,
            }
        }
    }
}

fn record_action_audit_event(
    ambient: &AmbientEventStore,
    entry: &AuditEntry<'_>,
    spec: &ActionSpec,
) {
    let (kind, severity, title) = match entry.outcome {
        "success" => (
            "action_succeeded",
            AmbientSeverity::Info,
            "Action completed",
        ),
        "rejected" => ("action_denied", AmbientSeverity::Warn, "Action did not run"),
        "timeout" => ("action_failed", AmbientSeverity::Warn, "Action timed out"),
        _ => ("action_failed", AmbientSeverity::Warn, "Action failed"),
    };
    let description = ActionEngine::describe_for_ambient(spec);
    let summary = match entry.outcome {
        "success" => format!("{} completed successfully.", description),
        "rejected" => format!("{} did not run.", description),
        "timeout" => format!("{} timed out.", description),
        _ => format!("{} failed.", description),
    };

    if let Err(error) = ambient.record_event_and_evaluate(
        "action",
        kind,
        severity,
        title,
        summary,
        serde_json::json!({
            "action_id": entry.action_id,
            "action_type": entry.r#type,
            "category": entry.category,
            "description": description,
            "outcome": entry.outcome,
            "exit_code": entry.exit_code,
            "duration_ms": entry.duration_ms,
            "operator_approved": entry.operator_approved,
            "has_output": entry.output_preview.is_some(),
            "has_error": entry.error.is_some()
        }),
    ) {
        warn!(
            action_id = %entry.action_id,
            outcome = %entry.outcome,
            error = %error,
            "Ambient action audit event failed"
        );
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_safe_shell() -> ActionSpec {
        ActionSpec::Shell {
            args: vec!["echo".to_string(), "engine-test".to_string()],
            working_dir: None,
            rationale: None,
            category_override: None,
        }
    }

    fn make_destructive_shell() -> ActionSpec {
        ActionSpec::Shell {
            args: vec![
                "rm".to_string(),
                "-rf".to_string(),
                "/tmp/dexter-engine-test".to_string(),
            ],
            working_dir: None,
            rationale: None,
            category_override: None,
        }
    }

    fn make_cautious_file_write(tmp: &std::path::Path) -> ActionSpec {
        ActionSpec::FileWrite {
            path: tmp.join("engine-out.txt"),
            content: "engine test content".to_string(),
            create_dirs: false,
            rationale: None,
            category_override: None,
        }
    }

    fn make_message_send() -> ActionSpec {
        ActionSpec::MessageSend {
            recipient: "Mom".to_string(),
            body: "I'll be late".to_string(),
            rationale: Some("structured send".to_string()),
        }
    }

    fn make_shortcut() -> ActionSpec {
        ActionSpec::Shortcut {
            name: "Morning Briefing".to_string(),
            input_path: Some(PathBuf::from("~/Desktop/input.txt")),
            output_path: Some(PathBuf::from("~/Desktop/output.txt")),
            rationale: Some("operator requested a Shortcut".to_string()),
            category_override: None,
        }
    }

    fn make_window_focus() -> ActionSpec {
        ActionSpec::WindowFocus {
            app_name: "Safari".to_string(),
            title_contains: Some("Dexter Docs".to_string()),
            rationale: Some("bring the relevant browser window forward".to_string()),
            category_override: None,
        }
    }

    fn make_window_inspect() -> ActionSpec {
        ActionSpec::WindowInspect {
            app_name: Some("Safari".to_string()),
            rationale: Some("confirm the current browser window".to_string()),
        }
    }

    fn make_ui_snapshot() -> ActionSpec {
        ActionSpec::UiSnapshot {
            app_name: Some("Safari".to_string()),
            max_depth: Some(2),
            rationale: Some("identify controls before clicking".to_string()),
        }
    }

    fn make_ui_click() -> ActionSpec {
        ActionSpec::UiClick {
            app_name: Some("Safari".to_string()),
            role: Some("AXButton".to_string()),
            label: "OK".to_string(),
            max_depth: Some(2),
            rationale: Some("press the visible confirmation button".to_string()),
            category_override: None,
        }
    }

    fn make_ui_type() -> ActionSpec {
        ActionSpec::UiType {
            app_name: Some("TextEdit".to_string()),
            role: Some("AXTextArea".to_string()),
            label: None,
            text: "hello Dexter".to_string(),
            max_depth: Some(2),
            rationale: Some("type into the only visible text area".to_string()),
            category_override: None,
        }
    }

    fn make_messages_send_applescript() -> ActionSpec {
        ActionSpec::AppleScript {
            script: r#"tell application "Messages"
                set targetService to 1st service whose service type = iMessage
                set targetBuddy to buddy "+15551234567" of targetService
                send "engine approval test" to targetBuddy
            end tell"#
                .to_string(),
            rationale: Some("Structured iMessage send to Test Contact".to_string()),
        }
    }

    #[test]
    fn engine_new_creates_correctly() {
        let tmp = tempdir().unwrap();
        let engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());
        assert_eq!(engine.state_dir, tmp.path());
        assert!(engine.pending_actions.is_empty());
    }

    #[test]
    fn message_send_describe_type_and_audit_are_safe() {
        let spec = make_message_send();
        assert_eq!(ActionEngine::describe(&spec), "Send iMessage to: Mom");
        assert_eq!(ActionEngine::type_str(&spec), "message_send");

        let audit = ActionEngine::spec_to_audit_json(&spec);
        assert_eq!(audit["recipient"], "Mom");
        assert_eq!(audit["body"], "<12 bytes omitted>");
        assert_eq!(audit["rationale"], "structured send");
    }

    #[test]
    fn shortcut_describe_type_and_audit_are_readable() {
        let spec = make_shortcut();
        assert_eq!(
            ActionEngine::describe(&spec),
            "Run Shortcut: Morning Briefing"
        );
        assert_eq!(ActionEngine::type_str(&spec), "shortcut");

        let audit = ActionEngine::spec_to_audit_json(&spec);
        assert_eq!(audit["name"], "Morning Briefing");
        assert_eq!(audit["input_path"], "~/Desktop/input.txt");
        assert_eq!(audit["output_path"], "~/Desktop/output.txt");
        assert_eq!(audit["rationale"], "operator requested a Shortcut");
    }

    #[test]
    fn window_focus_describe_type_and_audit_are_readable() {
        let spec = make_window_focus();
        assert_eq!(
            ActionEngine::describe(&spec),
            "Focus window: Safari \"Dexter Docs\""
        );
        assert_eq!(ActionEngine::type_str(&spec), "window_focus");

        let audit = ActionEngine::spec_to_audit_json(&spec);
        assert_eq!(audit["app_name"], "Safari");
        assert_eq!(audit["title_contains"], "Dexter Docs");
        assert_eq!(
            audit["rationale"],
            "bring the relevant browser window forward"
        );
    }

    #[test]
    fn window_inspect_describe_type_and_audit_are_readable() {
        let spec = make_window_inspect();
        assert_eq!(ActionEngine::describe(&spec), "Inspect windows: Safari");
        assert_eq!(ActionEngine::type_str(&spec), "window_inspect");

        let audit = ActionEngine::spec_to_audit_json(&spec);
        assert_eq!(audit["app_name"], "Safari");
        assert_eq!(audit["rationale"], "confirm the current browser window");
    }

    #[test]
    fn ui_snapshot_describe_type_and_audit_are_readable() {
        let spec = make_ui_snapshot();
        assert_eq!(ActionEngine::describe(&spec), "Snapshot UI: Safari");
        assert_eq!(ActionEngine::type_str(&spec), "ui_snapshot");

        let audit = ActionEngine::spec_to_audit_json(&spec);
        assert_eq!(audit["app_name"], "Safari");
        assert_eq!(audit["max_depth"], 2);
        assert_eq!(audit["rationale"], "identify controls before clicking");
    }

    #[test]
    fn ui_click_describe_type_and_audit_are_readable() {
        let spec = make_ui_click();
        assert_eq!(
            ActionEngine::describe(&spec),
            "Click UI: Safari AXButton \"OK\""
        );
        assert_eq!(ActionEngine::type_str(&spec), "ui_click");

        let audit = ActionEngine::spec_to_audit_json(&spec);
        assert_eq!(audit["app_name"], "Safari");
        assert_eq!(audit["role"], "AXButton");
        assert_eq!(audit["label"], "OK");
        assert_eq!(audit["max_depth"], 2);
        assert_eq!(audit["rationale"], "press the visible confirmation button");
    }

    #[test]
    fn ui_type_describe_type_and_audit_redacts_text() {
        let spec = make_ui_type();
        assert_eq!(
            ActionEngine::describe(&spec),
            "Type into UI: TextEdit AXTextArea"
        );
        assert_eq!(ActionEngine::type_str(&spec), "ui_type");

        let audit = ActionEngine::spec_to_audit_json(&spec);
        assert_eq!(audit["app_name"], "TextEdit");
        assert_eq!(audit["role"], "AXTextArea");
        assert_eq!(audit["label"], serde_json::Value::Null);
        assert_eq!(audit["text"], "<12 bytes omitted>");
        assert_eq!(audit["max_depth"], 2);
        assert_eq!(audit["rationale"], "type into the only visible text area");
    }

    #[test]
    fn approval_timeout_env_override_only_shortens_default() {
        const KEY: &str = "DEXTER_ACTION_APPROVAL_TIMEOUT_SECS";
        let old = std::env::var(KEY).ok();

        std::env::set_var(KEY, "2");
        assert_eq!(approval_timeout_secs(), 2);

        std::env::set_var(KEY, (ACTION_APPROVAL_TIMEOUT_SECS + 60).to_string());
        assert_eq!(approval_timeout_secs(), ACTION_APPROVAL_TIMEOUT_SECS);

        std::env::set_var(KEY, "0");
        assert_eq!(approval_timeout_secs(), ACTION_APPROVAL_TIMEOUT_SECS);

        match old {
            Some(value) => std::env::set_var(KEY, value),
            None => std::env::remove_var(KEY),
        }
    }

    #[test]
    fn applescript_timeout_env_override_only_shortens_default() {
        const KEY: &str = "DEXTER_ACTION_APPLESCRIPT_TIMEOUT_SECS";
        let old = std::env::var(KEY).ok();

        std::env::set_var(KEY, "2");
        assert_eq!(applescript_timeout_secs(), 2);

        std::env::set_var(KEY, (ACTION_APPLESCRIPT_TIMEOUT_SECS + 60).to_string());
        assert_eq!(applescript_timeout_secs(), ACTION_APPLESCRIPT_TIMEOUT_SECS);

        std::env::set_var(KEY, "0");
        assert_eq!(applescript_timeout_secs(), ACTION_APPLESCRIPT_TIMEOUT_SECS);

        match old {
            Some(value) => std::env::set_var(KEY, value),
            None => std::env::remove_var(KEY),
        }
    }

    #[test]
    fn shell_timeout_uses_default_for_non_download_commands() {
        assert_eq!(
            shell_timeout(&["echo".to_string(), "hello".to_string()]),
            ACTION_DEFAULT_TIMEOUT_SECS
        );
        assert_eq!(shell_timeout(&[]), ACTION_DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn shell_timeout_extends_for_download_tool_names_and_absolute_paths() {
        for bin in [
            "yt-dlp",
            "/opt/homebrew/bin/yt-dlp",
            "/usr/local/bin/curl",
            "/opt/homebrew/bin/wget",
            "/opt/homebrew/bin/ffmpeg",
            "/opt/homebrew/bin/ffprobe",
        ] {
            assert_eq!(
                shell_timeout(&[bin.to_string(), "--version".to_string()]),
                ACTION_DOWNLOAD_TIMEOUT_SECS,
                "{bin} should use the extended download timeout"
            );
        }
    }

    #[tokio::test]
    async fn message_send_fails_closed_if_it_reaches_action_engine() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());
        let outcome = engine
            .submit(make_message_send(), "trace-message-send")
            .await;
        match outcome {
            ActionOutcome::Rejected { error, .. } => {
                assert!(
                    error.contains("must be resolved by the orchestrator"),
                    "unexpected error: {error}"
                );
            }
            other => panic!("message_send must not execute generically: {other:?}"),
        }
    }

    #[tokio::test]
    async fn messages_send_applescript_requires_approval() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());
        let outcome = engine
            .submit(
                make_messages_send_applescript(),
                "trace-message-applescript",
            )
            .await;
        match outcome {
            ActionOutcome::PendingApproval {
                category,
                description,
                ..
            } => {
                assert_eq!(category, ActionCategory::Destructive);
                assert!(
                    description.starts_with("AppleScript:"),
                    "unexpected description: {description}"
                );
                assert!(
                    description.contains("Structured iMessage send to Test Contact"),
                    "AppleScript descriptions should prefer rationale over script body: {description}"
                );
                assert_eq!(engine.pending_actions.len(), 1);
            }
            other => panic!("Messages send AppleScript must await approval: {other:?}"),
        }
    }

    #[test]
    fn describe_applescript_prefers_rationale_over_script_preview() {
        let spec = make_messages_send_applescript();
        assert_eq!(
            ActionEngine::describe(&spec),
            "AppleScript: Structured iMessage send to Test Contact"
        );
    }

    #[tokio::test]
    async fn pending_receipt_metadata_returns_label_for_queued_action() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());
        let spec = ActionSpec::Shell {
            args: vec!["echo".to_string(), "pending-description".to_string()],
            working_dir: None,
            rationale: None,
            category_override: Some("destructive".to_string()),
        };

        let outcome = engine.submit(spec, "trace-pending-description").await;
        let action_id = match outcome {
            ActionOutcome::PendingApproval { action_id, .. } => action_id,
            other => panic!("expected PendingApproval, got: {other:?}"),
        };

        let metadata = engine
            .pending_receipt_metadata(&action_id)
            .expect("pending action should have receipt metadata");
        assert_eq!(metadata.description, "Run: echo pending-description");
        assert_eq!(metadata.action_type, "shell");
        assert_eq!(metadata.category, "destructive");
        assert!(engine.pending_receipt_metadata("missing-action").is_none());
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
        assert!(
            engine.audit.lock().await.path().exists(),
            "audit log must be created after first action"
        );
    }

    #[tokio::test]
    async fn submit_safe_shell_records_ambient_success_event() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());

        let outcome = engine
            .submit(make_safe_shell(), "trace-ambient-success")
            .await;
        let action_id = match outcome {
            ActionOutcome::Completed { action_id, .. } => action_id,
            other => panic!("expected Completed, got: {other:?}"),
        };

        let (_, events) = engine
            .ambient
            .recent_events(5)
            .expect("ambient events should be readable");
        let event = events
            .iter()
            .find(|event| event.kind == "action_succeeded")
            .expect("successful action should create an ambient event");
        assert_eq!(event.source, "action");
        assert_eq!(event.severity, AmbientSeverity::Info);
        assert_eq!(event.payload["action_id"], action_id);
        assert_eq!(event.payload["action_type"], "shell");
        assert_eq!(event.payload["outcome"], "success");
    }

    #[tokio::test]
    async fn submit_cautious_file_write_executes_immediately() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());
        let spec = make_cautious_file_write(tmp.path());

        let outcome = engine.submit(spec, "trace-002").await;
        assert!(
            matches!(outcome, ActionOutcome::Completed { .. }),
            "got: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn submit_destructive_stores_pending() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());

        let outcome = engine.submit(make_destructive_shell(), "trace-003").await;
        match outcome {
            ActionOutcome::PendingApproval { action_id, .. } => {
                assert_eq!(
                    engine.pending_actions.len(),
                    1,
                    "must be stored in pending_actions"
                );
                let (_, events) = engine
                    .ambient
                    .recent_events(5)
                    .expect("ambient events should be readable");
                let event = events
                    .iter()
                    .find(|event| event.kind == "action_approval_requested")
                    .expect("pending destructive action should create an ambient event");
                assert_eq!(event.severity, AmbientSeverity::Warn);
                assert_eq!(event.payload["action_id"], action_id);
                assert_eq!(event.payload["category"], "destructive");
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
            args: vec!["echo".to_string(), "approved-test".to_string()],
            working_dir: None,
            rationale: None,
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
        assert_eq!(
            engine.pending_actions.len(),
            0,
            "must be removed from pending_actions"
        );
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
        assert!(
            matches!(resolved, ActionOutcome::Rejected { .. }),
            "got: {resolved:?}"
        );
        assert_eq!(engine.pending_actions.len(), 0);

        let (_, events) = engine
            .ambient
            .recent_events(10)
            .expect("ambient events should be readable");
        let event = events
            .iter()
            .find(|event| event.kind == "action_denied")
            .expect("operator denial should create an ambient event");
        assert_eq!(event.severity, AmbientSeverity::Warn);
        assert_eq!(event.payload["action_id"], action_id);
        assert_eq!(event.payload["outcome"], "rejected");
        assert_eq!(event.payload["operator_approved"], false);
    }

    #[tokio::test]
    async fn resolve_expired_action_refuses_late_approval() {
        let tmp = tempdir().unwrap();
        let mut engine = ActionEngine::new(tmp.path(), BrowserCoordinator::new_degraded());

        let spec = ActionSpec::Shell {
            args: vec![
                "echo".to_string(),
                "late-approval-should-not-run".to_string(),
            ],
            working_dir: None,
            rationale: None,
            category_override: Some("destructive".to_string()),
        };

        let outcome = engine.submit(spec, "trace-expired-approval").await;
        let action_id = match outcome {
            ActionOutcome::PendingApproval { action_id, .. } => action_id,
            other => panic!("expected PendingApproval, got: {other:?}"),
        };
        let pending = engine
            .pending_actions
            .get_mut(&action_id)
            .expect("pending action should exist");
        pending.expires_at = Utc::now() - chrono::Duration::seconds(1);

        let resolved = engine
            .resolve(&action_id, true, "late approval by test")
            .await;
        match resolved {
            ActionOutcome::Rejected { error, .. } => {
                assert!(
                    error.contains("approval expired"),
                    "late approval should be rejected as expired: {error}"
                );
            }
            other => panic!("expired approval must not execute: {other:?}"),
        }
        assert_eq!(engine.pending_actions.len(), 0);

        let audit_path = engine.audit.lock().await.path().to_path_buf();
        let audit = std::fs::read_to_string(audit_path).expect("audit log should exist");
        assert!(audit.contains("\"outcome\":\"rejected\""));
        assert!(audit.contains("approval expired before operator response"));
        assert!(audit.contains("\"operator_approved\":false"));
        assert!(!audit.contains("late-approval-should-not-run\\n"));
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
