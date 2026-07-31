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
use crate::system::runtime::RuntimeAttestation;

const SCHEMA_VERSION: &str = "context_turn_record_v1";
const USER_PREVIEW_CHARS: usize = 180;
const OUTPUT_PREVIEW_CHARS: usize = 240;
const ACTION_DIAGNOSTIC_DETAIL_CHARS: usize = 240;
const ACTION_DIAGNOSTIC_RECOVERY_CHARS: usize = 240;
const ACTION_DIAGNOSTIC_TARGET_CHARS: usize = 320;
const ACTION_DIAGNOSTIC_EVIDENCE_CHARS: usize = 520;

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
    #[serde(default = "RuntimeAttestation::current")]
    pub runtime: RuntimeAttestation,
    #[serde(default)]
    pub evidence: Vec<EvidenceRecord>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub source: String,
    pub observed_at: DateTime<Utc>,
    pub detail: String,
    pub payload_hash: Option<String>,
}

impl EvidenceRecord {
    pub fn new(source: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            observed_at: Utc::now(),
            detail: detail.into(),
            payload_hash: None,
        }
    }

    pub fn with_payload_hash(mut self, payload: &str) -> Self {
        self.payload_hash = Some(fingerprint(payload));
        self
    }
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
    pub diagnostic: Option<ActionDiagnosticRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionDiagnosticSource {
    Browser,
    UiWindow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDiagnosticRecord {
    pub source: ActionDiagnosticSource,
    pub failure_kind: String,
    pub recovery_directive: Option<String>,
    pub recovery_hint: Option<String>,
    pub detail_preview: Option<String>,
    pub target_preview: Option<String>,
    pub evidence_preview: Option<String>,
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
    pub evidence: Vec<EvidenceRecord>,
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
            runtime: RuntimeAttestation::current(),
            evidence: input.evidence,
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

    pub fn attach_evidence(
        &mut self,
        trace_id: &str,
        evidence: EvidenceRecord,
    ) -> Result<(), TurnRecordError> {
        let record = self
            .records
            .get_mut(trace_id)
            .ok_or_else(|| TurnRecordError::MissingTrace(trace_id.to_string()))?;
        record.updated_at = Utc::now();
        record.evidence.push(evidence);
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
        let summary = action_summary(outcome);
        record.action = Some(ActionRecord {
            action_id: summary.action_id,
            receipt_id: None,
            action_kind: action_type.to_string(),
            policy: policy.map(ToOwned::to_owned),
            duration_ms: None,
            stdout_hash: summary.stdout_hash,
            stderr_hash: summary.stderr_hash,
            error_kind: summary.error_kind,
            diagnostic: summary.diagnostic,
        });
        record.outcome_label = summary.outcome_label;
        record.close_reason = summary.close_reason;
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

struct ActionSummary {
    action_id: Option<String>,
    stdout_hash: Option<String>,
    stderr_hash: Option<String>,
    error_kind: Option<String>,
    diagnostic: Option<ActionDiagnosticRecord>,
    outcome_label: TurnOutcomeLabel,
    close_reason: TurnCloseReason,
}

fn action_summary(outcome: &ActionOutcome) -> ActionSummary {
    match outcome {
        ActionOutcome::Completed {
            action_id, output, ..
        } => {
            // ActionEngine uses `Completed` for semantically successful actions.
            // If future executors distinguish non-zero process exits inside this
            // variant, ledger learning must stop treating this as a success label.
            ActionSummary {
                action_id: Some(action_id.clone()),
                stdout_hash: Some(fingerprint(output)),
                stderr_hash: None,
                error_kind: None,
                diagnostic: None,
                outcome_label: TurnOutcomeLabel::ActionExecutedSuccessfully,
                close_reason: TurnCloseReason::ActionCompleted,
            }
        }
        ActionOutcome::Rejected { action_id, error } => ActionSummary {
            action_id: Some(action_id.clone()),
            stdout_hash: None,
            stderr_hash: Some(fingerprint(error)),
            error_kind: Some(classify_action_error(error).to_string()),
            diagnostic: structured_action_diagnostic(error),
            outcome_label: TurnOutcomeLabel::ActionRejectedByPolicy,
            close_reason: TurnCloseReason::ActionRejected,
        },
        ActionOutcome::PendingApproval { action_id, .. } => ActionSummary {
            action_id: Some(action_id.clone()),
            stdout_hash: None,
            stderr_hash: None,
            error_kind: Some("pending_approval".to_string()),
            diagnostic: None,
            outcome_label: TurnOutcomeLabel::Unknown,
            close_reason: TurnCloseReason::Open,
        },
    }
}

fn structured_action_diagnostic(error: &str) -> Option<ActionDiagnosticRecord> {
    parse_ui_action_diagnostic(error).or_else(|| parse_browser_action_diagnostic(error))
}

fn parse_ui_action_diagnostic(error: &str) -> Option<ActionDiagnosticRecord> {
    let (failure_kind, body) = bracketed_failure_body(error, "UI failure [")?;
    let detail = marker_segment(body, "Detail:")
        .or_else(|| segment_until_any(body, &["Recovery:", "Next [", "Target:", "Evidence:"]));
    Some(ActionDiagnosticRecord {
        source: ActionDiagnosticSource::UiWindow,
        failure_kind,
        recovery_directive: next_directive(body),
        recovery_hint: marker_segment(body, "Recovery:")
            .map(|value| compact_diagnostic_text(&value, ACTION_DIAGNOSTIC_RECOVERY_CHARS)),
        detail_preview: detail
            .map(|value| compact_diagnostic_text(&value, ACTION_DIAGNOSTIC_DETAIL_CHARS)),
        target_preview: marker_segment(body, "Target:")
            .map(|value| compact_diagnostic_text(&value, ACTION_DIAGNOSTIC_TARGET_CHARS)),
        evidence_preview: marker_segment(body, "Evidence:")
            .map(|value| compact_diagnostic_text(&value, ACTION_DIAGNOSTIC_EVIDENCE_CHARS)),
    })
}

fn parse_browser_action_diagnostic(error: &str) -> Option<ActionDiagnosticRecord> {
    let (failure_kind, body) = bracketed_failure_body(error, "Browser failure [")?;
    Some(ActionDiagnosticRecord {
        source: ActionDiagnosticSource::Browser,
        failure_kind,
        recovery_directive: next_directive(body),
        recovery_hint: marker_segment(body, "Recovery:")
            .map(|value| compact_diagnostic_text(&value, ACTION_DIAGNOSTIC_RECOVERY_CHARS)),
        detail_preview: segment_until_any(body, &["Recovery:", "Next ["])
            .map(|value| compact_diagnostic_text(&value, ACTION_DIAGNOSTIC_DETAIL_CHARS)),
        target_preview: browser_target_preview(body),
        evidence_preview: browser_evidence_preview(body),
    })
}

fn bracketed_failure_body<'a>(error: &'a str, prefix: &str) -> Option<(String, &'a str)> {
    let rest = error.strip_prefix(prefix)?;
    let (kind, after_kind) = rest.split_once(']')?;
    let body = after_kind.trim_start_matches(|c: char| c == ':' || c == '.' || c.is_whitespace());
    Some((kind.trim().to_string(), body))
}

fn next_directive(value: &str) -> Option<String> {
    let (_, rest) = value.split_once("Next [")?;
    let (directive, _) = rest.split_once(']')?;
    let directive = directive.trim();
    if directive.is_empty() {
        None
    } else {
        Some(directive.to_string())
    }
}

fn marker_segment(value: &str, marker: &str) -> Option<String> {
    let (_, rest) = value.split_once(marker)?;
    segment_until_any(
        rest,
        &["Recovery:", "Next [", "Detail:", "Target:", "Evidence:"],
    )
}

fn segment_until_any(value: &str, markers: &[&str]) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let end = markers
        .iter()
        .filter_map(|marker| trimmed.find(marker))
        .filter(|idx| *idx > 0)
        .min()
        .unwrap_or(trimmed.len());
    let segment = trimmed[..end].trim().trim_end_matches('.').trim();
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_string())
    }
}

fn browser_target_preview(body: &str) -> Option<String> {
    let mut parts = Vec::new();
    for field in ["selector", "page_url", "page_title"] {
        if let Some(value) = delimited_field(body, field) {
            parts.push(format!("{field}={value}"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(compact_diagnostic_text(
            &parts.join("; "),
            ACTION_DIAGNOSTIC_TARGET_CHARS,
        ))
    }
}

fn browser_evidence_preview(body: &str) -> Option<String> {
    delimited_field(body, "replan_page_state")
        .or_else(|| delimited_field(body, "error"))
        .map(|value| compact_diagnostic_text(&value, ACTION_DIAGNOSTIC_EVIDENCE_CHARS))
}

fn delimited_field(value: &str, field: &str) -> Option<String> {
    let marker = format!("{field}=");
    let start = value.find(&marker)? + marker.len();
    let rest = &value[start..];
    let end = rest
        .find("; ")
        .or_else(|| rest.find(" Recovery:"))
        .or_else(|| rest.find(" Next ["))
        .unwrap_or(rest.len());
    let field_value = rest[..end].trim().trim_end_matches('.').trim();
    if field_value.is_empty() {
        None
    } else {
        Some(field_value.to_string())
    }
}

fn compact_diagnostic_text(value: &str, max_chars: usize) -> String {
    let redacted = redact_sensitive_fields(value);
    let cleaned = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let mut truncated = cleaned
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

fn redact_sensitive_fields(value: &str) -> String {
    let mut output = String::new();
    let mut rest = value;
    while let Some(idx) = rest.find("text=") {
        output.push_str(&rest[..idx]);
        output.push_str("text=<redacted>");
        let after_marker = &rest[idx + "text=".len()..];
        let skip = sensitive_field_value_len(after_marker);
        rest = &after_marker[skip..];
    }
    output.push_str(rest);
    output
}

fn sensitive_field_value_len(value: &str) -> usize {
    if let Some(remaining) = value.strip_prefix("<redacted>") {
        return value.len() - remaining.len();
    }
    if let Some(stripped) = value.strip_prefix('\'') {
        return stripped
            .find('\'')
            .map(|idx| idx + 2)
            .unwrap_or(value.len());
    }
    if let Some(stripped) = value.strip_prefix('"') {
        return stripped.find('"').map(|idx| idx + 2).unwrap_or(value.len());
    }
    value
        .find(|c: char| c.is_whitespace() || c == ';' || c == '.')
        .unwrap_or(value.len())
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
        TurnCloseReason::GenerationFailed => TurnOutcomeLabel::GenerationFailed,
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
            evidence: Vec::new(),
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
        assert_eq!(record.runtime.process_id, std::process::id());
        assert_eq!(record.runtime.identity.len(), 16);
        assert!(record.evidence.is_empty());
        assert_eq!(
            record.privacy_mode,
            TurnRecordPrivacyMode::RedactedPreviewV1
        );
        assert_eq!(record.close_reason, TurnCloseReason::Open);
        assert!(record.generation.is_none());
    }

    #[test]
    fn start_turn_persists_hashed_evidence_without_raw_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        let mut input = dispatch_input("trace-evidence");
        input.evidence = vec![
            EvidenceRecord::new("screen_capture", "fresh screenshot attached")
                .with_payload_hash("private-image-bytes"),
        ];

        recorder.start_turn(input).unwrap();

        let bytes = fs::read(recorder.record_path_for_trace("trace-evidence")).unwrap();
        let serialized = String::from_utf8(bytes.clone()).unwrap();
        let record: ContextTurnRecord = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record.evidence.len(), 1);
        assert_eq!(record.evidence[0].source, "screen_capture");
        assert_eq!(
            record.evidence[0].payload_hash.as_deref(),
            Some(fingerprint("private-image-bytes").as_str())
        );
        assert!(!serialized.contains("private-image-bytes"));
    }

    #[test]
    fn older_turn_record_without_runtime_field_remains_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder.start_turn(dispatch_input("trace-legacy")).unwrap();
        let path = recorder.record_path_for_trace("trace-legacy");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("runtime");

        let record: ContextTurnRecord = serde_json::from_value(value).unwrap();
        assert_eq!(record.runtime.process_id, std::process::id());
        assert_eq!(record.runtime.identity.len(), 16);
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
    fn attach_evidence_updates_existing_record() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder.start_turn(dispatch_input("trace-source")).unwrap();

        recorder
            .attach_evidence(
                "trace-source",
                EvidenceRecord::new("macos_top_process_sample", "rows=5")
                    .with_payload_hash("canonical rows"),
            )
            .unwrap();

        let record: ContextTurnRecord = serde_json::from_slice(
            &fs::read(recorder.record_path_for_trace("trace-source")).unwrap(),
        )
        .unwrap();
        assert_eq!(record.evidence.len(), 1);
        assert_eq!(record.evidence[0].source, "macos_top_process_sample");
        assert_eq!(
            record.evidence[0].payload_hash,
            Some(fingerprint("canonical rows"))
        );
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
    fn action_result_records_ui_failure_diagnostic_without_raw_typed_text() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder
            .start_turn(dispatch_input("trace-ui-diag"))
            .unwrap();
        let outcome = ActionOutcome::Rejected {
            action_id: "action-ui".to_string(),
            error: "UI failure [not_typeable]: Recovery: Target a visible editable text field or text area. Next [snapshot_then_replan]: Inspect the current UI snapshot before choosing another control. Do not repeat the same label blindly. Detail: ui type failed: matched control is disabled. Target: action=ui_type app=Fixture window='Fixture' role=AXTextField label='Secret field' text='super secret typed value'. Evidence: matched_control: AXTextField | name='Secret field' | enabled=false".to_string(),
        };

        recorder
            .attach_action_result("trace-ui-diag", "ui_type", Some("safe"), &outcome)
            .unwrap();

        let json = fs::read_to_string(recorder.record_path_for_trace("trace-ui-diag")).unwrap();
        assert!(!json.contains("super secret typed value"));
        assert!(json.contains("text=<redacted>"));

        let record: ContextTurnRecord = serde_json::from_str(&json).unwrap();
        let diagnostic = record
            .action
            .expect("action record")
            .diagnostic
            .expect("ui diagnostic");
        assert_eq!(diagnostic.source, ActionDiagnosticSource::UiWindow);
        assert_eq!(diagnostic.failure_kind, "not_typeable");
        assert_eq!(
            diagnostic.recovery_directive.as_deref(),
            Some("snapshot_then_replan")
        );
        assert!(diagnostic
            .target_preview
            .as_deref()
            .is_some_and(|target| target.contains("role=AXTextField")));
        assert!(diagnostic
            .evidence_preview
            .as_deref()
            .is_some_and(|evidence| evidence.contains("enabled=false")));
    }

    #[test]
    fn action_result_records_browser_failure_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut recorder = TurnRecordAggregator::new(tmp.path());
        recorder
            .start_turn(dispatch_input("trace-browser-diag"))
            .unwrap();
        let outcome = ActionOutcome::Rejected {
            action_id: "action-browser".to_string(),
            error: "Browser failure [selector_not_found]: selector=#missing; page_url=file:///tmp/page.html; page_title=Example; error=element not found; replan_page_state=#real-button button \"Real\" Recovery: Inspect or extract the page before retrying with a selector that exists. Next [extract_page_then_replan]: Do not repeat the same selector.".to_string(),
        };

        recorder
            .attach_action_result("trace-browser-diag", "browser", Some("safe"), &outcome)
            .unwrap();

        let record: ContextTurnRecord = serde_json::from_slice(
            &fs::read(recorder.record_path_for_trace("trace-browser-diag")).unwrap(),
        )
        .unwrap();
        let diagnostic = record
            .action
            .expect("action record")
            .diagnostic
            .expect("browser diagnostic");
        assert_eq!(diagnostic.source, ActionDiagnosticSource::Browser);
        assert_eq!(diagnostic.failure_kind, "selector_not_found");
        assert_eq!(
            diagnostic.recovery_directive.as_deref(),
            Some("extract_page_then_replan")
        );
        assert!(diagnostic
            .target_preview
            .as_deref()
            .is_some_and(|target| target.contains("selector=#missing")));
        assert!(diagnostic
            .evidence_preview
            .as_deref()
            .is_some_and(|evidence| evidence.contains("#real-button")));
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
