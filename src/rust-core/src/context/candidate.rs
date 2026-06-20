use serde::{Deserialize, Serialize};

use super::representation::CandidateRepresentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSourceKind {
    FocusedApp,
    Clipboard,
    LastShellCommand,
    ConversationHistory,
    RetrievalMemory,
    ActionResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextInjectionTarget {
    SystemMessage,
    UserTurnPrefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPriority {
    Critical,
    High,
    Normal,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepresentationSelectionPolicy {
    PreferHighestUtilityThatFits,
    PreferBestRoi,
    ForceRaw,
    PreferSummaryUnlessReferenced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRiskClass {
    Public,
    OperatorPrivate,
    Sensitive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    Chat,
    DebugShellFailure,
    UiAction,
    ClipboardReference,
    RetrievalGrounded,
    Humor,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateFeatures {
    pub user_referenced: bool,
    pub fresh: bool,
    pub error_present: bool,
    pub exact_match: bool,
    pub source_weight: f64,
    pub task_affinity: f64,
    pub app_affinity: f64,
    pub recency_boost: f64,
    pub distraction_penalty: f64,
}

impl Default for CandidateFeatures {
    fn default() -> Self {
        Self {
            user_referenced: false,
            fresh: false,
            error_present: false,
            exact_match: false,
            source_weight: 0.0,
            task_affinity: 0.0,
            app_affinity: 0.0,
            recency_boost: 0.0,
            distraction_penalty: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCandidate {
    pub id: String,
    pub source_kind: ContextSourceKind,
    pub injection_target: ContextInjectionTarget,
    pub priority: ContextPriority,
    pub freshness_ms: Option<u64>,
    pub app_bundle_id: Option<String>,
    pub task_class: Option<TaskClass>,
    pub risk_class: ContextRiskClass,
    pub content_fingerprint: String,
    pub features: CandidateFeatures,
    pub representation_policy: RepresentationSelectionPolicy,
    pub representations: Vec<CandidateRepresentation>,
}

impl ContextCandidate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        source_kind: ContextSourceKind,
        injection_target: ContextInjectionTarget,
        priority: ContextPriority,
        risk_class: ContextRiskClass,
        content_fingerprint: impl Into<String>,
        features: CandidateFeatures,
        representations: Vec<CandidateRepresentation>,
    ) -> Self {
        Self {
            id: id.into(),
            source_kind,
            injection_target,
            priority,
            freshness_ms: None,
            app_bundle_id: None,
            task_class: None,
            risk_class,
            content_fingerprint: content_fingerprint.into(),
            features,
            representation_policy: RepresentationSelectionPolicy::PreferBestRoi,
            representations,
        }
    }

    #[allow(dead_code)] // v2 Outcome Ledger will persist freshness as a learned feature.
    pub fn with_freshness_ms(mut self, freshness_ms: Option<u64>) -> Self {
        self.freshness_ms = freshness_ms;
        self
    }

    pub fn with_app_bundle_id(mut self, app_bundle_id: Option<String>) -> Self {
        self.app_bundle_id = app_bundle_id;
        self
    }

    pub fn with_task_class(mut self, task_class: Option<TaskClass>) -> Self {
        self.task_class = task_class;
        self
    }

    pub fn with_representation_policy(mut self, policy: RepresentationSelectionPolicy) -> Self {
        self.representation_policy = policy;
        self
    }
}

pub fn source_default_weight(source: ContextSourceKind) -> f64 {
    match source {
        ContextSourceKind::LastShellCommand => 95.0,
        ContextSourceKind::FocusedApp => 80.0,
        ContextSourceKind::ActionResult => 78.0,
        ContextSourceKind::RetrievalMemory => 72.0,
        ContextSourceKind::Clipboard => 48.0,
        ContextSourceKind::ConversationHistory => 38.0,
    }
}
