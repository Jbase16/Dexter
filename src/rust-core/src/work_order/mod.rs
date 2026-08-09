//! Rust-owned work-order records and lifecycle invariants.
//!
//! Slice A supplies shadow-only structural evidence. Slice B1 adds a
//! decision-only turn-entry classifier, but it is not wired into production
//! routing and does not yet derive obligations, dispatch actions, or affect an
//! operator-visible completion surface.

pub(crate) mod entry;
pub(crate) mod evidence;
#[cfg(test)]
mod replay;
pub(crate) mod shadow;
pub(crate) mod types;
