//! Runtime context compilation.
//!
//! The live compiler is intentionally deterministic: it builds cheap candidate
//! representations, estimates their prompt cost, packs by explainable ROI, and
//! emits diagnostics. Offline learning can later update the weights without
//! adding latency to the operator-facing turn.

pub mod candidate;
pub mod compiler;
pub mod diagnostics;
pub mod ledger;
pub mod representation;
pub mod turn_record;

pub(crate) use candidate::{
    CandidateFeatures, ContextCandidate, ContextInjectionTarget, ContextPriority, ContextRiskClass,
    ContextSourceKind, RepresentationSelectionPolicy, TaskClass,
};
pub(crate) use compiler::{ContextCompiler, ContextCompilerConfig};
pub(crate) use representation::{CandidateRepresentation, RepresentationKind};
