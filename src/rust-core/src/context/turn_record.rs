use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    diagnostics::CompiledContextDiagnostics, ledger::TurnOutcomeLabel, representation::fingerprint,
    TaskClass,
};
use crate::action::{ActionOutcome, ActionSpec};

const SCHEMA_VERSION: &str = "context_turn_record_v1";
const USER_PREVIEW_CHARS: usize = 180;
const OUTPUT_PREVIEW_CHARS: usize = 240;

#[derive(Debug, Error)]
pub enum TurnRecordError {
    #[error("turn record IO failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("turn record serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("turn record trace_id not found: {0}")]
    MissingTrace(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextTurnRecord {
    pub schema_version: String,
    pub privacy_mode: TurnRecordPrivacyMode,
    pub session_id: String,
    pub trace_id: String,
    pub turn_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub task_class: TaskClass,
    pub route_category: Option<String>,
    pub model: Option<String>,
    pub user_text_hash: String,
    pub user_text_preview: String,
    pub context_diagnostics: CompiledContextDiagnostics,
    pub generation: Option<GenerationRecord>,
    pub action: Option<ActionRecord>,
    pub outcome_label: TurnOutcomeLabel,
    pub close_reason: TurnCloseReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRecordPrivacyMode {
    RedactedPreviewV1,
}

#[derive(Debug, Clone)]
pub struct GenerationRecordInput {
    pub first_token_ms: Option<u64>,
    pub total_ms: u64,
    pub token_count: u32,
    pub cancelled: bool,
    pub response_len: usize,
    pub prompt_eval_count: Option<u64>,
    pub prompt_eval_ms: Option<u64>,
    pub load_ms: Option<u64>,
    pub eval_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub first_token_ms: Option<u64>,
    pub total_ms: u64,
    pub token_count: u32,
    pub cancelled: bool,
    pub response_len: usize,
    pub prompt_eval_count: Option<u64>,
    pub prompt_eval_ms: Option<u64>,
    pub load_ms: Option<u64>,
    pub eval_ms: Option<u64>,
    pub output_hash: String,
    pub output_preview: String,
    pub parsed_action_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub action_id: Option<String>,
    pub receipt_id: Option<String>,
    pub action_kind: String,
    pub policy: Option<String>,
    pub duration_ms: Option<u64>,
    pub stdout_hash: Option<String>,
    pub stderr_hash: Option<String>,
    pub error_kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCloseReason {
    Open,
    AnsweredNoAction,
    ActionCompleted,
    ActionRejected,
    ActionTimedOut,
    CancelledByUser,
    BargeIn,
    SupersededByNewInput,
    GenerationFailed,
    DaemonShutdown,
    AggregatorTtlExpired,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct TurnDispatchInput {
    pub session_id: String,
    pub trace_id: String,
    pub turn_id: String,
    pub task_class: TaskClass,
    pub route_category: Option<String>,
    pub model: Option<String>,
    pub user_text: String,
    pub context_diagnostics: CompiledContextDiagnostics,
}

pub struct TurnRecordAggregator {
    records: HashMap<String, ContextTurnRecord>,
    state_dir: PathBuf,
}

impl TurnRecordAggregator {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            records: HashMap::new(),
            state_dir: state_dir.into(),
        }
    }

    pub fn start_turn(&mut self, input: TurnDispatchInput) -> Result<(), TurnRecordError> {
        let now = Utc::now();
        let record = ContextTurnRecord {
            schema_version: SCHEMA_VERSION.to_string(),
            privacy_mode: TurnRecordPrivacyMode::RedactedPreviewV1,
            session_id: input.session_id,
            trace_id: input.trace_id.clone(),
            turn_id: input.turn_id,
            created_at: now,
            updated_at: now,
            task_class: input.task_class,
            route_category: input.route_category,
            model: input.model,
            user_text_hash: fingerprint(&input.user_text),
            user_text_preview: preview(&input.user_text, USER_PREVIEW_CHARS),
            context_diagnostics: input.context_diagnostics,
            generation: None,
            action: None,
            outcome_label: TurnOutcomeLabel::Unknown,
            close_reason: TurnCloseReason::Open,
        };
        self.write_record(&record)?;
        self.records.insert(input.trace_id, record);
        Ok(())
    }

    pub fn attach_generation(
        &mut self,
        trace_id: &str,
        telemetry: &GenerationRecordInput,
        output: &str,
        parsed_action: Option<&ActionSpec>,
    ) -> Result<(), TurnRecordError> {
        let record = self
            .records
            .get_mut(trace_id)
            .ok_or_else(|| TurnRecordError::MissingTrace(trace_id.to_string()))?;
        record.updated_at = Utc::now();
        record.generation = Some(GenerationRecord {
            first_token_ms: telemetry.first_token_ms,
            total_ms: telemetry.total_ms,
            token_count: telemetry.token_count,
            cancelled: telemetry.cancelled,
            response_len: telemetry.response_len,
            prompt_eval_count: telemetry.prompt_eval_count,
            prompt_eval_ms: telemetry.prompt_eval_ms,
            load_ms: telemetry.load_ms,
            eval_ms: telemetry.eval_ms,
            output_hash: fingerprint(output),
            output_preview: preview(output, OUTPUT_PREVIEW_CHARS),
            parsed_action_kind: parsed_action.map(action_kind).map(ToOwned::to_owned),
        });
        if telemetry.cancelled {
            record.outcome_label = TurnOutcomeLabel::UserCancelled;
            record.close_reason = TurnCloseReason::BargeIn;
        }
        let cloned = record.clone();
        self.write_record(&cloned)
    }

    pub fn attach_action_result(
        &mut self,
        trace_id: &str,
        action_type: &str,
        policy: Option<&str>,
        outcome: &ActionOutcome,
    ) -> Result<(), TurnRecordError> {
        let record = self
            .records
            .get_mut(trace_id)
            .ok_or_else(|| TurnRecordError::MissingTrace(trace_id.to_string()))?;
        record.updated_at = Utc::now();
        let (action_id, stdout_hash, stderr_hash, error_kind, outcome_label, close_reason) =
            action_summary(outcome);
        record.action = Some(ActionRecord {
            action_id,
            receipt_id: None,
            action_kind: action_type.to_string(),
            policy: policy.map(ToOwned::to_owned),
            duration_ms: None,
            stdout_hash,
            stderr_hash,
            error_kind,
        });
        record.outcome_label = outcome_label;
        record.close_reason = close_reason;
        let cloned = record.clone();
        self.write_record(&cloned)
    }

    pub fn close_turn(
        &mut self,
        trace_id: &str,
        close_reason: TurnCloseReason,
    ) -> Result<(), TurnRecordError> {
        let record = self
            .records
            .get_mut(trace_id)
            .ok_or_else(|| TurnRecordError::MissingTrace(trace_id.to_string()))?;
        if record.close_reason != TurnCloseReason::Open
            && close_reason == TurnCloseReason::AnsweredNoAction
        {
            return Ok(());
        }
        record.updated_at = Utc::now();
        record.close_reason = close_reason;
        if record.outcome_label == TurnOutcomeLabel::Unknown {
            record.outcome_label = outcome_for_close_reason(close_reason);
        }
        let cloned = record.clone();
        self.write_record(&cloned)?;
        if !matches!(
            close_reason,
            TurnCloseReason::Open | TurnCloseReason::Unknown | TurnCloseReason::DaemonShutdown
        ) {
            self.records.remove(trace_id);
        }
        Ok(())
    }

    pub fn close_all_open(&mut self, close_reason: TurnCloseReason) -> Result<(), TurnRecordError> {
        let trace_ids = self
            .records
            .iter()
            .filter(|(_, record)| record.close_reason == TurnCloseReason::Open)
            .map(|(trace_id, _)| trace_id.clone())
            .collect::<Vec<_>>();
        for trace_id in trace_ids {
            self.close_turn(&trace_id, close_reason)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn record_path_for_trace(&self, trace_id: &str) -> PathBuf {
        let date = Utc::now().format("%Y%m%d").to_string();
        self.record_path_for_date(trace_id, &date)
    }

    fn record_path_for_date(&self, trace_id: &str, date: &str) -> PathBuf {
        self.state_dir
            .join("context_turns")
            .join(date)
            .join(format!("{}.json", trace_record_filename_stem(trace_id)))
    }

    fn write_record(&self, record: &ContextTurnRecord) -> Result<(), TurnRecordError> {
        let date = record.created_at.format("%Y%m%d").to_string();
        let final_path = self.record_path_for_date(&record.trace_id, &date);
        let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| TurnRecordError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let bytes = serde_json::to_vec_pretty(record)?;
        let tmp_path = final_path.with_extension("json.tmp");
        fs::write(&tmp_path, bytes).map_err(|source| TurnRecordError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        fs::rename(&tmp_path, &final_path).map_err(|source| TurnRecordError::Io {
            path: final_path,
            source,
        })?;
        Ok(())
    }
}

fn preview(text: &str, max_chars: usize) -> String {
    let mut value = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        value.push_str("...");
    }
    value
}

fn sanitize_trace_id(trace_id: &str) -> String {
    let sanitized = trace_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown_trace".to_string()
    } else {
        sanitized
    }
}

fn trace_record_filename_stem(trace_id: &str) -> String {
    let hash = fingerprint(trace_id);
    let short_hash = &hash[..12];
    format!("{}-{}", sanitize_trace_id(trace_id), short_hash)
}

fn action_kind(spec: &ActionSpec) -> &'static str {
    match spec {
        ActionSpec::Shell { .. } => "shell",
        ActionSpec::FileRead { .. } => "file_read",
        ActionSpec::FileWrite { .. } => "file_write",
        ActionSpec::AppleScript { .. } => "apple_script",
        ActionSpec::MessageSend { .. } => "message_send",
        ActionSpec::Browser { .. } => "browser",
        ActionSpec::Shortcut { .. } => "shortcut",
        ActionSpec::WindowFocus { .. } => "window_focus",
        ActionSpec::WindowInspect { .. } => "window_inspect",
        ActionSpec::UiSnapshot { .. } => "ui_snapshot",
        ActionSpec::UiClick { .. } => "ui_click",
        ActionSpec::UiType { .. } => "ui_type",
        ActionSpec::UiSelect { .. } => "ui_select",
        ActionSpec::UiToggle { .. } => "ui_toggle",
        ActionSpec::UiPick { .. } => "ui_pick",
    }
}

fn action_summary(
    outcome: &ActionOutcome,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    TurnOutcomeLabel,
    TurnCloseReason,
) {
    match outcome {
        ActionOutcome::Completed {
            action_id, output, ..
        } => {
            // ActionEngine uses `Completed` for semantically successful actions.
            // If future executors distinguish non-zero process exits inside this
            // variant, ledger learning must stop treating this as a success label.
            (
                Some(action_id.clone()),
                Some(fingerprint(output)),
                None,
                None,
                TurnOutcomeLabel::ActionExecutedSuccessfully,
                TurnCloseReason::ActionCompleted,
            )
        }
        ActionOutcome::Rejected { action_id, error } => (
            Some(action_id.clone()),
            None,
            Some(fingerprint(error)),
            Some(classify_action_error(error).to_string()),
            TurnOutcomeLabel::ActionRejectedByPolicy,
            TurnCloseReason::ActionRejected,
        ),
        ActionOutcome::PendingApproval { action_id, .. } => (
            Some(action_id.clone()),
            None,
            None,
            Some("pending_approval".to_string()),
            TurnOutcomeLabel::Unknown,
            TurnCloseReason::Open,
        ),
    }
}

fn classify_action_error(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        "timeout"
    } else if lower.contains("applescript") || lower.contains("osascript") {
        "apple_script"
    } else if lower.contains("not found") || lower.contains("no such") {
        "not_found"
    } else if lower.contains("permission") || lower.contains("not authorized") {
        "permission"
    } else {
        "action_error"
    }
}

fn outcome_for_close_reason(reason: TurnCloseReason) -> TurnOutcomeLabel {
    match reason {
        TurnCloseReason::AnsweredNoAction => TurnOutcomeLabel::Answered,
        TurnCloseReason::ActionCompleted => TurnOutcomeLabel::ActionExecutedSuccessfully,
        TurnCloseReason::ActionRejected => TurnOutcomeLabel::ActionRejectedByPolicy,
        TurnCloseReason::CancelledByUser | TurnCloseReason::BargeIn => {
            TurnOutcomeLabel::UserCancelled
        }
        _ => TurnOutcomeLabel::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::diagnostics::{CompiledContextDiagnostics, CompilerScope, TokenCostMethod};

    fn diagnostics_with_secret() -> CompiledContextDiagnostics {
        CompiledContextDiagnostics {
            compiler_version: "test".to_string(),
            scope: CompilerScope::AmbientOnly,
            token_cost_method: TokenCostMethod::CharHeuristicV1,
            budget_tokens: 100,
            reserved_output_tokens: 10,
            estimated_used_tokens: 1,
            mandatory_tokens: 0,
            optional_tokens: 1,
            included: Vec::new(),
            dropped: Vec::new(),
        }
    }

    fn dispatch_input(state_marker: &str) -> TurnDispatchInput {
        TurnDispatchInput {
            session_id: "session-a".to_string(),
            trace_id: state_marker.to_string(),
            turn_id: "turn-a".to_string(),
            task_class: TaskClass::Chat,
            route_category: Some("Chat".to_string()),
            model: Some("qwen3:8b".to_string()),
            user_text: "explain this".to_string(),
            context_diagnostics: diagnostics_with_secret(),
        }
    }

    #[test]
    fn start_turn_writes_initial_record() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder.start_turn(dispatch_input("trace-start")).unwrap();

        let path = recorder.record_path_for_trace("trace-start");
        let record: ContextTurnRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(record.trace_id, "trace-start");
        assert_eq!(
            record.privacy_mode,
            TurnRecordPrivacyMode::RedactedPreviewV1
        );
        assert_eq!(record.close_reason, TurnCloseReason::Open);
        assert!(record.generation.is_none());
    }

    #[test]
    fn attach_generation_updates_existing_record() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder.start_turn(dispatch_input("trace-gen")).unwrap();
        let telemetry = GenerationRecordInput {
            first_token_ms: Some(12),
            total_ms: 34,
            token_count: 5,
            cancelled: false,
            response_len: 11,
            prompt_eval_count: Some(42),
            prompt_eval_ms: Some(250),
            load_ms: Some(10),
            eval_ms: Some(20),
        };

        recorder
            .attach_generation("trace-gen", &telemetry, "hello world", None)
            .unwrap();

        let record: ContextTurnRecord =
            serde_json::from_slice(&fs::read(recorder.record_path_for_trace("trace-gen")).unwrap())
                .unwrap();
        let generation = record.generation.expect("generation must be attached");
        assert_eq!(generation.first_token_ms, Some(12));
        assert_eq!(generation.prompt_eval_count, Some(42));
        assert_eq!(generation.prompt_eval_ms, Some(250));
        assert_eq!(generation.output_hash, fingerprint("hello world"));
    }

    #[test]
    fn answered_no_action_closes_record() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder.start_turn(dispatch_input("trace-close")).unwrap();

        recorder
            .close_turn("trace-close", TurnCloseReason::AnsweredNoAction)
            .unwrap();

        let record: ContextTurnRecord = serde_json::from_slice(
            &fs::read(recorder.record_path_for_trace("trace-close")).unwrap(),
        )
        .unwrap();
        assert_eq!(record.outcome_label, TurnOutcomeLabel::Answered);
        assert_eq!(record.close_reason, TurnCloseReason::AnsweredNoAction);
    }

    #[test]
    fn action_result_maps_to_turn_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder.start_turn(dispatch_input("trace-action")).unwrap();
        let outcome = ActionOutcome::Completed {
            action_id: "action-1".to_string(),
            output: "done".to_string(),
            rewritten_to: None,
        };

        recorder
            .attach_action_result("trace-action", "shell", Some("safe"), &outcome)
            .unwrap();

        let record: ContextTurnRecord = serde_json::from_slice(
            &fs::read(recorder.record_path_for_trace("trace-action")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            record.outcome_label,
            TurnOutcomeLabel::ActionExecutedSuccessfully
        );
        assert_eq!(record.close_reason, TurnCloseReason::ActionCompleted);
        assert_eq!(
            record.action.unwrap().stdout_hash,
            Some(fingerprint("done"))
        );
    }

    #[test]
    fn records_do_not_store_raw_context_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder
            .start_turn(dispatch_input("trace-privacy"))
            .unwrap();

        let json = fs::read_to_string(recorder.record_path_for_trace("trace-privacy")).unwrap();
        assert!(!json.contains("PRIVATE_SECRET_CONTEXT_PAYLOAD"));
        assert!(json.contains("user_text_hash"));
    }

    #[test]
    fn record_path_uses_configured_state_dir_and_sanitizes_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = TurnRecordAggregator::new(tmp.path());
        let path = recorder.record_path_for_trace("trace/with:bad chars");

        assert!(path.starts_with(tmp.path()));
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(file_name.starts_with("trace_with_bad_chars-"));
        assert!(file_name.ends_with(".json"));
    }

    #[test]
    fn sanitized_trace_filename_includes_hash_suffix_to_avoid_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = TurnRecordAggregator::new(tmp.path());
        let path_a = recorder.record_path_for_trace("trace/with:bad chars");
        let path_b = recorder.record_path_for_trace("trace:with/bad chars");

        assert_ne!(path_a, path_b);
    }

    #[test]
    fn shutdown_closes_open_records_as_daemon_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder
            .start_turn(dispatch_input("trace-shutdown"))
            .unwrap();

        recorder
            .close_all_open(TurnCloseReason::DaemonShutdown)
            .unwrap();

        let record: ContextTurnRecord = serde_json::from_slice(
            &fs::read(recorder.record_path_for_trace("trace-shutdown")).unwrap(),
        )
        .unwrap();
        assert_eq!(record.close_reason, TurnCloseReason::DaemonShutdown);
    }

    #[test]
    fn shutdown_does_not_overwrite_already_closed_action_record() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder
            .start_turn(dispatch_input("trace-action-closed"))
            .unwrap();
        let outcome = ActionOutcome::Completed {
            action_id: "action-closed".to_string(),
            output: "done".to_string(),
            rewritten_to: None,
        };
        recorder
            .attach_action_result("trace-action-closed", "shell", Some("safe"), &outcome)
            .unwrap();

        recorder
            .close_all_open(TurnCloseReason::DaemonShutdown)
            .unwrap();

        let record: ContextTurnRecord = serde_json::from_slice(
            &fs::read(recorder.record_path_for_trace("trace-action-closed")).unwrap(),
        )
        .unwrap();
        assert_eq!(record.close_reason, TurnCloseReason::ActionCompleted);
        assert_eq!(
            record.outcome_label,
            TurnOutcomeLabel::ActionExecutedSuccessfully
        );
    }

    #[test]
    fn atomic_write_leaves_valid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder.start_turn(dispatch_input("trace-atomic")).unwrap();
        recorder
            .close_turn("trace-atomic", TurnCloseReason::AnsweredNoAction)
            .unwrap();

        let path = recorder.record_path_for_trace("trace-atomic");
        let _record: ContextTurnRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
    }
}
