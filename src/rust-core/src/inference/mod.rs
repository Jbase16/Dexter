/// Ollama inference layer for the Dexter core.
///
/// Module structure:
///   error   — `InferenceError` enum with Display and tonic::Status conversions
///   models  — `ModelId` tier selector + `ModelInfo` inventory record
///   engine  — `InferenceEngine` HTTP client wrapping the full Ollama REST API
///   router  — `ModelRouter` (tier selection) + `ConversationContext` (message buffer)
///
/// Public surface re-exported here so callers can write:
///   `use crate::inference::{InferenceEngine, GenerationRequest, Message, ModelId};`
/// without knowing the internal submodule structure.
///
/// `unused_imports` is suppressed because re-exports in a binary crate warn when the
/// consuming orchestrator (Phase 6) does not yet import them. The re-exports are
/// intentional API surface — warnings clear once Phase 6 wires the orchestrator.
pub mod engine;
pub mod error;
pub mod interceptor;
pub mod models;
pub mod retrieval_classifier;
pub mod router;

// Re-exports form the public API surface consumed by Phase 6 (Orchestrator).
// `unused_imports` is suppressed because binary-crate re-exports warn until the
// consuming orchestrator module imports them. Warnings clear in Phase 6.
#[allow(unused_imports)]
pub use engine::{EmbeddingRequest, GenerationRequest, InferenceEngine, Message, TokenChunk};
#[allow(unused_imports)]
pub use error::InferenceError;
#[allow(unused_imports)]
pub use models::{ModelId, ModelInfo};
#[allow(unused_imports)]
pub use router::{Category, Complexity, ConversationContext, ModelRouter, RoutingDecision};
