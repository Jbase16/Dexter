//! Browser automation worker coordination.
//!
//! Provides `BrowserCoordinator` — a long-lived Playwright Chromium subprocess
//! manager following the same lifecycle pattern as `voice::VoiceCoordinator`.
pub mod coordinator;
pub mod diagnostics;

pub use coordinator::BrowserCoordinator;
