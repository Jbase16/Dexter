//! Isolated, best-effort persistence for DEX-03 shadow evidence.
//!
//! Production call sites can only call [`ShadowTracker::observe`], which uses a
//! bounded `try_send` and returns no result. Disk I/O and serialization live on
//! the receiver task, so a full queue, slow disk, closed receiver, or write
//! failure cannot delay or fail an operator-facing turn.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::warn;

use super::types::{
    fingerprint, EvidenceRef, EvidenceSource, ObligationKind, ObligationSource, ObligationStatus,
    SecurityLabel, WorkOrder, WorkOrderKind, WorkOrderScope, WorkOrderStatus,
};

pub(crate) const SHADOW_TRACE_FILENAME: &str = "work_order-shadow.jsonl";
const SHADOW_TRACE_ROTATED_FILENAME: &str = "work_order-shadow.jsonl.1";

// The shadow trace is diagnostic, not an immutable action audit. Two files at
// 4 MiB each retain useful recent history while bounding an always-on daemon to
// approximately 8 MiB plus filesystem metadata.
const SHADOW_TRACE_MAX_BYTES: u64 = 4 * 1024 * 1024;

// Large enough for normal bursts, small enough that a wedged shadow writer
// cannot accumulate meaningful memory pressure in the production process.
const SHADOW_EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug)]
pub(crate) enum ShadowEvent {
    /// Slice A work orders come only from hand-authored fixtures. Slice B will
    /// add validated runtime proposal sources; A3 must not infer obligations.
    #[cfg_attr(not(test), allow(dead_code))]
    WorkOrderSnapshot(WorkOrder),
    /// Live evidence can be recorded before a work order exists without
    /// pretending Slice A derived obligations from arbitrary language.
    EvidenceObserved {
        session_id: String,
        source_turn_id: Option<String>,
        evidence: EvidenceRef,
    },
}

enum ShadowCommand {
    Observe(Box<ShadowEvent>),
    #[cfg(test)]
    Flush(tokio::sync::oneshot::Sender<()>),
}

#[derive(Clone)]
pub(crate) struct ShadowTracker {
    sender: mpsc::Sender<ShadowCommand>,
    dropped_events: Arc<AtomicU64>,
}

impl ShadowTracker {
    pub(crate) fn spawn(state_dir: &Path) -> Self {
        let (sender, receiver) = mpsc::channel(SHADOW_EVENT_CHANNEL_CAPACITY);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let store = ShadowTraceStore::new(state_dir, SHADOW_TRACE_MAX_BYTES);
        let worker_dropped_events = Arc::clone(&dropped_events);
        tokio::spawn(async move {
            run_shadow_worker(receiver, store, worker_dropped_events).await;
        });
        Self {
            sender,
            dropped_events,
        }
    }

    /// Best-effort, non-blocking observation. This deliberately exposes no
    /// future and no error result to production callers.
    pub(crate) fn observe(&self, event: ShadowEvent) {
        if self
            .sender
            .try_send(ShadowCommand::Observe(Box::new(event)))
            .is_err()
        {
            let dropped = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                warn!(
                    dropped_shadow_events = dropped,
                    "Work-order shadow event dropped; production execution is unaffected"
                );
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn dropped_event_count(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) async fn flush_for_test(&self) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.sender
            .send(ShadowCommand::Flush(sender))
            .await
            .expect("shadow worker must remain alive during test");
        receiver
            .await
            .expect("shadow worker must acknowledge flush");
    }
}

async fn run_shadow_worker(
    mut receiver: mpsc::Receiver<ShadowCommand>,
    mut store: ShadowTraceStore,
    dropped_events: Arc<AtomicU64>,
) {
    while let Some(command) = receiver.recv().await {
        match command {
            ShadowCommand::Observe(event) => {
                let record =
                    ShadowTraceRecord::from_event(*event, dropped_events.load(Ordering::Relaxed));
                if let Err(error) = store.append(&record) {
                    warn!(
                        error = %error,
                        path = %store.path.display(),
                        "Failed to persist work-order shadow event; production execution is unaffected"
                    );
                }
            }
            #[cfg(test)]
            ShadowCommand::Flush(acknowledge) => {
                let _ = acknowledge.send(());
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct ShadowTraceRecord {
    schema_version: u8,
    recorded_at: DateTime<Utc>,
    dropped_events_before_record: u64,
    #[serde(flatten)]
    payload: ShadowTracePayload,
}

impl ShadowTraceRecord {
    fn from_event(event: ShadowEvent, dropped_events_before_record: u64) -> Self {
        let payload = match event {
            ShadowEvent::WorkOrderSnapshot(work_order) => ShadowTracePayload::WorkOrderSnapshot {
                work_order: SecretSafeWorkOrder::from_work_order(&work_order),
            },
            ShadowEvent::EvidenceObserved {
                session_id,
                source_turn_id,
                evidence,
            } => ShadowTracePayload::EvidenceObserved {
                session_id_fingerprint: fingerprint(&session_id).as_str().to_string(),
                source_turn_id_fingerprint: source_turn_id
                    .as_deref()
                    .map(|value| fingerprint(value).as_str().to_string()),
                evidence: SecretSafeEvidence::from_evidence(&evidence),
            },
        };
        Self {
            schema_version: 2,
            recorded_at: Utc::now(),
            dropped_events_before_record,
            payload,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum ShadowTracePayload {
    WorkOrderSnapshot {
        work_order: SecretSafeWorkOrder,
    },
    EvidenceObserved {
        session_id_fingerprint: String,
        source_turn_id_fingerprint: Option<String>,
        evidence: SecretSafeEvidence,
    },
}

#[derive(Debug, Serialize)]
struct SecretSafeWorkOrder {
    id: String,
    session_id_fingerprint: String,
    source_turn_id_fingerprint: String,
    source_text_fingerprint: String,
    created_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
    kind: WorkOrderKind,
    goal_fingerprint: String,
    scope: WorkOrderScope,
    status: WorkOrderStatus,
    obligation_source: ObligationSource,
    obligations: Vec<SecretSafeObligation>,
    evidence_journal: Vec<SecretSafeEvidence>,
    attempt_count: usize,
    correction_generation: u32,
}

impl SecretSafeWorkOrder {
    fn from_work_order(work_order: &WorkOrder) -> Self {
        Self {
            id: work_order.id().to_string(),
            session_id_fingerprint: fingerprint(work_order.session_id()).as_str().to_string(),
            source_turn_id_fingerprint: fingerprint(work_order.source_turn_id())
                .as_str()
                .to_string(),
            source_text_fingerprint: work_order.source_text_fingerprint().as_str().to_string(),
            created_at: *work_order.created_at(),
            deadline: *work_order.deadline(),
            kind: work_order.kind(),
            goal_fingerprint: fingerprint(work_order.goal()).as_str().to_string(),
            scope: work_order.scope(),
            status: work_order.status(),
            obligation_source: work_order.obligation_source().clone(),
            obligations: work_order
                .obligations()
                .iter()
                .map(SecretSafeObligation::from_obligation)
                .collect(),
            evidence_journal: work_order
                .evidence_journal()
                .iter()
                .map(SecretSafeEvidence::from_evidence)
                .collect(),
            attempt_count: work_order.attempt_count(),
            correction_generation: work_order.correction_generation(),
        }
    }
}

#[derive(Debug, Serialize)]
struct SecretSafeObligation {
    id: String,
    kind: ObligationKind,
    status: ObligationStatus,
    satisfying_evidence_id: Option<String>,
}

impl SecretSafeObligation {
    fn from_obligation(obligation: &super::types::Obligation) -> Self {
        Self {
            id: obligation.id().to_string(),
            kind: obligation.kind(),
            status: obligation.status(),
            satisfying_evidence_id: obligation
                .satisfying_evidence()
                .map(|id| id.as_str().to_string()),
        }
    }
}

#[derive(Debug, Serialize)]
struct SecretSafeEvidence {
    id: String,
    source: EvidenceSource,
    source_id_fingerprint: String,
    observed_at: DateTime<Utc>,
    fact: PersistedFact,
    security_label: SecurityLabel,
}

impl SecretSafeEvidence {
    fn from_evidence(evidence: &EvidenceRef) -> Self {
        Self {
            id: evidence.id().as_str().to_string(),
            source: evidence.source(),
            source_id_fingerprint: fingerprint(evidence.source_id()).as_str().to_string(),
            observed_at: *evidence.observed_at(),
            fact: PersistedFact::from_evidence(evidence),
            security_label: evidence.security_label(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "projection", rename_all = "snake_case")]
enum PersistedFact {
    Public { value: serde_json::Value },
    OperatorPrivate { structure: serde_json::Value },
    Sensitive { fingerprint: String },
}

impl PersistedFact {
    fn from_evidence(evidence: &EvidenceRef) -> Self {
        match evidence.security_label() {
            SecurityLabel::Public => Self::Public {
                value: parse_fact(evidence.fact()),
            },
            SecurityLabel::OperatorPrivate => Self::OperatorPrivate {
                structure: operator_private_structure(evidence.fact()),
            },
            SecurityLabel::Sensitive => Self::Sensitive {
                fingerprint: fingerprint(evidence.fact()).as_str().to_string(),
            },
        }
    }
}

fn parse_fact(fact: &str) -> serde_json::Value {
    serde_json::from_str(fact).unwrap_or_else(|_| serde_json::Value::String(fact.to_string()))
}

fn operator_private_structure(fact: &str) -> serde_json::Value {
    match serde_json::from_str(fact) {
        Ok(value) => fingerprint_scalar_values(value),
        Err(_) => serde_json::json!({
            "value_fingerprint": fingerprint(fact).as_str()
        }),
    }
}

fn fingerprint_scalar_values(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, fingerprint_scalar_values(value)))
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(fingerprint_scalar_values).collect())
        }
        serde_json::Value::String(value) => {
            serde_json::Value::String(fingerprint(&value).as_str().to_string())
        }
        serde_json::Value::Number(value) => {
            serde_json::Value::String(fingerprint(&value.to_string()).as_str().to_string())
        }
        serde_json::Value::Bool(value) => serde_json::Value::String(
            fingerprint(if value { "true" } else { "false" })
                .as_str()
                .to_string(),
        ),
        serde_json::Value::Null => serde_json::Value::Null,
    }
}

struct ShadowTraceStore {
    path: PathBuf,
    rotated_path: PathBuf,
    max_bytes: u64,
    file: Option<File>,
    current_bytes: u64,
}

impl ShadowTraceStore {
    fn new(state_dir: &Path, max_bytes: u64) -> Self {
        Self {
            path: state_dir.join(SHADOW_TRACE_FILENAME),
            rotated_path: state_dir.join(SHADOW_TRACE_ROTATED_FILENAME),
            max_bytes,
            file: None,
            current_bytes: 0,
        }
    }

    fn append(&mut self, record: &ShadowTraceRecord) -> Result<(), ShadowTraceStoreError> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let line_len = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if line_len > self.max_bytes {
            return Err(ShadowTraceStoreError::RecordTooLarge {
                bytes: line_len,
                max_bytes: self.max_bytes,
            });
        }
        self.ensure_open()?;
        if self.current_bytes.saturating_add(line_len) > self.max_bytes && self.current_bytes > 0 {
            self.file.take();
            fs::rename(&self.path, &self.rotated_path)?;
            self.current_bytes = 0;
            self.open_file()?;
        }
        self.file
            .as_mut()
            .ok_or(ShadowTraceStoreError::FileUnavailable)?
            .write_all(&line)?;
        self.current_bytes = self.current_bytes.saturating_add(line_len);
        Ok(())
    }

    fn ensure_open(&mut self) -> Result<(), ShadowTraceStoreError> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            self.current_bytes = fs::metadata(&self.path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            self.open_file()?;
        }
        Ok(())
    }

    fn open_file(&mut self) -> Result<(), ShadowTraceStoreError> {
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        Ok(())
    }
}

#[derive(Debug, Error)]
enum ShadowTraceStoreError {
    #[error("failed to serialize shadow trace: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("shadow trace I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("shadow trace record is {bytes} bytes, exceeding the {max_bytes}-byte limit")]
    RecordTooLarge { bytes: u64, max_bytes: u64 },
    #[error("shadow trace file handle is unavailable")]
    FileUnavailable,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::work_order::types::{FreshnessRequirement, Obligation, ObligationSource};
    use crate::{
        context_observer::ContextSnapshot, ipc::proto::HealthResponse,
        orchestrator::SharedDaemonState,
    };

    fn test_time() -> DateTime<Utc> {
        Utc.timestamp_opt(1_786_000_000, 0).single().unwrap()
    }

    fn test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dexter-shadow-{label}-{}", uuid::Uuid::new_v4()))
    }

    fn fixture_work_order(marker: &str) -> WorkOrder {
        let obligation = Obligation::new(
            "observe_title",
            "Observe the page title",
            ObligationKind::Observation,
            Vec::new(),
            vec![EvidenceSource::BrowserResult],
            FreshnessRequirement::Any,
        )
        .unwrap();
        WorkOrder::new(
            "work-order-1",
            format!("session-{marker}"),
            format!("turn-{marker}"),
            fingerprint(marker),
            test_time(),
            test_time() + Duration::seconds(30),
            WorkOrderKind::Action,
            format!("Open the private page {marker}"),
            WorkOrderScope::Browser,
            ObligationSource::Fixture {
                fixture_id: "founding-browser-title".to_string(),
            },
            vec![obligation],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn persisted_trace_fingerprints_operator_content() {
        let state_dir = test_dir("redaction");
        let marker = "DEXTER-SHADOW-CANARY fake-password=correct-horse-battery-staple";
        let tracker = ShadowTracker::spawn(&state_dir);

        tracker.observe(ShadowEvent::WorkOrderSnapshot(fixture_work_order(marker)));
        tracker.flush_for_test().await;

        let trace = fs::read_to_string(state_dir.join(SHADOW_TRACE_FILENAME)).unwrap();
        assert!(!trace.contains("DEXTER-SHADOW-CANARY"));
        assert!(!trace.contains("correct-horse-battery-staple"));
        assert!(trace.contains("source_text_fingerprint"));
        assert!(trace.contains("goal_fingerprint"));
        assert!(trace.contains("\"obligation_source\":{\"kind\":\"fixture\""));
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[tokio::test]
    async fn daemon_hooks_persist_trusted_evidence_through_shared_tracker() {
        let state_dir = test_dir("daemon-hooks");
        let mut shared = SharedDaemonState::new_degraded();
        shared.enable_work_order_shadow(&state_dir);

        shared.record_shadow_session_lifecycle(
            "session-live",
            crate::work_order::evidence::SessionLifecycleState::Ready,
        );
        shared.record_shadow_action_receipt("session-live", "action-live", "browser", "success");
        shared.record_operator_context(
            "session-live",
            &ContextSnapshot {
                app_bundle_id: Some("com.apple.finder".to_string()),
                app_name: Some("Finder".to_string()),
                focused_element: None,
                is_screen_locked: false,
                clipboard_text: None,
                clipboard_changed_at: None,
                visible_windows: Vec::new(),
                last_shell_command: None,
                snapshot_hash: 7,
                last_updated: test_time(),
            },
        );
        shared.record_shadow_health(&HealthResponse {
            trace_id: "health-live".to_string(),
            status: "ready".to_string(),
            stt_worker: "ready".to_string(),
            tts_worker: "ready".to_string(),
            browser_worker: "ready".to_string(),
            ..Default::default()
        });
        shared.browser.observe_work_order_shadow_result(
            "action-browser-result",
            &serde_json::json!({
                "success": true,
                "page_url": "https://example.com/",
                "page_title": "Private title DEXTER-BROWSER-CANARY",
                "output": "fake-password=browser-secret",
            })
            .to_string(),
        );
        shared.flush_work_order_shadow_for_test().await;

        let trace = fs::read_to_string(state_dir.join(SHADOW_TRACE_FILENAME)).unwrap();
        let rows = trace
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|row| row["schema_version"] == 2));
        for source in [
            "session_event",
            "action_receipt",
            "context_snapshot",
            "health_snapshot",
            "browser_result",
        ] {
            assert!(rows.iter().any(|row| row["evidence"]["source"] == source));
        }
        let action = rows
            .iter()
            .find(|row| row["evidence"]["source"] == "action_receipt")
            .unwrap();
        assert_eq!(action["evidence"]["fact"]["projection"], "public");
        assert_eq!(action["evidence"]["fact"]["value"]["outcome"], "success");
        let health = rows
            .iter()
            .find(|row| row["evidence"]["source"] == "health_snapshot")
            .unwrap();
        assert_eq!(health["evidence"]["fact"]["value"]["status"], "ready");
        let browser_status = rows
            .iter()
            .find(|row| {
                row["evidence"]["source"] == "browser_result"
                    && row["evidence"]["fact"]["projection"] == "public"
            })
            .unwrap();
        assert_eq!(browser_status["evidence"]["fact"]["value"]["success"], true);
        let browser_page = rows
            .iter()
            .find(|row| {
                row["evidence"]["source"] == "browser_result"
                    && row["evidence"]["fact"]["projection"] == "operator_private"
            })
            .unwrap();
        assert!(browser_page["evidence"]["fact"]["structure"]
            .get("page_title")
            .is_some());
        assert!(!trace.contains("DEXTER-BROWSER-CANARY"));
        assert!(!trace.contains("browser-secret"));
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn observe_drops_immediately_when_bounded_queue_is_full() {
        let (sender, _receiver) = mpsc::channel(1);
        let tracker = ShadowTracker {
            sender,
            dropped_events: Arc::new(AtomicU64::new(0)),
        };

        tracker.observe(ShadowEvent::WorkOrderSnapshot(fixture_work_order("first")));
        tracker.observe(ShadowEvent::WorkOrderSnapshot(fixture_work_order("second")));

        assert_eq!(tracker.dropped_event_count(), 1);
    }

    #[test]
    fn store_rotates_at_configured_bound() {
        let state_dir = test_dir("rotation");
        let mut store = ShadowTraceStore::new(&state_dir, 1_500);
        for _ in 0..12 {
            let record = ShadowTraceRecord::from_event(
                ShadowEvent::WorkOrderSnapshot(fixture_work_order("rotation-marker")),
                0,
            );
            store.append(&record).unwrap();
        }

        assert!(store.path.exists());
        assert!(store.rotated_path.exists());
        assert!(fs::metadata(&store.path).unwrap().len() <= 1_500);
        assert!(fs::metadata(&store.rotated_path).unwrap().len() <= 1_500);
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn sensitive_evidence_trace_fingerprints_fact_and_correlation_ids() {
        let marker = "private-browser-title-CANARY";
        let evidence = EvidenceRef::new(
            EvidenceSource::BrowserResult,
            marker,
            test_time(),
            marker,
            SecurityLabel::Sensitive,
        )
        .unwrap();

        let record = ShadowTraceRecord::from_event(
            ShadowEvent::EvidenceObserved {
                session_id: marker.to_string(),
                source_turn_id: Some(marker.to_string()),
                evidence,
            },
            0,
        );
        let serialized = serde_json::to_string(&record).unwrap();

        assert!(!serialized.contains(marker));
        assert!(serialized.contains("\"projection\":\"sensitive\""));
        assert!(serialized.contains("\"fingerprint\""));
        assert!(serialized.contains("source_id_fingerprint"));
    }

    #[test]
    fn operator_private_projection_preserves_keys_and_fingerprints_values() {
        let marker = "private-title-CANARY";
        let evidence = EvidenceRef::new(
            EvidenceSource::BrowserResult,
            "browser-action",
            test_time(),
            serde_json::json!({
                "success": true,
                "page_title": marker,
                "nested": { "url": "https://private.example/" },
            })
            .to_string(),
            SecurityLabel::OperatorPrivate,
        )
        .unwrap();

        let serialized =
            serde_json::to_value(SecretSafeEvidence::from_evidence(&evidence)).unwrap();

        assert_eq!(serialized["fact"]["projection"], "operator_private");
        assert!(serialized["fact"]["structure"].get("page_title").is_some());
        assert!(serialized["fact"]["structure"]["nested"]
            .get("url")
            .is_some());
        assert!(!serialized.to_string().contains(marker));
        assert!(!serialized.to_string().contains("private.example"));
    }
}
