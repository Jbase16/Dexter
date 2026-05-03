/// Personality layer for the Dexter core.
///
/// The personality is a first-class architectural parameter: every inference call receives
/// it as an injected system message, not as a hard-coded string in any component. This
/// separation allows the personality to be:
///   - Loaded from a YAML file the operator can edit without recompiling
///   - Replaced entirely by swapping the profile (different persona, same code)
///   - Fine-tuned independently of capability components (Phase 5+ roadmap item)
///
/// Public surface re-exported here so callers can write:
///   `use crate::personality::{PersonalityLayer, PersonalityProfile};`
/// without knowing the internal submodule layout.
pub mod layer;

#[allow(unused_imports)]
pub use layer::{PersonalityError, PersonalityLayer, PersonalityProfile, ResponseStyle};
