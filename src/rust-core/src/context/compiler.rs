use super::{
    candidate::{
        source_default_weight, ContextCandidate, ContextPriority, ContextRiskClass,
        ContextSourceKind, RepresentationSelectionPolicy,
    },
    diagnostics::{
        CompiledContextDiagnostics, CompilerScope, ContextDecision, ContextDecisionReason,
        TokenCostMethod,
    },
    CandidateRepresentation, ContextInjectionTarget, RepresentationKind,
};

#[derive(Debug, Clone)]
pub struct ContextCompilerConfig {
    pub budget_tokens: usize,
    pub reserved_output_tokens: usize,
    pub compiler_version: &'static str,
}

impl Default for ContextCompilerConfig {
    fn default() -> Self {
        Self {
            budget_tokens: crate::constants::CONTEXT_COMPILER_BUDGET_TOKENS,
            reserved_output_tokens: crate::constants::CONTEXT_COMPILER_RESERVED_OUTPUT_TOKENS,
            compiler_version: crate::constants::CONTEXT_COMPILER_VERSION,
        }
    }
}

#[derive(Debug, Clone)]
struct CandidateScore {
    candidate: ContextCandidate,
    base_score: f64,
    best_roi: f64,
}

#[derive(Debug, Clone)]
struct SelectedRepresentation {
    representation: CandidateRepresentation,
    score: f64,
    roi: f64,
}

#[derive(Debug, Clone)]
pub struct PackedCandidate {
    pub source_kind: ContextSourceKind,
    pub injection_target: ContextInjectionTarget,
    pub payload: String,
}

#[derive(Debug, Clone)]
pub struct CompiledContext {
    pub packed_candidates: Vec<PackedCandidate>,
    pub diagnostics: CompiledContextDiagnostics,
}

impl CompiledContext {
    pub fn system_messages(&self) -> Vec<String> {
        self.packed_candidates
            .iter()
            .filter(|c| c.injection_target == ContextInjectionTarget::SystemMessage)
            .map(|c| c.payload.clone())
            .collect()
    }

    pub fn user_prefix(&self) -> Option<String> {
        let mut blocks = self
            .packed_candidates
            .iter()
            .filter(|c| c.injection_target == ContextInjectionTarget::UserTurnPrefix)
            .collect::<Vec<_>>();

        blocks.sort_by_key(|c| user_prefix_order(c.source_kind));
        let prefix = blocks
            .into_iter()
            .map(|c| c.payload.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        if prefix.is_empty() {
            None
        } else {
            Some(prefix)
        }
    }
}

pub struct ContextCompiler {
    config: ContextCompilerConfig,
}

impl ContextCompiler {
    pub fn new(config: ContextCompilerConfig) -> Self {
        Self { config }
    }

    pub fn compile(&self, candidates: Vec<ContextCandidate>) -> CompiledContext {
        let mut candidate_scores: Vec<CandidateScore> = Vec::new();
        let mut dropped: Vec<ContextDecision> = Vec::new();

        for candidate in candidates {
            if candidate.representations.is_empty() {
                dropped.push(decision(
                    &candidate,
                    None,
                    0.0,
                    0.0,
                    ContextDecisionReason::NoRepresentation,
                ));
                continue;
            }

            let base_score = candidate_base_score(&candidate);
            if base_score <= 0.0 {
                let cheapest = candidate
                    .representations
                    .iter()
                    .min_by_key(|r| r.estimated_tokens);
                dropped.push(decision(
                    &candidate,
                    cheapest,
                    base_score,
                    0.0,
                    ContextDecisionReason::LowScore,
                ));
                continue;
            }

            let best_roi = candidate
                .representations
                .iter()
                .map(|r| representation_score(base_score, r) / r.estimated_tokens.max(1) as f64)
                .fold(0.0, f64::max);
            candidate_scores.push(CandidateScore {
                candidate,
                base_score,
                best_roi,
            });
        }

        let mut remaining = self
            .config
            .budget_tokens
            .saturating_sub(self.config.reserved_output_tokens);
        let mut used = 0usize;
        let mut included: Vec<ContextDecision> = Vec::new();
        let mut packed_candidates: Vec<PackedCandidate> = Vec::new();

        let (critical, optional): (Vec<_>, Vec<_>) = candidate_scores
            .into_iter()
            .partition(|s| s.candidate.priority == ContextPriority::Critical);

        for item in sort_critical(critical) {
            pack_one(
                item,
                &mut remaining,
                &mut used,
                &mut included,
                &mut dropped,
                &mut packed_candidates,
            );
        }

        for item in sort_optional(optional) {
            pack_one(
                item,
                &mut remaining,
                &mut used,
                &mut included,
                &mut dropped,
                &mut packed_candidates,
            );
        }

        let diagnostics = CompiledContextDiagnostics {
            compiler_version: self.config.compiler_version.to_string(),
            scope: CompilerScope::AmbientOnly,
            token_cost_method: TokenCostMethod::CharHeuristicV1,
            budget_tokens: self.config.budget_tokens,
            reserved_output_tokens: self.config.reserved_output_tokens,
            estimated_used_tokens: used,
            mandatory_tokens: 0,
            optional_tokens: used,
            included,
            dropped,
        };

        CompiledContext {
            packed_candidates,
            diagnostics,
        }
    }
}

fn pack_one(
    item: CandidateScore,
    remaining: &mut usize,
    used: &mut usize,
    included: &mut Vec<ContextDecision>,
    dropped: &mut Vec<ContextDecision>,
    packed_candidates: &mut Vec<PackedCandidate>,
) {
    match select_representation(&item.candidate, item.base_score, *remaining) {
        Some(selected) => {
            *remaining = remaining.saturating_sub(selected.representation.estimated_tokens);
            *used += selected.representation.estimated_tokens;
            included.push(decision(
                &item.candidate,
                Some(&selected.representation),
                selected.score,
                selected.roi,
                ContextDecisionReason::Included,
            ));
            packed_candidates.push(PackedCandidate {
                source_kind: item.candidate.source_kind,
                injection_target: item.candidate.injection_target,
                payload: selected.representation.payload,
            });
        }
        None => {
            let preferred = preferred_representation_for_diagnostics(&item.candidate);
            dropped.push(decision(
                &item.candidate,
                preferred,
                item.base_score,
                0.0,
                ContextDecisionReason::BudgetExceeded,
            ));
        }
    }
}

fn select_representation(
    candidate: &ContextCandidate,
    base_score: f64,
    remaining: usize,
) -> Option<SelectedRepresentation> {
    let fitting = candidate
        .representations
        .iter()
        .filter(|r| r.estimated_tokens <= remaining)
        .collect::<Vec<_>>();

    if fitting.is_empty() {
        return None;
    }

    let selected = match candidate.representation_policy {
        RepresentationSelectionPolicy::ForceRaw => fitting
            .iter()
            .copied()
            .find(|r| r.kind == RepresentationKind::Raw)
            .or_else(|| highest_utility_that_fits(&fitting)),
        RepresentationSelectionPolicy::PreferHighestUtilityThatFits => {
            highest_utility_that_fits(&fitting)
        }
        RepresentationSelectionPolicy::PreferBestRoi => best_roi_that_fits(&fitting, base_score),
        RepresentationSelectionPolicy::PreferSummaryUnlessReferenced => {
            if candidate.features.user_referenced {
                fitting
                    .iter()
                    .copied()
                    .find(|r| r.kind == RepresentationKind::Raw)
                    .or_else(|| highest_utility_that_fits(&fitting))
            } else {
                fitting
                    .iter()
                    .copied()
                    .find(|r| r.kind == RepresentationKind::Summary)
                    .or_else(|| best_roi_that_fits(&fitting, base_score))
            }
        }
    }?;

    let score = representation_score(base_score, selected);
    let roi = score / selected.estimated_tokens.max(1) as f64;
    Some(SelectedRepresentation {
        representation: selected.clone(),
        score,
        roi,
    })
}

fn highest_utility_that_fits<'a>(
    representations: &[&'a CandidateRepresentation],
) -> Option<&'a CandidateRepresentation> {
    representations.iter().copied().max_by(|a, b| {
        a.utility_multiplier
            .partial_cmp(&b.utility_multiplier)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.estimated_tokens.cmp(&a.estimated_tokens))
            .then_with(|| representation_rank(a.kind).cmp(&representation_rank(b.kind)))
    })
}

fn best_roi_that_fits<'a>(
    representations: &[&'a CandidateRepresentation],
    base_score: f64,
) -> Option<&'a CandidateRepresentation> {
    representations.iter().copied().max_by(|a, b| {
        let a_roi = representation_score(base_score, a) / a.estimated_tokens.max(1) as f64;
        let b_roi = representation_score(base_score, b) / b.estimated_tokens.max(1) as f64;
        a_roi
            .partial_cmp(&b_roi)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| representation_rank(a.kind).cmp(&representation_rank(b.kind)))
    })
}

fn preferred_representation_for_diagnostics(
    candidate: &ContextCandidate,
) -> Option<&CandidateRepresentation> {
    candidate
        .representations
        .iter()
        .max_by_key(|r| representation_rank(r.kind))
}

fn sort_critical(mut items: Vec<CandidateScore>) -> Vec<CandidateScore> {
    items.sort_by(|a, b| {
        b.base_score
            .partial_cmp(&a.base_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                source_rank(a.candidate.source_kind).cmp(&source_rank(b.candidate.source_kind))
            })
            .then_with(|| a.candidate.id.cmp(&b.candidate.id))
    });
    items
}

fn sort_optional(mut items: Vec<CandidateScore>) -> Vec<CandidateScore> {
    items.sort_by(|a, b| {
        b.best_roi
            .partial_cmp(&a.best_roi)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                priority_rank(b.candidate.priority).cmp(&priority_rank(a.candidate.priority))
            })
            .then_with(|| {
                source_rank(a.candidate.source_kind).cmp(&source_rank(b.candidate.source_kind))
            })
            .then_with(|| a.candidate.id.cmp(&b.candidate.id))
    });
    items
}

fn candidate_base_score(candidate: &ContextCandidate) -> f64 {
    let mut score = candidate.features.source_weight;
    if score <= 0.0 {
        score = source_default_weight(candidate.source_kind);
    }
    score += candidate.features.task_affinity;
    score += candidate.features.app_affinity;
    score += candidate.features.recency_boost;

    if candidate.features.user_referenced {
        score += 70.0;
    }
    if candidate.features.fresh {
        score += 20.0;
    }
    if candidate.features.error_present {
        score += 45.0;
    }
    if candidate.features.exact_match {
        score += 35.0;
    }

    score -= candidate.features.distraction_penalty;
    score -= match candidate.risk_class {
        ContextRiskClass::Public => 0.0,
        ContextRiskClass::OperatorPrivate => 4.0,
        ContextRiskClass::Sensitive => 40.0,
    };

    score
}

fn representation_score(base_score: f64, representation: &CandidateRepresentation) -> f64 {
    base_score * representation.utility_multiplier
}

fn decision(
    candidate: &ContextCandidate,
    representation: Option<&CandidateRepresentation>,
    score: f64,
    roi: f64,
    reason: ContextDecisionReason,
) -> ContextDecision {
    ContextDecision {
        candidate_id: candidate.id.clone(),
        source_kind: candidate.source_kind,
        injection_target: candidate.injection_target,
        representation: representation.map(|r| r.kind),
        estimated_tokens: representation.map(|r| r.estimated_tokens).unwrap_or(0),
        score,
        roi,
        reason,
        content_fingerprint: candidate.content_fingerprint.clone(),
    }
}

fn priority_rank(priority: ContextPriority) -> u8 {
    match priority {
        ContextPriority::Critical => 4,
        ContextPriority::High => 3,
        ContextPriority::Normal => 2,
        ContextPriority::Low => 1,
    }
}

fn representation_rank(kind: RepresentationKind) -> u8 {
    match kind {
        RepresentationKind::Raw => 8,
        RepresentationKind::CommandStatus => 7,
        RepresentationKind::ErrorOnly => 6,
        RepresentationKind::Diff => 5,
        RepresentationKind::KeyValue => 4,
        RepresentationKind::Summary => 3,
        RepresentationKind::CapabilityOnly => 2,
        RepresentationKind::MetadataOnly => 1,
        RepresentationKind::FingerprintOnly => 0,
    }
}

fn source_rank(source: ContextSourceKind) -> u8 {
    match source {
        ContextSourceKind::LastShellCommand => 0,
        ContextSourceKind::Clipboard => 1,
        ContextSourceKind::FocusedApp => 2,
        ContextSourceKind::ActionResult => 3,
        ContextSourceKind::RetrievalMemory => 4,
        ContextSourceKind::ConversationHistory => 5,
    }
}

fn user_prefix_order(source: ContextSourceKind) -> u8 {
    match source {
        ContextSourceKind::Clipboard => 0,
        ContextSourceKind::LastShellCommand => 1,
        ContextSourceKind::ActionResult => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{
        candidate::{
            CandidateFeatures, ContextCandidate, ContextInjectionTarget, ContextPriority,
            ContextRiskClass, ContextSourceKind, RepresentationSelectionPolicy,
        },
        representation::{CandidateRepresentation, RepresentationKind},
    };

    fn candidate(
        id: &str,
        source: ContextSourceKind,
        payload: &str,
        score: f64,
    ) -> ContextCandidate {
        ContextCandidate::new(
            id,
            source,
            ContextInjectionTarget::SystemMessage,
            ContextPriority::Normal,
            ContextRiskClass::Public,
            id,
            CandidateFeatures {
                source_weight: score,
                ..CandidateFeatures::default()
            },
            vec![CandidateRepresentation::new(
                RepresentationKind::Raw,
                payload,
                1.0,
            )],
        )
    }

    #[test]
    fn compiler_packs_high_roi_candidates_first() {
        let compiler = ContextCompiler::new(ContextCompilerConfig {
            budget_tokens: 80,
            reserved_output_tokens: 0,
            compiler_version: "test",
        });
        let big = "x".repeat(400);
        let compiled = compiler.compile(vec![
            candidate("large", ContextSourceKind::Clipboard, &big, 100.0),
            candidate("small", ContextSourceKind::LastShellCommand, "exit 1", 50.0),
        ]);

        assert_eq!(compiled.diagnostics.included[0].candidate_id, "small");
        assert!(compiled
            .diagnostics
            .dropped
            .iter()
            .any(|d| d.candidate_id == "large"));
    }

    #[test]
    fn compiler_skips_oversized_candidate_and_continues() {
        let compiler = ContextCompiler::new(ContextCompilerConfig {
            budget_tokens: 40,
            reserved_output_tokens: 0,
            compiler_version: "test",
        });
        let huge = "x".repeat(400);
        let compiled = compiler.compile(vec![
            candidate("huge", ContextSourceKind::Clipboard, &huge, 500.0),
            candidate("tiny", ContextSourceKind::FocusedApp, "Terminal", 80.0),
        ]);

        assert!(compiled
            .diagnostics
            .included
            .iter()
            .any(|d| d.candidate_id == "tiny"));
    }

    #[test]
    fn critical_candidate_packs_before_tiny_normal_candidate() {
        let compiler = ContextCompiler::new(ContextCompilerConfig {
            budget_tokens: 120,
            reserved_output_tokens: 0,
            compiler_version: "test",
        });
        let critical = ContextCandidate::new(
            "critical",
            ContextSourceKind::Clipboard,
            ContextInjectionTarget::SystemMessage,
            ContextPriority::Critical,
            ContextRiskClass::Public,
            "critical",
            CandidateFeatures {
                source_weight: 40.0,
                ..CandidateFeatures::default()
            },
            vec![CandidateRepresentation::new(
                RepresentationKind::Raw,
                "x".repeat(220),
                1.0,
            )],
        );
        let tiny = candidate("tiny", ContextSourceKind::FocusedApp, "A", 200.0);
        let compiled = compiler.compile(vec![tiny, critical]);

        assert_eq!(compiled.diagnostics.included[0].candidate_id, "critical");
    }

    #[test]
    fn force_raw_uses_raw_when_budget_allows() {
        let compiler = ContextCompiler::new(ContextCompilerConfig {
            budget_tokens: 300,
            reserved_output_tokens: 0,
            compiler_version: "test",
        });
        let candidate = ContextCandidate::new(
            "clipboard",
            ContextSourceKind::Clipboard,
            ContextInjectionTarget::UserTurnPrefix,
            ContextPriority::High,
            ContextRiskClass::OperatorPrivate,
            "clipboard",
            CandidateFeatures {
                source_weight: 100.0,
                user_referenced: true,
                ..CandidateFeatures::default()
            },
            vec![
                CandidateRepresentation::new(RepresentationKind::Summary, "summary", 0.72),
                CandidateRepresentation::new(RepresentationKind::Raw, "raw payload", 1.0),
            ],
        )
        .with_representation_policy(RepresentationSelectionPolicy::ForceRaw);

        let compiled = compiler.compile(vec![candidate]);
        assert_eq!(
            compiled.diagnostics.included[0].representation,
            Some(RepresentationKind::Raw)
        );
    }

    #[test]
    fn diagnostics_mark_ambient_scope_and_char_heuristic() {
        let compiler = ContextCompiler::new(ContextCompilerConfig {
            budget_tokens: 40,
            reserved_output_tokens: 0,
            compiler_version: "test",
        });
        let compiled = compiler.compile(vec![]);
        assert_eq!(compiled.diagnostics.scope, CompilerScope::AmbientOnly);
        assert_eq!(
            compiled.diagnostics.token_cost_method,
            TokenCostMethod::CharHeuristicV1
        );
    }

    #[test]
    fn diagnostics_do_not_include_raw_payload() {
        let compiler = ContextCompiler::new(ContextCompilerConfig {
            budget_tokens: 80,
            reserved_output_tokens: 0,
            compiler_version: "test",
        });
        let compiled = compiler.compile(vec![candidate(
            "secret_clipboard",
            ContextSourceKind::Clipboard,
            "PRIVATE_SECRET_CONTEXT_PAYLOAD",
            100.0,
        )]);

        let json = serde_json::to_string(&compiled.diagnostics).unwrap();
        assert!(json.contains("secret_clipboard"));
        assert!(!json.contains("PRIVATE_SECRET_CONTEXT_PAYLOAD"));
    }
}
