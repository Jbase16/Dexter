//! Trusted-subsystem adapters for work-order evidence.
//!
//! Slice A2 deliberately has no live orchestrator caller. These adapters define
//! the narrow facts the later shadow tracker may accept without copying broad
//! command output, clipboard contents, or raw operator corrections into the
//! work-order evidence stream.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

use crate::{context_observer::ContextSnapshot, ipc::proto::HealthResponse};

use super::types::{fingerprint, EvidenceRef, EvidenceSource, SecurityLabel, WorkOrderError};

const BROWSER_EVIDENCE_FIELD_MAX_CHARS: usize = 512;

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum EvidenceAdapterError {
    #[error(transparent)]
    WorkOrder(#[from] WorkOrderError),
    #[error("browser result is not valid worker JSON: {0}")]
    InvalidBrowserResult(String),
    // Slice B will feed corrections into the live work-order journal.
    #[cfg_attr(not(test), allow(dead_code))]
    #[error("operator correction must not be empty")]
    EmptyOperatorCorrection,
}

/// Convert an audited receipt into evidence of the action's recorded outcome.
///
/// `summary` and `description` are intentionally excluded: both can contain
/// operator-controlled targets or bounded output previews. The immutable audit
/// receipt remains authoritative and can be resolved later through `action_id`.
pub(crate) fn from_action_receipt(
    action_id: &str,
    action_type: &str,
    outcome: &str,
    observed_at: DateTime<Utc>,
) -> Result<EvidenceRef, EvidenceAdapterError> {
    EvidenceRef::new(
        EvidenceSource::ActionReceipt,
        format!("{action_id}:status"),
        observed_at,
        json!({
            "action_type": action_type,
            "outcome": outcome,
        })
        .to_string(),
        SecurityLabel::Public,
    )
    .map_err(Into::into)
}

/// Convert a context snapshot without copying clipboard, focused-field values,
/// window titles, or shell command text into evidence.
pub(crate) fn from_context_snapshot(
    snapshot: &ContextSnapshot,
) -> Result<EvidenceRef, EvidenceAdapterError> {
    EvidenceRef::new(
        EvidenceSource::ContextSnapshot,
        format!("context-{:016x}", snapshot.snapshot_hash),
        snapshot.last_updated,
        json!({
            "frontmost_app_bundle_id": snapshot.app_bundle_id,
            "frontmost_app_name": snapshot.app_name,
            "screen_locked": snapshot.is_screen_locked,
        })
        .to_string(),
        SecurityLabel::OperatorPrivate,
    )
    .map_err(Into::into)
}

#[derive(Debug, Default, Deserialize)]
struct BrowserEvidencePayload {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    page_url: Option<String>,
    #[serde(default)]
    page_title: Option<String>,
    #[serde(default)]
    error_kind: Option<String>,
}

/// Convert the existing browser worker JSON result into bounded page evidence.
/// The worker's broad `output` and `error` fields are never copied.
pub(crate) fn from_browser_result(
    action_id: &str,
    payload: &str,
    observed_at: DateTime<Utc>,
) -> Result<[EvidenceRef; 2], EvidenceAdapterError> {
    let payload: BrowserEvidencePayload = serde_json::from_str(payload)
        .map_err(|error| EvidenceAdapterError::InvalidBrowserResult(error.to_string()))?;
    let status = EvidenceRef::new(
        EvidenceSource::BrowserResult,
        format!("{action_id}:status"),
        observed_at,
        json!({
            "success": payload.success,
            "error_kind": clean_browser_field(payload.error_kind.as_deref()),
        })
        .to_string(),
        SecurityLabel::Public,
    )?;
    let page = EvidenceRef::new(
        EvidenceSource::BrowserResult,
        format!("{action_id}:page"),
        observed_at,
        json!({
            "page_url": clean_browser_field(payload.page_url.as_deref()),
            "page_title": clean_browser_field(payload.page_title.as_deref()),
        })
        .to_string(),
        SecurityLabel::OperatorPrivate,
    )?;
    Ok([status, page])
}

/// Convert daemon health into component-state evidence without copying paths,
/// model identifiers, or operator context markdown from the proto response.
pub(crate) fn from_health_snapshot(
    health: &HealthResponse,
    observed_at: DateTime<Utc>,
) -> Result<EvidenceRef, EvidenceAdapterError> {
    EvidenceRef::new(
        EvidenceSource::HealthSnapshot,
        &health.trace_id,
        observed_at,
        json!({
            "status": health.status,
            "stt_worker": health.stt_worker,
            "tts_worker": health.tts_worker,
            "browser_worker": health.browser_worker,
        })
        .to_string(),
        SecurityLabel::Public,
    )
    .map_err(Into::into)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionLifecycleState {
    Starting,
    Ready,
    Ended,
    Failed,
}

impl SessionLifecycleState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }
}

/// Convert the existing session lifecycle boundary into evidence. The caller
/// supplies the session ID already owned by the gRPC session state machine.
pub(crate) fn from_session_event(
    session_id: &str,
    state: SessionLifecycleState,
    observed_at: DateTime<Utc>,
) -> Result<EvidenceRef, EvidenceAdapterError> {
    EvidenceRef::new(
        EvidenceSource::SessionEvent,
        format!("{session_id}:{}", state.as_str()),
        observed_at,
        json!({
            "session_id": session_id,
            "state": state.as_str(),
        })
        .to_string(),
        SecurityLabel::Public,
    )
    .map_err(Into::into)
}

/// Record that the operator disputed prior work without retaining the raw text.
/// Matching and reopening the correct order belong to a later slice.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn from_operator_correction(
    source_turn_id: &str,
    correction_text: &str,
    observed_at: DateTime<Utc>,
) -> Result<EvidenceRef, EvidenceAdapterError> {
    let correction_text = correction_text.trim();
    if correction_text.is_empty() {
        return Err(EvidenceAdapterError::EmptyOperatorCorrection);
    }
    EvidenceRef::new(
        EvidenceSource::OperatorCorrection,
        source_turn_id,
        observed_at,
        json!({
            "correction_fingerprint": fingerprint(correction_text).as_str(),
        })
        .to_string(),
        SecurityLabel::Sensitive,
    )
    .map_err(Into::into)
}

fn clean_browser_field(value: Option<&str>) -> Option<String> {
    let cleaned = value?
        .chars()
        .filter(|character| !character.is_control())
        .take(BROWSER_EVIDENCE_FIELD_MAX_CHARS)
        .collect::<String>();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use crate::context_observer::ContextSnapshot;

    use super::*;

    fn observed_at() -> DateTime<Utc> {
        Utc.timestamp_opt(1_786_000_000, 0).single().unwrap()
    }

    #[test]
    fn action_receipt_adapter_excludes_summary_and_description() {
        let evidence = from_action_receipt("action-1", "shell", "success", observed_at()).unwrap();

        assert_eq!(evidence.source(), EvidenceSource::ActionReceipt);
        assert_eq!(evidence.source_id(), "action-1:status");
        assert!(evidence.fact().contains("shell"));
        assert!(evidence.fact().contains("success"));
        assert!(!evidence.fact().contains("CANARY-CREDENTIAL"));
        assert_eq!(evidence.security_label(), SecurityLabel::Public);
    }

    #[test]
    fn context_adapter_excludes_operator_content() {
        let snapshot = ContextSnapshot {
            app_bundle_id: Some("com.apple.finder".to_string()),
            app_name: Some("Finder".to_string()),
            focused_element: None,
            is_screen_locked: false,
            clipboard_text: Some("CANARY-CREDENTIAL".to_string()),
            clipboard_changed_at: Some(observed_at()),
            visible_windows: Vec::new(),
            last_shell_command: None,
            snapshot_hash: 42,
            last_updated: observed_at(),
        };

        let evidence = from_context_snapshot(&snapshot).unwrap();

        assert_eq!(evidence.source(), EvidenceSource::ContextSnapshot);
        assert!(evidence.fact().contains("com.apple.finder"));
        assert!(!evidence.fact().contains("CANARY-CREDENTIAL"));
        assert_eq!(evidence.security_label(), SecurityLabel::OperatorPrivate);
    }

    #[test]
    fn browser_adapter_keeps_page_metadata_but_drops_broad_output() {
        let payload = json!({
            "success": true,
            "page_url": "https://example.com/",
            "page_title": "Example Domain",
            "output": "CANARY-CREDENTIAL",
        })
        .to_string();

        let [status, page] =
            from_browser_result("action-browser", &payload, observed_at()).unwrap();

        assert_eq!(status.source_id(), "action-browser:status");
        assert_eq!(status.security_label(), SecurityLabel::Public);
        assert!(status.fact().contains("\"success\":true"));
        assert!(!status.fact().contains("Example Domain"));
        assert_eq!(page.source_id(), "action-browser:page");
        assert_eq!(page.security_label(), SecurityLabel::OperatorPrivate);
        assert!(page.fact().contains("Example Domain"));
        assert!(page.fact().contains("https://example.com/"));
        assert!(!page.fact().contains("CANARY-CREDENTIAL"));
    }

    #[test]
    fn health_adapter_keeps_status_but_drops_runtime_paths() {
        let health = HealthResponse {
            trace_id: "health-1".to_string(),
            status: "ready".to_string(),
            stt_worker: "ready".to_string(),
            tts_worker: "ready".to_string(),
            browser_worker: "ready".to_string(),
            config_path: "/private/CANARY-CREDENTIAL".to_string(),
            ..Default::default()
        };

        let evidence = from_health_snapshot(&health, observed_at()).unwrap();

        assert_eq!(evidence.source(), EvidenceSource::HealthSnapshot);
        assert!(evidence.fact().contains("ready"));
        assert!(!evidence.fact().contains("CANARY-CREDENTIAL"));
        assert_eq!(evidence.security_label(), SecurityLabel::Public);
    }

    #[test]
    fn session_adapter_records_typed_lifecycle_state() {
        let evidence =
            from_session_event("session-2", SessionLifecycleState::Ready, observed_at()).unwrap();

        assert_eq!(evidence.source(), EvidenceSource::SessionEvent);
        assert_eq!(evidence.source_id(), "session-2:ready");
        assert!(evidence.fact().contains("session-2"));
        assert!(evidence.fact().contains("ready"));
        assert_eq!(evidence.security_label(), SecurityLabel::Public);
    }

    #[test]
    fn correction_adapter_fingerprints_raw_operator_text() {
        let evidence = from_operator_correction(
            "turn-correction",
            "No, Finder is not frontmost CANARY-CREDENTIAL",
            observed_at(),
        )
        .unwrap();

        assert_eq!(evidence.source(), EvidenceSource::OperatorCorrection);
        assert_eq!(evidence.security_label(), SecurityLabel::Sensitive);
        assert!(!evidence.fact().contains("Finder"));
        assert!(!evidence.fact().contains("CANARY-CREDENTIAL"));
        assert_eq!(evidence.observed_at(), &observed_at());
    }

    #[test]
    fn correction_adapter_rejects_empty_text() {
        assert_eq!(
            from_operator_correction("turn-correction", "  ", observed_at()),
            Err(EvidenceAdapterError::EmptyOperatorCorrection)
        );
    }
}
