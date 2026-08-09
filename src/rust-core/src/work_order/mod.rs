//! Rust-owned work-order records and lifecycle invariants.
//!
//! Slice A is shadow-only structural evidence. It does not classify turns,
//! derive obligations from operator language, dispatch actions, or affect any
//! production completion surface.

pub(crate) mod evidence;
#[cfg(test)]
mod replay;
pub(crate) mod shadow;
pub(crate) mod types;
