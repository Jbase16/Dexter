use serde::{Deserialize, Serialize};

use super::{ContextInjectionTarget, ContextSourceKind, RepresentationKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextDecisionReason {
    Included,
    BudgetExceeded,
    LowScore,
    NoRepresentation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilerScope {
    AmbientOnly,
    FullPrompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenCostMethod {
    CharHeuristicV1,
    HuggingFaceTokenizer,
    OllamaReportedPromptEval,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextDecision {
    pub candidate_id: String,
    pub source_kind: ContextSourceKind,
    pub injection_target: ContextInjectionTarget,
    pub representation: Option<RepresentationKind>,
    pub estimated_tokens: usize,
    pub score: f64,
    pub roi: f64,
    pub reason: ContextDecisionReason,
    pub content_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledContextDiagnostics {
    pub compiler_version: String,
    pub scope: CompilerScope,
    pub token_cost_method: TokenCostMethod,
    pub budget_tokens: usize,
    pub reserved_output_tokens: usize,
    pub estimated_used_tokens: usize,
    pub mandatory_tokens: usize,
    pub optional_tokens: usize,
    pub included: Vec<ContextDecision>,
    pub dropped: Vec<ContextDecision>,
}

impl CompiledContextDiagnostics {
    pub fn dropped_count(&self) -> usize {
        self.dropped.len()
    }
}
