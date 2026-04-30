/// Proactive observation engine — Phase 17.
///
/// Governs when Dexter initiates unprompted ambient observations based on
/// context changes. See `engine.rs` for the full architectural rationale.
pub mod engine;
pub use engine::ProactiveEngine;
