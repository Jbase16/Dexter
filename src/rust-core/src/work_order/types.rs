use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkOrderKind {
    Question,
    Action,
    Lifecycle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkOrderScope {
    LocalUi,
    Browser,
    Filesystem,
    Process,
    External,
    Internal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkOrderStatus {
    Proposed,
    Active,
    AwaitingApproval,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

// Slice B will use terminal-state checks from the live work-order owner.
#[cfg_attr(not(test), allow(dead_code))]
impl WorkOrderStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

/// Identifies who supplied the required obligations.
///
/// Slice A deliberately supports fixtures only. Live language-to-obligation
/// derivation belongs to Slice B and must add a distinguishable source variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ObligationSource {
    Fixture { fixture_id: String },
}

// Live obligation construction begins in Slice B; Slice A uses fixtures.
#[cfg_attr(not(test), allow(dead_code))]
impl ObligationSource {
    pub(crate) fn fixture(fixture_id: impl Into<String>) -> Result<Self, WorkOrderError> {
        let fixture_id = fixture_id.into();
        require_non_empty("fixture_id", &fixture_id)?;
        Ok(Self::Fixture { fixture_id })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObligationKind {
    Effect,
    Observation,
    RequestedOutput,
    OperatorDelivery,
    LifecycleTransition,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObligationStatus {
    Pending,
    Satisfied,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceSource {
    ActionReceipt,
    ContextSnapshot,
    BrowserResult,
    HealthSnapshot,
    SessionEvent,
    OperatorCorrection,
}

impl EvidenceSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::ActionReceipt => "action_receipt",
            Self::ContextSnapshot => "context_snapshot",
            Self::BrowserResult => "browser_result",
            Self::HealthSnapshot => "health_snapshot",
            Self::SessionEvent => "session_event",
            Self::OperatorCorrection => "operator_correction",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum FreshnessRequirement {
    Any,
    ObservedAfter(DateTime<Utc>),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SecurityLabel {
    Public,
    OperatorPrivate,
    Sensitive,
}

/// A BLAKE3 digest whose provenance is guaranteed by [`fingerprint`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub(crate) struct Fingerprint(String);

impl Fingerprint {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn fingerprint(value: &str) -> Fingerprint {
    Fingerprint(crate::context::representation::fingerprint(value))
}

/// Stable identity for one immutable evidence-journal entry.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub(crate) struct EvidenceId(Fingerprint);

impl EvidenceId {
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Immutable reference to a fact produced by an existing trusted subsystem.
///
/// Slice A keeps the in-memory fact needed by later evidence evaluation. A3's
/// persistence boundary must reuse Dexter's existing redaction/fingerprinting
/// path and must never serialize this structure directly as a shadow trace.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct EvidenceRef {
    id: EvidenceId,
    source: EvidenceSource,
    source_id: String,
    observed_at: DateTime<Utc>,
    fact: String,
    security_label: SecurityLabel,
}

impl EvidenceRef {
    pub(crate) fn new(
        source: EvidenceSource,
        source_id: impl Into<String>,
        observed_at: DateTime<Utc>,
        fact: impl Into<String>,
        security_label: SecurityLabel,
    ) -> Result<Self, WorkOrderError> {
        let source_id = source_id.into();
        let fact = fact.into();
        require_non_empty("evidence.source_id", &source_id)?;
        require_non_empty("evidence.fact", &fact)?;
        let identity = format!(
            "{}\u{0}{}\u{0}{}",
            source.as_str(),
            source_id,
            observed_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        );
        Ok(Self {
            id: EvidenceId(fingerprint(&identity)),
            source,
            source_id,
            observed_at,
            fact,
            security_label,
        })
    }

    pub(crate) fn id(&self) -> &EvidenceId {
        &self.id
    }

    pub(crate) fn source(&self) -> EvidenceSource {
        self.source
    }

    pub(crate) fn source_id(&self) -> &str {
        &self.source_id
    }

    pub(crate) fn observed_at(&self) -> &DateTime<Utc> {
        &self.observed_at
    }

    pub(crate) fn fact(&self) -> &str {
        &self.fact
    }

    pub(crate) fn security_label(&self) -> SecurityLabel {
        self.security_label
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn meets(&self, requirement: &FreshnessRequirement) -> bool {
        match requirement {
            FreshnessRequirement::Any => true,
            FreshnessRequirement::ObservedAfter(threshold) => self.observed_at > *threshold,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn has_same_journal_identity(&self, other: &Self) -> bool {
        self.source == other.source
            && self.source_id == other.source_id
            && self.observed_at == other.observed_at
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct Obligation {
    id: String,
    description: String,
    kind: ObligationKind,
    dependencies: Vec<String>,
    acceptable_evidence: Vec<EvidenceSource>,
    freshness_requirement: FreshnessRequirement,
    status: ObligationStatus,
    satisfying_evidence: Option<EvidenceId>,
}

impl Obligation {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        kind: ObligationKind,
        dependencies: Vec<String>,
        acceptable_evidence: Vec<EvidenceSource>,
        freshness_requirement: FreshnessRequirement,
    ) -> Result<Self, WorkOrderError> {
        let id = id.into();
        let description = description.into();
        require_non_empty("obligation.id", &id)?;
        require_non_empty("obligation.description", &description)?;
        if acceptable_evidence.is_empty() {
            return Err(WorkOrderError::NoAcceptableEvidence(id));
        }
        Ok(Self {
            id,
            description,
            kind,
            dependencies,
            acceptable_evidence,
            freshness_requirement,
            status: ObligationStatus::Pending,
            satisfying_evidence: None,
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn kind(&self) -> ObligationKind {
        self.kind
    }

    pub(crate) fn status(&self) -> ObligationStatus {
        self.status
    }

    pub(crate) fn satisfying_evidence(&self) -> Option<&EvidenceId> {
        self.satisfying_evidence.as_ref()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn select_satisfying_evidence(
        &mut self,
        evidence_id: &EvidenceId,
        evidence: &EvidenceRef,
    ) -> Result<(), WorkOrderError> {
        if !matches!(
            self.status,
            ObligationStatus::Pending | ObligationStatus::Satisfied
        ) {
            return Err(WorkOrderError::ObligationNotPending {
                obligation_id: self.id.clone(),
                status: self.status,
            });
        }
        if !self.acceptable_evidence.contains(&evidence.source()) {
            return Err(WorkOrderError::UnacceptableEvidence {
                obligation_id: self.id.clone(),
                evidence_source: evidence.source(),
            });
        }
        if !evidence.meets(&self.freshness_requirement) {
            return Err(WorkOrderError::StaleEvidence(self.id.clone()));
        }
        self.satisfying_evidence = Some(evidence_id.clone());
        self.status = ObligationStatus::Satisfied;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttemptStatus {
    Proposed,
    AwaitingApproval,
    Executed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct Attempt {
    id: String,
    obligation_id: String,
    target: String,
    action_fingerprint: Fingerprint,
    automatic: bool,
    correction_generation: u32,
    status: AttemptStatus,
    proposed_at: DateTime<Utc>,
    dispatched_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    receipt_evidence: Option<EvidenceId>,
}

// Live attempt callers arrive with the Slice B execution loop.
#[cfg_attr(not(test), allow(dead_code))]
impl Attempt {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: impl Into<String>,
        obligation_id: impl Into<String>,
        target: impl Into<String>,
        action_fingerprint: Fingerprint,
        automatic: bool,
        status: AttemptStatus,
        proposed_at: DateTime<Utc>,
        dispatched_at: Option<DateTime<Utc>>,
        completed_at: Option<DateTime<Utc>>,
        receipt_evidence: Option<EvidenceId>,
    ) -> Result<Self, WorkOrderError> {
        let id = id.into();
        let obligation_id = obligation_id.into();
        let target = target.into();
        require_non_empty("attempt.id", &id)?;
        require_non_empty("attempt.obligation_id", &obligation_id)?;
        require_non_empty("attempt.target", &target)?;
        validate_attempt_timing(
            status,
            proposed_at,
            dispatched_at.as_ref(),
            completed_at.as_ref(),
            receipt_evidence.as_ref(),
        )?;
        Ok(Self {
            id,
            obligation_id,
            target,
            action_fingerprint,
            automatic,
            correction_generation: 0,
            status,
            proposed_at,
            dispatched_at,
            completed_at,
            receipt_evidence,
        })
    }

    pub(crate) fn dispatched_at(&self) -> Option<&DateTime<Utc>> {
        self.dispatched_at.as_ref()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WorkOrder {
    id: String,
    session_id: String,
    source_turn_id: String,
    source_text_fingerprint: Fingerprint,
    created_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
    kind: WorkOrderKind,
    goal: String,
    scope: WorkOrderScope,
    status: WorkOrderStatus,
    obligation_source: ObligationSource,
    obligations: Vec<Obligation>,
    evidence_journal: Vec<EvidenceRef>,
    attempts: Vec<Attempt>,
    correction_generation: u32,
    final_delivery_evidence: Option<EvidenceId>,
}

// Slice B will make this lifecycle live; Slice A constructs and proves it in fixtures.
#[cfg_attr(not(test), allow(dead_code))]
impl WorkOrder {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: impl Into<String>,
        session_id: impl Into<String>,
        source_turn_id: impl Into<String>,
        source_text_fingerprint: Fingerprint,
        created_at: DateTime<Utc>,
        deadline: DateTime<Utc>,
        kind: WorkOrderKind,
        goal: impl Into<String>,
        scope: WorkOrderScope,
        obligation_source: ObligationSource,
        obligations: Vec<Obligation>,
    ) -> Result<Self, WorkOrderError> {
        let id = id.into();
        let session_id = session_id.into();
        let source_turn_id = source_turn_id.into();
        let goal = goal.into();
        require_non_empty("work_order.id", &id)?;
        require_non_empty("work_order.session_id", &session_id)?;
        require_non_empty("work_order.source_turn_id", &source_turn_id)?;
        require_non_empty("work_order.goal", &goal)?;
        if deadline <= created_at {
            return Err(WorkOrderError::InvalidDeadline);
        }
        if obligations.is_empty() {
            return Err(WorkOrderError::NoObligations);
        }
        let mut obligation_ids = HashSet::new();
        for obligation in &obligations {
            if !obligation_ids.insert(obligation.id().to_string()) {
                return Err(WorkOrderError::DuplicateObligationId(
                    obligation.id().to_string(),
                ));
            }
        }
        for obligation in &obligations {
            for dependency in &obligation.dependencies {
                if !obligation_ids.contains(dependency) {
                    return Err(WorkOrderError::UnknownDependency {
                        obligation_id: obligation.id().to_string(),
                        dependency_id: dependency.clone(),
                    });
                }
            }
        }

        Ok(Self {
            id,
            session_id,
            source_turn_id,
            source_text_fingerprint,
            created_at,
            deadline,
            kind,
            goal,
            scope,
            status: WorkOrderStatus::Proposed,
            obligation_source,
            obligations,
            evidence_journal: Vec::new(),
            attempts: Vec::new(),
            correction_generation: 0,
            final_delivery_evidence: None,
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn source_turn_id(&self) -> &str {
        &self.source_turn_id
    }

    pub(crate) fn source_text_fingerprint(&self) -> &Fingerprint {
        &self.source_text_fingerprint
    }

    pub(crate) fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    pub(crate) fn deadline(&self) -> &DateTime<Utc> {
        &self.deadline
    }

    pub(crate) fn kind(&self) -> WorkOrderKind {
        self.kind
    }

    pub(crate) fn goal(&self) -> &str {
        &self.goal
    }

    pub(crate) fn scope(&self) -> WorkOrderScope {
        self.scope
    }

    pub(crate) fn status(&self) -> WorkOrderStatus {
        self.status
    }

    pub(crate) fn obligation_source(&self) -> &ObligationSource {
        &self.obligation_source
    }

    pub(crate) fn obligations(&self) -> &[Obligation] {
        &self.obligations
    }

    pub(crate) fn evidence_journal(&self) -> &[EvidenceRef] {
        &self.evidence_journal
    }

    pub(crate) fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    pub(crate) fn correction_generation(&self) -> u32 {
        self.correction_generation
    }

    pub(crate) fn activate(&mut self) -> Result<(), WorkOrderError> {
        self.transition_to(WorkOrderStatus::Active)
    }

    // Slice B will drive the approval lifecycle through the live work-order loop.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn await_approval(&mut self) -> Result<(), WorkOrderError> {
        self.transition_to(WorkOrderStatus::AwaitingApproval)
    }

    pub(crate) fn begin_verification(&mut self) -> Result<(), WorkOrderError> {
        self.transition_to(WorkOrderStatus::Verifying)
    }

    pub(crate) fn resume(&mut self) -> Result<(), WorkOrderError> {
        self.transition_to(WorkOrderStatus::Active)
    }

    pub(crate) fn fail(&mut self) -> Result<(), WorkOrderError> {
        self.transition_to(WorkOrderStatus::Failed)
    }

    // Slice B will route live operator cancellation through these terminal states.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn cancel(&mut self) -> Result<(), WorkOrderError> {
        self.transition_to(WorkOrderStatus::Cancelled)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn time_out(&mut self) -> Result<(), WorkOrderError> {
        self.transition_to(WorkOrderStatus::TimedOut)
    }

    pub(crate) fn record_evidence(
        &mut self,
        evidence: EvidenceRef,
    ) -> Result<EvidenceId, WorkOrderError> {
        if self.status.is_terminal() {
            return Err(WorkOrderError::TerminalWorkOrder(self.status));
        }
        if self
            .evidence_journal
            .iter()
            .any(|existing| existing.has_same_journal_identity(&evidence))
        {
            return Err(WorkOrderError::DuplicateEvidence {
                evidence_source: evidence.source(),
                source_id: evidence.source_id().to_string(),
                observed_at: *evidence.observed_at(),
            });
        }
        let is_correction = evidence.source() == EvidenceSource::OperatorCorrection;
        let evidence_id = evidence.id().clone();
        self.evidence_journal.push(evidence);
        if is_correction {
            self.correction_generation = self.correction_generation.saturating_add(1);
        }
        Ok(evidence_id)
    }

    pub(crate) fn satisfy_obligation(
        &mut self,
        obligation_id: &str,
        evidence_id: &EvidenceId,
    ) -> Result<(), WorkOrderError> {
        if !matches!(
            self.status,
            WorkOrderStatus::Active | WorkOrderStatus::Verifying
        ) {
            return Err(WorkOrderError::InvalidEvidenceSelectionState(self.status));
        }
        let obligation_index = self
            .obligations
            .iter()
            .position(|obligation| obligation.id() == obligation_id)
            .ok_or_else(|| WorkOrderError::UnknownObligation(obligation_id.to_string()))?;
        let dependencies = self.obligations[obligation_index].dependencies.clone();
        let unsatisfied_dependencies = dependencies
            .iter()
            .filter(|dependency| {
                !self.obligations.iter().any(|candidate| {
                    candidate.id() == dependency.as_str()
                        && candidate.status() == ObligationStatus::Satisfied
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if !unsatisfied_dependencies.is_empty() {
            return Err(WorkOrderError::UnsatisfiedDependencies {
                obligation_id: obligation_id.to_string(),
                dependency_ids: unsatisfied_dependencies,
            });
        }
        let evidence = self
            .evidence_journal
            .iter()
            .find(|evidence| evidence.id() == evidence_id)
            .ok_or_else(|| WorkOrderError::UnknownEvidence(evidence_id.clone()))?;
        self.obligations[obligation_index].select_satisfying_evidence(evidence_id, evidence)
    }

    pub(crate) fn record_attempt(&mut self, mut attempt: Attempt) -> Result<(), WorkOrderError> {
        if !self
            .obligations
            .iter()
            .any(|obligation| obligation.id() == attempt.obligation_id)
        {
            return Err(WorkOrderError::UnknownAttemptObligation(
                attempt.obligation_id.clone(),
            ));
        }
        if self
            .attempts
            .iter()
            .any(|existing| existing.id == attempt.id)
        {
            return Err(WorkOrderError::DuplicateAttemptId(attempt.id));
        }
        if let Some(receipt_id) = attempt.receipt_evidence.as_ref() {
            if !self
                .evidence_journal
                .iter()
                .any(|evidence| evidence.id() == receipt_id)
            {
                return Err(WorkOrderError::UnknownEvidence(receipt_id.clone()));
            }
        }
        if attempt.automatic
            && self
                .attempts
                .iter()
                .filter(|existing| {
                    existing.automatic
                        && existing.obligation_id == attempt.obligation_id
                        && existing.target == attempt.target
                })
                .count()
                >= 2
        {
            return Err(WorkOrderError::AutomaticAttemptLimit {
                obligation_id: attempt.obligation_id,
                target: attempt.target,
            });
        }
        if self.attempts.iter().any(|existing| {
            existing.status == AttemptStatus::Failed
                && existing.correction_generation == self.correction_generation
                && existing.action_fingerprint == attempt.action_fingerprint
        }) {
            return Err(WorkOrderError::RepeatedFailedActionFingerprint(
                attempt.action_fingerprint,
            ));
        }
        attempt.correction_generation = self.correction_generation;
        self.attempts.push(attempt);
        Ok(())
    }

    pub(crate) fn pending_obligation_ids(&self) -> Vec<&str> {
        self.obligations
            .iter()
            .filter(|obligation| obligation.status() == ObligationStatus::Pending)
            .map(Obligation::id)
            .collect()
    }

    /// Build an opaque proof that this exact work-order generation is complete.
    /// Callers cannot construct this proof directly.
    pub(crate) fn completion_proof(
        &self,
        final_delivery_evidence_id: &EvidenceId,
    ) -> Result<CompletionProof, WorkOrderError> {
        if !matches!(
            self.status,
            WorkOrderStatus::Active | WorkOrderStatus::Verifying
        ) {
            return Err(WorkOrderError::InvalidCompletionState(self.status));
        }
        let unsatisfied = self
            .obligations
            .iter()
            .filter(|obligation| obligation.status() != ObligationStatus::Satisfied)
            .map(|obligation| obligation.id().to_string())
            .collect::<Vec<_>>();
        if !unsatisfied.is_empty() {
            return Err(WorkOrderError::UnsatisfiedObligations(unsatisfied));
        }
        let has_selected_delivery = self
            .obligations
            .iter()
            .filter(|obligation| obligation.kind() == ObligationKind::OperatorDelivery)
            .any(|obligation| obligation.satisfying_evidence() == Some(final_delivery_evidence_id));
        if !has_selected_delivery {
            return Err(WorkOrderError::FinalDeliveryEvidenceMissing(
                final_delivery_evidence_id.clone(),
            ));
        }

        Ok(CompletionProof {
            work_order_id: self.id.clone(),
            correction_generation: self.correction_generation,
            final_delivery_evidence: final_delivery_evidence_id.clone(),
        })
    }

    /// Consume this order and an opaque completion proof to construct success.
    pub(crate) fn succeed(
        mut self,
        proof: CompletionProof,
    ) -> Result<SucceededWorkOrder, SuccessTransitionError> {
        let validation = if proof.work_order_id != self.id {
            Err(WorkOrderError::CompletionProofMismatch)
        } else if proof.correction_generation != self.correction_generation
            || self
                .obligations
                .iter()
                .any(|obligation| obligation.status() != ObligationStatus::Satisfied)
        {
            Err(WorkOrderError::CompletionProofStale)
        } else if !matches!(
            self.status,
            WorkOrderStatus::Active | WorkOrderStatus::Verifying
        ) {
            Err(WorkOrderError::InvalidCompletionState(self.status))
        } else {
            Ok(())
        };

        if let Err(error) = validation {
            return Err(SuccessTransitionError {
                work_order: Box::new(self),
                error,
            });
        }
        self.status = WorkOrderStatus::Succeeded;
        self.final_delivery_evidence = Some(proof.final_delivery_evidence);
        Ok(SucceededWorkOrder { work_order: self })
    }

    fn transition_to(&mut self, next: WorkOrderStatus) -> Result<(), WorkOrderError> {
        let allowed = matches!(
            (self.status, next),
            (WorkOrderStatus::Proposed, WorkOrderStatus::Active)
                | (WorkOrderStatus::Proposed, WorkOrderStatus::Cancelled)
                | (WorkOrderStatus::Proposed, WorkOrderStatus::TimedOut)
                | (WorkOrderStatus::Proposed, WorkOrderStatus::Failed)
                | (WorkOrderStatus::Active, WorkOrderStatus::AwaitingApproval)
                | (WorkOrderStatus::Active, WorkOrderStatus::Verifying)
                | (WorkOrderStatus::Active, WorkOrderStatus::Failed)
                | (WorkOrderStatus::Active, WorkOrderStatus::Cancelled)
                | (WorkOrderStatus::Active, WorkOrderStatus::TimedOut)
                | (WorkOrderStatus::AwaitingApproval, WorkOrderStatus::Active)
                | (WorkOrderStatus::AwaitingApproval, WorkOrderStatus::Failed)
                | (
                    WorkOrderStatus::AwaitingApproval,
                    WorkOrderStatus::Cancelled
                )
                | (WorkOrderStatus::AwaitingApproval, WorkOrderStatus::TimedOut)
                | (WorkOrderStatus::Verifying, WorkOrderStatus::Active)
                | (WorkOrderStatus::Verifying, WorkOrderStatus::Failed)
                | (WorkOrderStatus::Verifying, WorkOrderStatus::Cancelled)
                | (WorkOrderStatus::Verifying, WorkOrderStatus::TimedOut)
        );
        if !allowed {
            return Err(WorkOrderError::InvalidTransition {
                from: self.status,
                to: next,
            });
        }
        self.status = next;
        Ok(())
    }
}

/// A successfully completed order. Its inner status cannot be constructed by
/// callers because both the field and constructor are private to this module.
/// It intentionally cannot be deserialized around the completion-proof gate.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SucceededWorkOrder {
    work_order: WorkOrder,
}

#[cfg_attr(not(test), allow(dead_code))]
impl SucceededWorkOrder {
    pub(crate) fn work_order(&self) -> &WorkOrder {
        &self.work_order
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct CompletionProof {
    work_order_id: String,
    correction_generation: u32,
    final_delivery_evidence: EvidenceId,
}

#[derive(Debug)]
// Live recovery callers arrive with the Slice B completion surface.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SuccessTransitionError {
    work_order: Box<WorkOrder>,
    error: WorkOrderError,
}

impl SuccessTransitionError {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn error(&self) -> &WorkOrderError {
        &self.error
    }

    // Slice B recovery retains the order when a completion proof goes stale.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn into_work_order(self) -> WorkOrder {
        *self.work_order
    }
}

// Slice A exercises the complete error surface through fixtures; Slice B will
// construct these variants from the live entry and execution paths.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum WorkOrderError {
    #[error("{0} must not be empty")]
    EmptyField(&'static str),
    #[error("work-order deadline must be after its creation time")]
    InvalidDeadline,
    #[error("work order must contain at least one obligation")]
    NoObligations,
    #[error("obligation {0} must declare at least one acceptable evidence source")]
    NoAcceptableEvidence(String),
    #[error("duplicate obligation id: {0}")]
    DuplicateObligationId(String),
    #[error("obligation {obligation_id} references unknown dependency {dependency_id}")]
    UnknownDependency {
        obligation_id: String,
        dependency_id: String,
    },
    #[error("unknown obligation: {0}")]
    UnknownObligation(String),
    #[error("unknown evidence journal entry: {0:?}")]
    UnknownEvidence(EvidenceId),
    #[error("evidence can be selected only while active or verifying, not {0:?}")]
    InvalidEvidenceSelectionState(WorkOrderStatus),
    #[error("obligation {obligation_id} is {status:?}, not pending")]
    ObligationNotPending {
        obligation_id: String,
        status: ObligationStatus,
    },
    #[error("obligation {obligation_id} does not accept {evidence_source:?} evidence")]
    UnacceptableEvidence {
        obligation_id: String,
        evidence_source: EvidenceSource,
    },
    #[error("evidence for obligation {0} did not meet its freshness requirement")]
    StaleEvidence(String),
    #[error(
        "duplicate evidence journal entry: source={evidence_source:?} source_id={source_id} observed_at={observed_at}"
    )]
    DuplicateEvidence {
        evidence_source: EvidenceSource,
        source_id: String,
        observed_at: DateTime<Utc>,
    },
    #[error("obligation {obligation_id} has unsatisfied dependencies: {dependency_ids:?}")]
    UnsatisfiedDependencies {
        obligation_id: String,
        dependency_ids: Vec<String>,
    },
    #[error("invalid work-order transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: WorkOrderStatus,
        to: WorkOrderStatus,
    },
    #[error("work order is terminal: {0:?}")]
    TerminalWorkOrder(WorkOrderStatus),
    #[error("work order cannot complete from {0:?}")]
    InvalidCompletionState(WorkOrderStatus),
    #[error("work order still has unsatisfied obligations: {0:?}")]
    UnsatisfiedObligations(Vec<String>),
    #[error("operator-delivery evidence was not found: {0:?}")]
    FinalDeliveryEvidenceMissing(EvidenceId),
    #[error("completion proof belongs to a different work order")]
    CompletionProofMismatch,
    #[error("completion proof no longer matches the work order")]
    CompletionProofStale,
    #[error("attempt timestamps are inconsistent with {0:?} state")]
    InvalidAttemptLifecycle(AttemptStatus),
    #[error("attempt timestamp {later} is earlier than {earlier}")]
    InvalidAttemptTimestamp {
        earlier: DateTime<Utc>,
        later: DateTime<Utc>,
    },
    #[error("attempt references unknown obligation: {0}")]
    UnknownAttemptObligation(String),
    #[error("duplicate attempt id: {0}")]
    DuplicateAttemptId(String),
    #[error("automatic attempt limit reached for obligation {obligation_id} target {target}")]
    AutomaticAttemptLimit {
        obligation_id: String,
        target: String,
    },
    #[error("failed action fingerprint cannot repeat in the same correction generation: {0:?}")]
    RepeatedFailedActionFingerprint(Fingerprint),
}

fn validate_attempt_timing(
    status: AttemptStatus,
    proposed_at: DateTime<Utc>,
    dispatched_at: Option<&DateTime<Utc>>,
    completed_at: Option<&DateTime<Utc>>,
    receipt_evidence: Option<&EvidenceId>,
) -> Result<(), WorkOrderError> {
    if let Some(dispatched_at) = dispatched_at {
        if *dispatched_at < proposed_at {
            return Err(WorkOrderError::InvalidAttemptTimestamp {
                earlier: proposed_at,
                later: *dispatched_at,
            });
        }
    }
    if let Some(completed_at) = completed_at {
        let lower_bound = dispatched_at.copied().unwrap_or(proposed_at);
        if *completed_at < lower_bound {
            return Err(WorkOrderError::InvalidAttemptTimestamp {
                earlier: lower_bound,
                later: *completed_at,
            });
        }
    }
    let valid_shape = match status {
        AttemptStatus::Proposed | AttemptStatus::AwaitingApproval => {
            dispatched_at.is_none() && completed_at.is_none() && receipt_evidence.is_none()
        }
        AttemptStatus::Executed | AttemptStatus::Failed => {
            dispatched_at.is_some() && completed_at.is_some() && receipt_evidence.is_some()
        }
        AttemptStatus::Cancelled => completed_at.is_some(),
    };
    if !valid_shape {
        return Err(WorkOrderError::InvalidAttemptLifecycle(status));
    }
    Ok(())
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), WorkOrderError> {
    if value.trim().is_empty() {
        return Err(WorkOrderError::EmptyField(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn obligation(
        id: &str,
        kind: ObligationKind,
        dependencies: &[&str],
        source: EvidenceSource,
    ) -> Obligation {
        Obligation::new(
            id,
            id.replace('_', " "),
            kind,
            dependencies
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            vec![source],
            FreshnessRequirement::Any,
        )
        .expect("valid fixture obligation")
    }

    fn evidence_at(
        source: EvidenceSource,
        source_id: &str,
        observed_at: DateTime<Utc>,
    ) -> EvidenceRef {
        EvidenceRef::new(
            source,
            source_id,
            observed_at,
            format!("fixture fact {source_id}"),
            SecurityLabel::OperatorPrivate,
        )
        .expect("valid fixture evidence")
    }

    fn evidence(source: EvidenceSource, source_id: &str) -> EvidenceRef {
        evidence_at(source, source_id, Utc::now())
    }

    fn record_and_satisfy(
        order: &mut WorkOrder,
        obligation_id: &str,
        evidence: EvidenceRef,
    ) -> EvidenceId {
        let evidence_id = order.record_evidence(evidence).expect("record evidence");
        order
            .satisfy_obligation(obligation_id, &evidence_id)
            .expect("select evidence");
        evidence_id
    }

    fn browser_title_order() -> WorkOrder {
        let now = Utc::now();
        WorkOrder::new(
            "order-1",
            "session-1",
            "turn-1",
            fingerprint("fixture source text"),
            now,
            now + Duration::seconds(15),
            WorkOrderKind::Action,
            "Open the requested page and deliver its title",
            WorkOrderScope::Browser,
            ObligationSource::fixture("founding-browser-title").expect("valid obligation source"),
            vec![
                obligation(
                    "navigate",
                    ObligationKind::Effect,
                    &[],
                    EvidenceSource::ActionReceipt,
                ),
                obligation(
                    "verify_url",
                    ObligationKind::Observation,
                    &["navigate"],
                    EvidenceSource::BrowserResult,
                ),
                obligation(
                    "observe_title",
                    ObligationKind::RequestedOutput,
                    &["verify_url"],
                    EvidenceSource::BrowserResult,
                ),
                obligation(
                    "deliver_title",
                    ObligationKind::OperatorDelivery,
                    &["observe_title"],
                    EvidenceSource::SessionEvent,
                ),
            ],
        )
        .expect("valid fixture work order")
    }

    #[test]
    fn new_order_starts_proposed_and_carries_fixture_source() {
        let order = browser_title_order();
        assert_eq!(order.status(), WorkOrderStatus::Proposed);
        assert_eq!(order.id(), "order-1");
        assert!(matches!(
            order.obligation_source,
            ObligationSource::Fixture { ref fixture_id }
                if fixture_id == "founding-browser-title"
        ));
    }

    #[test]
    fn invalid_transition_is_rejected_without_changing_state() {
        let mut order = browser_title_order();
        let error = order.begin_verification().expect_err("must reject skip");
        assert_eq!(
            error,
            WorkOrderError::InvalidTransition {
                from: WorkOrderStatus::Proposed,
                to: WorkOrderStatus::Verifying,
            }
        );
        assert_eq!(order.status(), WorkOrderStatus::Proposed);
    }

    #[test]
    fn proposed_order_can_fail_directly() {
        let mut order = browser_title_order();
        order.fail().expect("proposal failure is terminal");
        assert_eq!(order.status(), WorkOrderStatus::Failed);
    }

    #[test]
    fn approval_cancel_and_timeout_lifecycle_transitions_are_explicit() {
        let mut approval = browser_title_order();
        approval.activate().unwrap();
        approval.await_approval().unwrap();
        assert_eq!(approval.status(), WorkOrderStatus::AwaitingApproval);
        approval.resume().unwrap();
        assert_eq!(approval.status(), WorkOrderStatus::Active);

        let mut cancelled = browser_title_order();
        cancelled.cancel().unwrap();
        assert_eq!(cancelled.status(), WorkOrderStatus::Cancelled);

        let mut timed_out = browser_title_order();
        timed_out.time_out().unwrap();
        assert_eq!(timed_out.status(), WorkOrderStatus::TimedOut);
    }

    #[test]
    fn journal_records_evidence_before_activation_but_selection_waits_for_active_state() {
        let mut order = browser_title_order();
        let evidence_id = order
            .record_evidence(evidence(EvidenceSource::ActionReceipt, "early-receipt"))
            .expect("journal accepts early trusted evidence");
        assert_eq!(order.evidence_journal().len(), 1);
        assert_eq!(
            order
                .satisfy_obligation("navigate", &evidence_id)
                .expect_err("proposal cannot select evidence"),
            WorkOrderError::InvalidEvidenceSelectionState(WorkOrderStatus::Proposed)
        );
        order.activate().unwrap();
        order.satisfy_obligation("navigate", &evidence_id).unwrap();
    }

    #[test]
    fn journal_rejects_duplicate_identity() {
        let mut order = browser_title_order();
        let observed_at = Utc::now();
        order
            .record_evidence(evidence_at(
                EvidenceSource::BrowserResult,
                "browser-duplicate",
                observed_at,
            ))
            .unwrap();
        assert!(matches!(
            order.record_evidence(evidence_at(
                EvidenceSource::BrowserResult,
                "browser-duplicate",
                observed_at,
            )),
            Err(WorkOrderError::DuplicateEvidence { .. })
        ));
    }

    #[test]
    fn newer_evidence_repoints_selection_without_rewriting_history() {
        let mut order = browser_title_order();
        order.activate().unwrap();
        let first = record_and_satisfy(
            &mut order,
            "navigate",
            evidence(EvidenceSource::ActionReceipt, "receipt-first"),
        );
        let second = order
            .record_evidence(evidence(EvidenceSource::ActionReceipt, "receipt-second"))
            .unwrap();
        order.satisfy_obligation("navigate", &second).unwrap();
        assert_ne!(first, second);
        assert_eq!(order.evidence_journal().len(), 2);
        assert_eq!(order.obligations()[0].satisfying_evidence(), Some(&second));
    }

    #[test]
    fn dependency_and_evidence_rules_gate_satisfaction() {
        let mut order = browser_title_order();
        order.activate().expect("activate");

        let dependency_evidence = order
            .record_evidence(evidence(EvidenceSource::BrowserResult, "browser-1"))
            .unwrap();
        let dependency_error = order
            .satisfy_obligation("verify_url", &dependency_evidence)
            .expect_err("navigation dependency must be satisfied first");
        assert!(matches!(
            dependency_error,
            WorkOrderError::UnsatisfiedDependencies { .. }
        ));

        let wrong_source = order
            .record_evidence(evidence(EvidenceSource::BrowserResult, "browser-wrong"))
            .unwrap();
        let source_error = order
            .satisfy_obligation("navigate", &wrong_source)
            .expect_err("navigate requires an action receipt");
        assert!(matches!(
            source_error,
            WorkOrderError::UnacceptableEvidence { .. }
        ));
    }

    #[test]
    fn navigation_only_trace_keeps_title_and_delivery_pending() {
        let mut order = browser_title_order();
        order.activate().expect("activate");
        record_and_satisfy(
            &mut order,
            "navigate",
            evidence(EvidenceSource::ActionReceipt, "receipt-navigate"),
        );
        record_and_satisfy(
            &mut order,
            "verify_url",
            evidence(EvidenceSource::BrowserResult, "browser-url"),
        );

        assert_eq!(
            order.pending_obligation_ids(),
            vec!["observe_title", "deliver_title"]
        );
        let delivery = order
            .record_evidence(evidence(EvidenceSource::SessionEvent, "delivery"))
            .unwrap();
        assert_eq!(
            order
                .completion_proof(&delivery)
                .expect_err("navigation alone is incomplete"),
            WorkOrderError::UnsatisfiedObligations(vec![
                "observe_title".to_string(),
                "deliver_title".to_string(),
            ])
        );
    }

    #[test]
    fn success_requires_and_consumes_complete_delivery_proof() {
        let mut order = browser_title_order();
        order.activate().expect("activate");
        let mut delivery_id = None;
        for (obligation_id, source, source_id) in [
            (
                "navigate",
                EvidenceSource::ActionReceipt,
                "receipt-navigate",
            ),
            ("verify_url", EvidenceSource::BrowserResult, "browser-url"),
            (
                "observe_title",
                EvidenceSource::BrowserResult,
                "browser-title",
            ),
            (
                "deliver_title",
                EvidenceSource::SessionEvent,
                "delivery-title",
            ),
        ] {
            let evidence_id =
                record_and_satisfy(&mut order, obligation_id, evidence(source, source_id));
            if obligation_id == "deliver_title" {
                delivery_id = Some(evidence_id);
            }
        }
        order.begin_verification().expect("begin verification");

        let proof = order
            .completion_proof(delivery_id.as_ref().unwrap())
            .expect("complete order produces proof");
        let succeeded = order.succeed(proof).expect("proof constructs success");
        assert_eq!(succeeded.work_order().status(), WorkOrderStatus::Succeeded);
        assert_eq!(
            succeeded.work_order().final_delivery_evidence.as_ref(),
            delivery_id.as_ref()
        );
    }

    #[test]
    fn terminal_order_rejects_further_transitions_and_evidence() {
        let mut order = browser_title_order();
        order.activate().expect("activate");
        order.fail().expect("fail active order");
        assert!(matches!(
            order.resume(),
            Err(WorkOrderError::InvalidTransition { .. })
        ));
        assert_eq!(
            order
                .record_evidence(evidence(EvidenceSource::ActionReceipt, "late-receipt"))
                .expect_err("terminal order rejects evidence"),
            WorkOrderError::TerminalWorkOrder(WorkOrderStatus::Failed)
        );
    }

    #[test]
    fn failed_success_transition_returns_the_original_order() {
        let order = browser_title_order();
        let proof = CompletionProof {
            work_order_id: "another-order".to_string(),
            correction_generation: 0,
            final_delivery_evidence: EvidenceId(fingerprint("unrelated-delivery")),
        };
        let error = order
            .succeed(proof)
            .expect_err("proof must not cross orders");
        assert_eq!(error.error(), &WorkOrderError::CompletionProofMismatch);
        assert_eq!(error.into_work_order().id(), "order-1");
    }

    #[test]
    fn attempt_lifecycle_requires_dispatch_completion_and_receipt() {
        let proposed_at = Utc::now();
        assert_eq!(
            Attempt::new(
                "attempt-invalid",
                "navigate",
                "https://example.com",
                fingerprint("navigate example"),
                true,
                AttemptStatus::Executed,
                proposed_at,
                None,
                None,
                None,
            )
            .expect_err("executed attempt needs timestamps and receipt"),
            WorkOrderError::InvalidAttemptLifecycle(AttemptStatus::Executed)
        );
    }

    #[test]
    fn attempt_dispatch_time_is_the_observation_anchor() {
        let mut order = browser_title_order();
        let proposed_at = Utc::now();
        let dispatched_at = proposed_at + Duration::milliseconds(5);
        let completed_at = dispatched_at + Duration::milliseconds(5);
        let receipt_id = order
            .record_evidence(evidence_at(
                EvidenceSource::ActionReceipt,
                "attempt-receipt",
                completed_at,
            ))
            .unwrap();
        let attempt = Attempt::new(
            "attempt-executed",
            "navigate",
            "https://example.com",
            fingerprint("navigate example"),
            true,
            AttemptStatus::Executed,
            proposed_at,
            Some(dispatched_at),
            Some(completed_at),
            Some(receipt_id),
        )
        .unwrap();
        assert_eq!(attempt.dispatched_at(), Some(&dispatched_at));
        order.record_attempt(attempt).unwrap();
    }

    #[test]
    fn automatic_attempts_are_bounded_per_obligation_and_target() {
        let mut order = browser_title_order();
        let now = Utc::now();
        for index in 0..2 {
            order
                .record_attempt(
                    Attempt::new(
                        format!("attempt-{index}"),
                        "navigate",
                        "https://example.com",
                        fingerprint(&format!("navigate example {index}")),
                        true,
                        AttemptStatus::Proposed,
                        now,
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let third = Attempt::new(
            "attempt-3",
            "navigate",
            "https://example.com",
            fingerprint("navigate example third"),
            true,
            AttemptStatus::Proposed,
            now,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(matches!(
            order.record_attempt(third),
            Err(WorkOrderError::AutomaticAttemptLimit { .. })
        ));
    }

    #[test]
    fn failed_fingerprint_cannot_repeat_until_operator_correction() {
        let mut order = browser_title_order();
        let proposed_at = Utc::now();
        let dispatched_at = proposed_at + Duration::milliseconds(1);
        let completed_at = dispatched_at + Duration::milliseconds(1);
        let receipt_id = order
            .record_evidence(evidence_at(
                EvidenceSource::ActionReceipt,
                "failed-receipt",
                completed_at,
            ))
            .unwrap();
        let failed_fingerprint = fingerprint("same failed action");
        order
            .record_attempt(
                Attempt::new(
                    "failed-1",
                    "navigate",
                    "https://example.com",
                    failed_fingerprint.clone(),
                    false,
                    AttemptStatus::Failed,
                    proposed_at,
                    Some(dispatched_at),
                    Some(completed_at),
                    Some(receipt_id),
                )
                .unwrap(),
            )
            .unwrap();
        let retry = |id| {
            Attempt::new(
                id,
                "navigate",
                "https://example.com",
                failed_fingerprint.clone(),
                false,
                AttemptStatus::Proposed,
                completed_at,
                None,
                None,
                None,
            )
            .unwrap()
        };
        assert!(matches!(
            order.record_attempt(retry("retry-blocked")),
            Err(WorkOrderError::RepeatedFailedActionFingerprint(_))
        ));
        order
            .record_evidence(evidence(
                EvidenceSource::OperatorCorrection,
                "operator-correction",
            ))
            .unwrap();
        order
            .record_attempt(retry("retry-after-correction"))
            .unwrap();
        assert_eq!(order.correction_generation(), 1);
    }

    #[test]
    fn serialized_order_contains_fingerprint_but_no_source_text_field() {
        let serialized = serde_json::to_string(&browser_title_order()).expect("serialize order");
        assert!(serialized.contains(fingerprint("fixture source text").as_str()));
        assert!(!serialized.contains("source_text\""));
        assert!(serialized.contains("\"kind\":\"fixture\""));
    }
}
