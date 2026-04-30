/// Action Engine — Phase 8.
///
/// Provides Dexter with the ability to take system actions: run shell commands,
/// read/write files, and execute AppleScript. Every action is classified by
/// `PolicyEngine` (SAFE / CAUTIOUS / DESTRUCTIVE) and written to the audit log.
/// DESTRUCTIVE actions require explicit operator approval before execution.
///
/// ## Module structure
///
/// - `engine`   — `ActionEngine`, `ActionSpec`, `ActionOutcome`
/// - `policy`   — `PolicyEngine::classify()`
/// - `executor` — OS-level execution functions (shell, file ops, AppleScript)
/// - `audit`    — `AuditLog` + `AuditEntry`: append-only JSONL audit record

pub mod audit;
pub mod engine;
pub mod executor;
pub mod policy;

pub use engine::{ActionEngine, ActionOutcome, ActionResult, ActionSpec, ExecutorHandle};
#[allow(unused_imports)] // Phase 9+ — external callers will use crate::action::PolicyEngine
pub use policy::PolicyEngine;
#[allow(unused_imports)] // Phase 9+ — external callers will use crate::action::AuditLog
pub use audit::AuditLog;
