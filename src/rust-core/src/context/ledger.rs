use serde::{Deserialize, Serialize};

use super::{ContextSourceKind, TaskClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)] // Passive v2 scaffold; live compiler v1 only emits diagnostics.
#[serde(rename_all = "snake_case")]
pub enum TurnOutcomeLabel {
    Unknown,
    Answered,
    ActionExecutedSuccessfully,
    ActionRejectedByPolicy,
    AppleScriptCompileFailed,
    ShellCommandFailed,
    UiElementNotFound,
    UserCancelled,
    UserRetriedSameIntent,
    RepairTurnRequired,
    OperatorManualOverride,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)] // Passive v2 scaffold; populated after turn records land.
pub struct ContextOutcomeLedger {
    pub source_kind: ContextSourceKind,
    pub task_class: TaskClass,
    pub app_bundle_id: Option<String>,
    pub include_count: u64,
    pub drop_count: u64,
    pub success_when_included: u64,
    pub failure_when_included: u64,
    pub success_when_dropped: u64,
    pub failure_when_dropped: u64,
    pub mean_prompt_eval_ms_when_included: f64,
    pub mean_token_cost: f64,
    pub learned_value: f64,
    pub confidence: f64,
}

impl ContextOutcomeLedger {
    #[allow(dead_code)] // Passive v2 scaffold; used when ledger persistence activates.
    pub fn cold_start(source_kind: ContextSourceKind, task_class: TaskClass) -> Self {
        Self {
            source_kind,
            task_class,
            app_bundle_id: None,
            include_count: 0,
            drop_count: 0,
            success_when_included: 0,
            failure_when_included: 0,
            success_when_dropped: 0,
            failure_when_dropped: 0,
            mean_prompt_eval_ms_when_included: 0.0,
            mean_token_cost: 0.0,
            learned_value: 0.0,
            confidence: 0.0,
        }
    }
}
