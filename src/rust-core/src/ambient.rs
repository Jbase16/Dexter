//! Ambient event engine primitives.
//!
//! This module is the durable backbone for "when X happens, do Y or ask me"
//! behavior. It deliberately starts with deterministic local facts: append-only
//! event records, persisted trigger definitions, and pure trigger matching. Model
//! calls can summarize or draft later, but they do not decide whether an event
//! happened or whether a trigger condition matches.
//!
//! V1 is intentionally shared by the daemon and dexter-cli before every producer
//! and trigger action is wired. Some primitives are daemon-only, some are CLI-only,
//! and some are tested scaffolding for the next ingestion pass.
#![allow(dead_code)]

use std::{
    collections::HashSet,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const AMBIENT_EVENT_SCHEMA_VERSION: &str = "1.0";
pub const AMBIENT_TRIGGER_SCHEMA_VERSION: &str = "1.0";
pub const AMBIENT_ACKNOWLEDGEMENT_SCHEMA_VERSION: &str = "1.0";
pub const AMBIENT_EVENTS_FILENAME: &str = "ambient_events.jsonl";
pub const AMBIENT_TRIGGERS_FILENAME: &str = "ambient_triggers.json";
pub const AMBIENT_ACKNOWLEDGEMENTS_FILENAME: &str = "ambient_acknowledgements.json";
const DEFAULT_ACTION_FAILURE_TRIGGER: &str = "Dexter action failures";
const DEFAULT_DEGRADED_HEALTH_TRIGGER: &str = "Dexter degraded health";
const DEFAULT_COMPONENT_RESTART_FAILURE_TRIGGER: &str = "Dexter component restart failures";

#[derive(Debug)]
pub enum AmbientStoreError {
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidData(String),
}

impl fmt::Display for AmbientStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "ambient store IO error: {error}"),
            Self::Json(error) => write!(f, "ambient store JSON error: {error}"),
            Self::InvalidData(message) => write!(f, "ambient store invalid data: {message}"),
        }
    }
}

impl Error for AmbientStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidData(_) => None,
        }
    }
}

impl From<std::io::Error> for AmbientStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for AmbientStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AmbientSeverity {
    Info,
    Warn,
    Critical,
}

impl AmbientSeverity {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Warn => 1,
            Self::Critical => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AmbientEventStatus {
    New,
    Acknowledged,
    Dismissed,
}

impl AmbientEventStatus {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Acknowledged => "acknowledged",
            Self::Dismissed => "dismissed",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AmbientTriggerAction {
    NotifyOnly,
    AskApproval,
    StartTask,
}

impl AmbientTriggerAction {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotifyOnly => "notify_only",
            Self::AskApproval => "ask_approval",
            Self::StartTask => "start_task",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AmbientEvent {
    pub schema_version: String,
    pub event_id: String,
    pub timestamp: String,
    pub source: String,
    pub kind: String,
    pub severity: AmbientSeverity,
    pub title: String,
    pub summary: String,
    pub status: AmbientEventStatus,
    pub payload: serde_json::Value,
}

impl AmbientEvent {
    pub fn new(
        source: impl Into<String>,
        kind: impl Into<String>,
        severity: AmbientSeverity,
        title: impl Into<String>,
        summary: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            schema_version: AMBIENT_EVENT_SCHEMA_VERSION.to_string(),
            event_id: Uuid::new_v4().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            source: source.into(),
            kind: kind.into(),
            severity,
            title: title.into(),
            summary: summary.into(),
            status: AmbientEventStatus::New,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AmbientTrigger {
    pub schema_version: String,
    pub trigger_id: String,
    pub created_at: String,
    pub enabled: bool,
    pub name: String,
    pub event_kind: Option<String>,
    pub minimum_severity: AmbientSeverity,
    pub action: AmbientTriggerAction,
}

impl AmbientTrigger {
    pub fn new(
        name: impl Into<String>,
        event_kind: Option<String>,
        minimum_severity: AmbientSeverity,
        action: AmbientTriggerAction,
    ) -> Self {
        Self {
            schema_version: AMBIENT_TRIGGER_SCHEMA_VERSION.to_string(),
            trigger_id: Uuid::new_v4().to_string(),
            created_at: Utc::now().to_rfc3339(),
            enabled: true,
            name: name.into(),
            event_kind,
            minimum_severity,
            action,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AmbientAcknowledgements {
    schema_version: String,
    acknowledged_event_ids: Vec<String>,
}

impl AmbientAcknowledgements {
    fn from_ids(ids: HashSet<String>) -> Self {
        let mut acknowledged_event_ids: Vec<String> = ids.into_iter().collect();
        acknowledged_event_ids.sort();
        Self {
            schema_version: AMBIENT_ACKNOWLEDGEMENT_SCHEMA_VERSION.to_string(),
            acknowledged_event_ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbientTriggerMatch {
    pub trigger_id: String,
    pub name: String,
    pub action: AmbientTriggerAction,
}

#[derive(Debug, Clone)]
pub struct AmbientEventStore {
    state_dir: PathBuf,
}

impl AmbientEventStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
        }
    }

    pub fn events_path(&self) -> PathBuf {
        self.state_dir.join(AMBIENT_EVENTS_FILENAME)
    }

    #[allow(dead_code)]
    pub fn triggers_path(&self) -> PathBuf {
        self.state_dir.join(AMBIENT_TRIGGERS_FILENAME)
    }

    #[allow(dead_code)]
    pub fn acknowledgements_path(&self) -> PathBuf {
        self.state_dir.join(AMBIENT_ACKNOWLEDGEMENTS_FILENAME)
    }

    pub fn append_event(&self, event: &AmbientEvent) -> Result<PathBuf, AmbientStoreError> {
        fs::create_dir_all(&self.state_dir)?;
        let path = self.events_path();
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(line.as_bytes())?;
        Ok(path)
    }

    pub fn record_event(
        &self,
        source: impl Into<String>,
        kind: impl Into<String>,
        severity: AmbientSeverity,
        title: impl Into<String>,
        summary: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<AmbientEvent, AmbientStoreError> {
        let event = AmbientEvent::new(source, kind, severity, title, summary, payload);
        self.append_event(&event)?;
        Ok(event)
    }

    pub fn record_event_and_evaluate(
        &self,
        source: impl Into<String>,
        kind: impl Into<String>,
        severity: AmbientSeverity,
        title: impl Into<String>,
        summary: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<(AmbientEvent, Vec<AmbientEvent>), AmbientStoreError> {
        let event = self.record_event(source, kind, severity, title, summary, payload)?;
        if event.kind == "trigger_matched" {
            return Ok((event, Vec::new()));
        }

        let triggers = self.read_triggers()?;
        let matches = evaluate_triggers(&event, &triggers);
        let mut emitted = Vec::with_capacity(matches.len());
        for trigger_match in matches {
            let trigger_event = build_trigger_followup_event(&event, &trigger_match);
            self.append_event(&trigger_event)?;
            emitted.push(trigger_event);
        }

        Ok((event, emitted))
    }

    pub fn recent_events(
        &self,
        limit: usize,
    ) -> Result<(PathBuf, Vec<AmbientEvent>), AmbientStoreError> {
        let (path, mut events) = recent_events(&self.state_dir, limit)?;
        let acknowledged = read_acknowledged_event_ids(&self.state_dir)?;
        apply_acknowledged_status(&mut events, &acknowledged);
        Ok((path, events))
    }

    pub fn unread_trigger_matches(
        &self,
        limit: usize,
    ) -> Result<(PathBuf, Vec<AmbientEvent>), AmbientStoreError> {
        unread_trigger_matches(&self.state_dir, limit)
    }

    pub fn acknowledge_events(
        &self,
        event_ids: &[String],
    ) -> Result<(PathBuf, usize), AmbientStoreError> {
        acknowledge_events(&self.state_dir, event_ids)
    }

    pub fn read_triggers(&self) -> Result<Vec<AmbientTrigger>, AmbientStoreError> {
        read_triggers(&self.state_dir)
    }

    pub fn write_triggers(
        &self,
        triggers: &[AmbientTrigger],
    ) -> Result<PathBuf, AmbientStoreError> {
        write_triggers(&self.state_dir, triggers)
    }

    pub fn ensure_default_triggers(&self) -> Result<Vec<AmbientTrigger>, AmbientStoreError> {
        let mut triggers = self.read_triggers()?;
        let mut installed = Vec::new();
        for default_trigger in default_triggers() {
            if triggers
                .iter()
                .any(|trigger| trigger.name == default_trigger.name)
            {
                continue;
            }
            installed.push(default_trigger.clone());
            triggers.push(default_trigger);
        }

        if !installed.is_empty() {
            self.write_triggers(&triggers)?;
        }

        Ok(installed)
    }
}

fn default_triggers() -> Vec<AmbientTrigger> {
    vec![
        AmbientTrigger::new(
            DEFAULT_ACTION_FAILURE_TRIGGER,
            Some("action_failed".to_string()),
            AmbientSeverity::Warn,
            AmbientTriggerAction::NotifyOnly,
        ),
        AmbientTrigger::new(
            DEFAULT_DEGRADED_HEALTH_TRIGGER,
            Some("health_status_changed".to_string()),
            AmbientSeverity::Critical,
            AmbientTriggerAction::NotifyOnly,
        ),
        AmbientTrigger::new(
            DEFAULT_COMPONENT_RESTART_FAILURE_TRIGGER,
            Some("component_restarted".to_string()),
            AmbientSeverity::Warn,
            AmbientTriggerAction::NotifyOnly,
        ),
    ]
}

pub fn recent_events(
    state_dir: &Path,
    limit: usize,
) -> Result<(PathBuf, Vec<AmbientEvent>), AmbientStoreError> {
    let path = state_dir.join(AMBIENT_EVENTS_FILENAME);
    if limit == 0 || !path.exists() {
        return Ok((path, Vec::new()));
    }

    let content = fs::read_to_string(&path)?;
    let lines: Vec<&str> = content.lines().collect();
    let mut events = Vec::with_capacity(limit.min(lines.len()));
    for (idx, line) in lines.iter().enumerate().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: AmbientEvent = serde_json::from_str(line).map_err(|error| {
            AmbientStoreError::InvalidData(format!(
                "ambient event log line {} is not valid JSON: {error}",
                idx + 1
            ))
        })?;
        events.push(event);
        if events.len() >= limit {
            break;
        }
    }

    Ok((path, events))
}

pub fn unread_trigger_matches(
    state_dir: &Path,
    limit: usize,
) -> Result<(PathBuf, Vec<AmbientEvent>), AmbientStoreError> {
    let path = state_dir.join(AMBIENT_EVENTS_FILENAME);
    if limit == 0 || !path.exists() {
        return Ok((path, Vec::new()));
    }

    let acknowledged = read_acknowledged_event_ids(state_dir)?;
    let content = fs::read_to_string(&path)?;
    let lines: Vec<&str> = content.lines().collect();
    let mut events = Vec::with_capacity(limit.min(lines.len()));
    for (idx, line) in lines.iter().enumerate().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: AmbientEvent = serde_json::from_str(line).map_err(|error| {
            AmbientStoreError::InvalidData(format!(
                "ambient event log line {} is not valid JSON: {error}",
                idx + 1
            ))
        })?;
        if !is_inbox_event_kind(&event.kind) || event.status != AmbientEventStatus::New {
            continue;
        }
        if acknowledged.contains(&event.event_id) {
            continue;
        }
        events.push(event);
        if events.len() >= limit {
            break;
        }
    }

    Ok((path, events))
}

fn is_inbox_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "trigger_matched" | "trigger_action_approval_requested" | "trigger_task_completed"
    )
}

pub fn acknowledge_events(
    state_dir: &Path,
    event_ids: &[String],
) -> Result<(PathBuf, usize), AmbientStoreError> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(AMBIENT_ACKNOWLEDGEMENTS_FILENAME);
    let mut acknowledged = read_acknowledged_event_ids(state_dir)?;
    let before = acknowledged.len();

    for event_id in event_ids {
        let event_id = event_id.trim();
        if !event_id.is_empty() {
            acknowledged.insert(event_id.to_string());
        }
    }

    let added = acknowledged.len().saturating_sub(before);
    if added == 0 && path.exists() {
        return Ok((path, 0));
    }

    write_acknowledged_event_ids(state_dir, acknowledged)?;
    Ok((path, added))
}

fn apply_acknowledged_status(events: &mut [AmbientEvent], acknowledged: &HashSet<String>) {
    for event in events {
        if event.status == AmbientEventStatus::New && acknowledged.contains(&event.event_id) {
            event.status = AmbientEventStatus::Acknowledged;
        }
    }
}

fn read_acknowledged_event_ids(state_dir: &Path) -> Result<HashSet<String>, AmbientStoreError> {
    let path = state_dir.join(AMBIENT_ACKNOWLEDGEMENTS_FILENAME);
    if !path.exists() {
        return Ok(HashSet::new());
    }

    let content = fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(HashSet::new());
    }

    let acknowledgements: AmbientAcknowledgements = serde_json::from_str(&content)?;
    Ok(acknowledgements
        .acknowledged_event_ids
        .into_iter()
        .filter_map(|event_id| {
            let event_id = event_id.trim();
            if event_id.is_empty() {
                None
            } else {
                Some(event_id.to_string())
            }
        })
        .collect())
}

fn write_acknowledged_event_ids(
    state_dir: &Path,
    acknowledged: HashSet<String>,
) -> Result<PathBuf, AmbientStoreError> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(AMBIENT_ACKNOWLEDGEMENTS_FILENAME);
    let tmp_path = state_dir.join(format!(
        ".{}.{}.tmp",
        AMBIENT_ACKNOWLEDGEMENTS_FILENAME,
        Uuid::new_v4()
    ));
    let content = serde_json::to_string_pretty(&AmbientAcknowledgements::from_ids(acknowledged))?;
    fs::write(&tmp_path, content)?;
    fs::rename(&tmp_path, &path)?;
    Ok(path)
}

pub fn read_triggers(state_dir: &Path) -> Result<Vec<AmbientTrigger>, AmbientStoreError> {
    let path = state_dir.join(AMBIENT_TRIGGERS_FILENAME);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&content)?)
}

pub fn write_triggers(
    state_dir: &Path,
    triggers: &[AmbientTrigger],
) -> Result<PathBuf, AmbientStoreError> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(AMBIENT_TRIGGERS_FILENAME);
    let tmp_path = state_dir.join(format!(
        ".{}.{}.tmp",
        AMBIENT_TRIGGERS_FILENAME,
        Uuid::new_v4()
    ));
    let content = serde_json::to_string_pretty(triggers)?;
    fs::write(&tmp_path, content)?;
    fs::rename(&tmp_path, &path)?;
    Ok(path)
}

pub fn evaluate_triggers(
    event: &AmbientEvent,
    triggers: &[AmbientTrigger],
) -> Vec<AmbientTriggerMatch> {
    triggers
        .iter()
        .filter(|trigger| trigger_matches_event(trigger, event))
        .map(|trigger| AmbientTriggerMatch {
            trigger_id: trigger.trigger_id.clone(),
            name: trigger.name.clone(),
            action: trigger.action,
        })
        .collect()
}

fn build_trigger_followup_event(
    event: &AmbientEvent,
    trigger_match: &AmbientTriggerMatch,
) -> AmbientEvent {
    match trigger_match.action {
        AmbientTriggerAction::NotifyOnly => build_trigger_notice_event(event, trigger_match),
        AmbientTriggerAction::AskApproval => {
            build_trigger_approval_request_event(event, trigger_match)
        }
        AmbientTriggerAction::StartTask => build_trigger_task_event(event, trigger_match),
    }
}

fn build_trigger_notice_event(
    event: &AmbientEvent,
    trigger_match: &AmbientTriggerMatch,
) -> AmbientEvent {
    AmbientEvent::new(
        "trigger",
        "trigger_matched",
        event.severity,
        format!("Trigger matched: {}", trigger_match.name),
        format!(
            "Ambient trigger '{}' matched event '{}'.",
            trigger_match.name, event.kind
        ),
        trigger_followup_payload(event, trigger_match, None),
    )
}

fn build_trigger_approval_request_event(
    event: &AmbientEvent,
    trigger_match: &AmbientTriggerMatch,
) -> AmbientEvent {
    AmbientEvent::new(
        "trigger",
        "trigger_action_approval_requested",
        event.severity,
        format!("Trigger needs approval: {}", trigger_match.name),
        format!(
            "Ambient trigger '{}' matched event '{}' and is waiting for operator approval before running its configured action.",
            trigger_match.name, event.kind
        ),
        trigger_followup_payload(event, trigger_match, Some("approval_required")),
    )
}

fn build_trigger_task_event(
    event: &AmbientEvent,
    trigger_match: &AmbientTriggerMatch,
) -> AmbientEvent {
    let task_summary = deterministic_task_summary(event);
    AmbientEvent::new(
        "trigger",
        "trigger_task_completed",
        event.severity,
        format!("Trigger task completed: {}", trigger_match.name),
        task_summary.clone(),
        trigger_followup_payload(event, trigger_match, Some(&task_summary)),
    )
}

fn trigger_followup_payload(
    event: &AmbientEvent,
    trigger_match: &AmbientTriggerMatch,
    task_result: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "trigger_id": trigger_match.trigger_id,
        "trigger_name": trigger_match.name,
        "action": trigger_match.action.as_str(),
        "matched_event_id": event.event_id,
        "matched_event_kind": event.kind,
        "matched_event_title": event.title
    });
    if let Some(task_result) = task_result {
        payload["task_result"] = serde_json::Value::String(task_result.to_string());
    }
    payload
}

fn deterministic_task_summary(event: &AmbientEvent) -> String {
    if event.kind == "action_failed" {
        let description = event
            .payload
            .get("description")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("the latest action");
        let exit_code = event
            .payload
            .get("exit_code")
            .and_then(|value| value.as_i64())
            .map(|code| format!(" Exit code: {code}."))
            .unwrap_or_default();
        return format!(
            "Action failure diagnostic ready for {description}.{exit_code} Review the action receipt or run `make why` before retrying."
        );
    }

    format!(
        "Deterministic follow-up task completed for ambient event '{}'. Review the event details before taking action.",
        event.kind
    )
}

fn trigger_matches_event(trigger: &AmbientTrigger, event: &AmbientEvent) -> bool {
    if !trigger.enabled {
        return false;
    }

    if trigger.minimum_severity.rank() > event.severity.rank() {
        return false;
    }

    match trigger.event_kind.as_deref() {
        Some(kind) => kind == event.kind,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn missing_event_log_returns_empty_history() {
        let temp = TempDir::new().expect("tempdir");
        let (path, events) = recent_events(temp.path(), 5).expect("recent events");
        assert_eq!(path, temp.path().join(AMBIENT_EVENTS_FILENAME));
        assert!(events.is_empty());
    }

    #[test]
    fn append_event_and_read_recent_returns_newest_first() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let first = store
            .record_event(
                "test",
                "health_status_changed",
                AmbientSeverity::Info,
                "Ready",
                "Dexter became ready.",
                json!({"status": "ready"}),
            )
            .expect("first event");
        let second = store
            .record_event(
                "test",
                "component_restarted",
                AmbientSeverity::Warn,
                "Browser restarted",
                "The browser worker was restarted.",
                json!({"component": "browser"}),
            )
            .expect("second event");

        let (_path, events) = store.recent_events(2).expect("recent events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_id, second.event_id);
        assert_eq!(events[1].event_id, first.event_id);
    }

    #[test]
    fn read_recent_zero_limit_does_not_create_file() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let (_path, events) = store.recent_events(0).expect("recent events");
        assert!(events.is_empty());
        assert!(!store.events_path().exists());
    }

    #[test]
    fn invalid_event_log_line_reports_line_number() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(AMBIENT_EVENTS_FILENAME);
        fs::write(&path, "{\"kind\":\"ok\"}\nnot-json\n").expect("write log");

        let error = recent_events(temp.path(), 5).expect_err("invalid log should fail");
        assert!(
            error.to_string().contains("line 2"),
            "error should identify bad line: {error}"
        );
    }

    #[test]
    fn trigger_evaluator_matches_kind_and_minimum_severity() {
        let event = AmbientEvent::new(
            "health",
            "health_status_changed",
            AmbientSeverity::Warn,
            "Health degraded",
            "Primary model is not warm.",
            json!({"component": "primary_model"}),
        );
        let mut matching = AmbientTrigger::new(
            "Tell me about health warnings",
            Some("health_status_changed".to_string()),
            AmbientSeverity::Warn,
            AmbientTriggerAction::NotifyOnly,
        );
        matching.trigger_id = "matching".to_string();
        let wrong_kind = AmbientTrigger::new(
            "Wrong kind",
            Some("component_restarted".to_string()),
            AmbientSeverity::Warn,
            AmbientTriggerAction::NotifyOnly,
        );
        let too_severe = AmbientTrigger::new(
            "Critical only",
            Some("health_status_changed".to_string()),
            AmbientSeverity::Critical,
            AmbientTriggerAction::NotifyOnly,
        );
        let mut disabled = AmbientTrigger::new(
            "Disabled",
            Some("health_status_changed".to_string()),
            AmbientSeverity::Info,
            AmbientTriggerAction::NotifyOnly,
        );
        disabled.enabled = false;

        let matches = evaluate_triggers(&event, &[matching, wrong_kind, too_severe, disabled]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].trigger_id, "matching");
    }

    #[test]
    fn trigger_registry_round_trips_as_json_array() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let trigger = AmbientTrigger::new(
            "Notify on critical events",
            None,
            AmbientSeverity::Critical,
            AmbientTriggerAction::NotifyOnly,
        );

        let path = store
            .write_triggers(std::slice::from_ref(&trigger))
            .expect("write triggers");
        assert_eq!(path, temp.path().join(AMBIENT_TRIGGERS_FILENAME));
        let loaded = store.read_triggers().expect("read triggers");
        assert_eq!(loaded, vec![trigger]);
    }

    #[test]
    fn ensure_default_triggers_installs_once() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());

        let installed = store
            .ensure_default_triggers()
            .expect("install default triggers");
        assert_eq!(installed.len(), 3);
        assert!(installed
            .iter()
            .any(|trigger| trigger.name == DEFAULT_ACTION_FAILURE_TRIGGER));
        assert!(installed
            .iter()
            .any(|trigger| trigger.name == DEFAULT_DEGRADED_HEALTH_TRIGGER));
        assert!(installed
            .iter()
            .any(|trigger| trigger.name == DEFAULT_COMPONENT_RESTART_FAILURE_TRIGGER));

        let second = store
            .ensure_default_triggers()
            .expect("second install should be idempotent");
        assert!(second.is_empty());
        assert_eq!(store.read_triggers().expect("read triggers").len(), 3);
    }

    #[test]
    fn ensure_default_triggers_preserves_existing_named_trigger() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let mut disabled = AmbientTrigger::new(
            DEFAULT_ACTION_FAILURE_TRIGGER,
            Some("action_failed".to_string()),
            AmbientSeverity::Critical,
            AmbientTriggerAction::NotifyOnly,
        );
        disabled.enabled = false;
        store
            .write_triggers(std::slice::from_ref(&disabled))
            .expect("write user trigger");

        let installed = store
            .ensure_default_triggers()
            .expect("install remaining defaults");
        assert_eq!(installed.len(), 2);
        let loaded = store.read_triggers().expect("read triggers");
        let preserved = loaded
            .iter()
            .find(|trigger| trigger.name == DEFAULT_ACTION_FAILURE_TRIGGER)
            .expect("original named trigger should remain");
        assert!(!preserved.enabled);
        assert_eq!(preserved.minimum_severity, AmbientSeverity::Critical);
    }

    #[test]
    fn record_event_and_evaluate_emits_trigger_match_events() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let trigger = AmbientTrigger::new(
            "Notify on health warnings",
            Some("health_status_changed".to_string()),
            AmbientSeverity::Warn,
            AmbientTriggerAction::NotifyOnly,
        );
        store
            .write_triggers(std::slice::from_ref(&trigger))
            .expect("write triggers");

        let (event, emitted) = store
            .record_event_and_evaluate(
                "health",
                "health_status_changed",
                AmbientSeverity::Warn,
                "Health pending",
                "Dexter is warming models.",
                json!({"status": "pending"}),
            )
            .expect("record event");

        assert_eq!(event.kind, "health_status_changed");
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].kind, "trigger_matched");
        assert_eq!(
            emitted[0].payload["matched_event_id"],
            serde_json::Value::String(event.event_id)
        );
    }

    #[test]
    fn ask_approval_trigger_emits_operator_approval_notice() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let trigger = AmbientTrigger::new(
            "Ask before failure follow-up",
            Some("action_failed".to_string()),
            AmbientSeverity::Warn,
            AmbientTriggerAction::AskApproval,
        );
        store
            .write_triggers(std::slice::from_ref(&trigger))
            .expect("write triggers");

        let (event, emitted) = store
            .record_event_and_evaluate(
                "action",
                "action_failed",
                AmbientSeverity::Warn,
                "Action failed",
                "A shell action failed.",
                json!({"description": "Run: false approval-smoke", "exit_code": 1}),
            )
            .expect("record failed action");

        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].kind, "trigger_action_approval_requested");
        assert_eq!(emitted[0].payload["action"], "ask_approval");
        assert_eq!(
            emitted[0].payload["matched_event_id"],
            serde_json::Value::String(event.event_id)
        );
        assert!(emitted[0].summary.contains("waiting for operator approval"));

        let (_path, inbox) = store.unread_trigger_matches(5).expect("read inbox");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, "trigger_action_approval_requested");
    }

    #[test]
    fn start_task_trigger_emits_deterministic_action_failure_diagnostic() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let trigger = AmbientTrigger::new(
            "Diagnose action failures",
            Some("action_failed".to_string()),
            AmbientSeverity::Warn,
            AmbientTriggerAction::StartTask,
        );
        store
            .write_triggers(std::slice::from_ref(&trigger))
            .expect("write triggers");

        let (_event, emitted) = store
            .record_event_and_evaluate(
                "action",
                "action_failed",
                AmbientSeverity::Warn,
                "Action failed",
                "A shell action failed.",
                json!({"description": "Run: false task-smoke", "exit_code": 1}),
            )
            .expect("record failed action");

        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].kind, "trigger_task_completed");
        assert_eq!(emitted[0].payload["action"], "start_task");
        assert!(emitted[0].summary.contains("Run: false task-smoke"));
        assert!(emitted[0].summary.contains("Exit code: 1"));
        assert!(emitted[0].summary.contains("make why"));
        assert_eq!(
            emitted[0].payload["task_result"],
            serde_json::Value::String(emitted[0].summary.clone())
        );

        let (_path, inbox) = store.unread_trigger_matches(5).expect("read inbox");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, "trigger_task_completed");
    }

    #[test]
    fn unread_trigger_matches_excludes_acknowledged_events() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let trigger = AmbientTrigger::new(
            "Notify on action failures",
            Some("action_failed".to_string()),
            AmbientSeverity::Warn,
            AmbientTriggerAction::NotifyOnly,
        );
        store
            .write_triggers(std::slice::from_ref(&trigger))
            .expect("write triggers");

        let (_failed, emitted) = store
            .record_event_and_evaluate(
                "action",
                "action_failed",
                AmbientSeverity::Warn,
                "Action failed",
                "A shell action failed.",
                json!({"action_id": "a1"}),
            )
            .expect("record failed action");
        let trigger_event = emitted
            .first()
            .expect("trigger match should be emitted")
            .clone();

        let (_path, unread) = store.unread_trigger_matches(5).expect("read inbox");
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].event_id, trigger_event.event_id);

        let (_ack_path, added) = store
            .acknowledge_events(std::slice::from_ref(&trigger_event.event_id))
            .expect("acknowledge event");
        assert_eq!(added, 1);

        let (_path, unread_after_ack) = store.unread_trigger_matches(5).expect("read inbox");
        assert!(unread_after_ack.is_empty());

        let (_path, history) = store.recent_events(5).expect("read history");
        let acknowledged = history
            .iter()
            .find(|event| event.event_id == trigger_event.event_id)
            .expect("trigger event should remain in history");
        assert_eq!(acknowledged.status, AmbientEventStatus::Acknowledged);
    }

    #[test]
    fn acknowledge_events_is_idempotent_and_ignores_blank_ids() {
        let temp = TempDir::new().expect("tempdir");
        let store = AmbientEventStore::new(temp.path());
        let event_id = "event-1".to_string();

        let (_path, first_added) = store
            .acknowledge_events(&[event_id.clone(), "  ".to_string(), event_id.clone()])
            .expect("first acknowledge");
        assert_eq!(first_added, 1);

        let (_path, second_added) = store
            .acknowledge_events(std::slice::from_ref(&event_id))
            .expect("second acknowledge");
        assert_eq!(second_added, 0);
    }
}
