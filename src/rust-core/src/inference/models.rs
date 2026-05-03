// All public items in this module are consumed by Phase 5 (ModelRouter) and Phase 6
// (Orchestrator). Dead-code warnings are suppressed until those modules are wired in.
#![allow(dead_code)]

/// Model identity and inventory types for the Dexter inference layer.
///
/// `ModelId` is the typed routing key used by callers (the orchestrator, personality layer,
/// etc.) to request a specific inference tier without knowing which Ollama tag backs it.
/// The actual tag is resolved at call time from the operator's `ModelConfig` via
/// `ModelId::ollama_name()`.
///
/// `ModelInfo` is the inventory record returned by `InferenceEngine::list_available_models()`.
/// It mirrors the fields Ollama returns in `/api/tags` that are operationally useful —
/// `size_bytes` for disk space accounting, `parameter_size` and `quantization` for
/// display, `families` for capability detection (e.g., whether vision is supported).
use crate::config::ModelConfig;

// ── ModelId ───────────────────────────────────────────────────────────────────

/// Typed inference tier selector.
///
/// Each variant corresponds to one slot in the operator's `[models]` config section.
/// Callers use `ModelId` rather than raw strings so that:
/// 1. A typo in a model name is a compile error, not a runtime `ModelNotFound`.
/// 2. The operator can change which model backs each tier in config without touching code.
/// 3. The routing layer can apply tier-specific policies (e.g., never keep `Heavy` resident
///    in VRAM; always unload after use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelId {
    /// Fastest response, smallest model. Used for intent classification, quick lookups,
    /// and any path where latency matters more than output quality.
    /// Default: `qwen3:8b`
    Fast,

    /// Balanced quality/speed. The workhorse for most conversational turns.
    /// Default: `gemma4:26b` (MoE, 3.8B active params — ~4B-class inference speed
    /// with ~97% of 31B dense quality; natively multimodal so the Vision tier
    /// aliases to this same model by default).
    Primary,

    /// Highest-quality reasoning. Loaded on-demand only; unloaded after use to
    /// reclaim VRAM. Used for complex multi-step reasoning, long-context synthesis.
    /// Default: `deepseek-r1:32b`
    Heavy,

    /// Code generation, review, and completion. Optimised for programming tasks.
    /// Default: `deepseek-coder-v2:16b`
    Code,

    /// Vision + language. Accepts image bytes alongside text prompts.
    /// Default: `gemma4:26b` — aliased to PRIMARY because Gemma 4 is natively
    /// multimodal. Operators with a dedicated vision model can set this to
    /// `qwen3-vl:8b`, `llama3.2-vision:11b`, or any other multimodal tag.
    Vision,

    /// Dense vector embedding for semantic retrieval (Phase 9).
    /// Uses `/api/embed`, not `/api/chat`. Output is `Vec<Vec<f32>>`.
    /// Default: `mxbai-embed-large`
    Embed,
}

impl ModelId {
    /// Resolve this tier to the operator-configured Ollama model tag.
    ///
    /// The returned string is the tag as it appears in `ollama list` output, e.g.
    /// `"qwen3:8b"`. This is passed verbatim to Ollama in request bodies.
    pub fn ollama_name<'a>(&self, cfg: &'a ModelConfig) -> &'a str {
        match self {
            ModelId::Fast => &cfg.fast,
            ModelId::Primary => &cfg.primary,
            ModelId::Heavy => &cfg.heavy,
            ModelId::Code => &cfg.code,
            ModelId::Vision => &cfg.vision,
            ModelId::Embed => &cfg.embed,
        }
    }

    /// Human-readable tier name for logging and display. Does not change with config.
    pub fn tier_name(&self) -> &'static str {
        match self {
            ModelId::Fast => "fast",
            ModelId::Primary => "primary",
            ModelId::Heavy => "heavy",
            ModelId::Code => "code",
            ModelId::Vision => "vision",
            ModelId::Embed => "embed",
        }
    }

    /// Returns true if this tier should be unloaded from VRAM immediately after use.
    ///
    /// Heavy (~19GB) always unloads: would evict the PRIMARY model from VRAM if left
    /// resident given the 36GB unified memory budget.
    ///
    /// Vision unloads ONLY when it resolves to a different Ollama model than PRIMARY.
    /// When an operator configures a multimodal PRIMARY (e.g., `gemma4:26b`, which
    /// handles both text and image input) and aliases `vision = primary`, unloading
    /// after a Vision-routed query would evict the warm PRIMARY — defeating the
    /// 30-minute keep-alive and forcing a cold reload on the next chat turn. In that
    /// case Vision piggybacks on PRIMARY's keep-alive and does NOT unload.
    ///
    /// When Vision is a distinct model (e.g., `llama3.2-vision:11b`, `qwen3-vl:8b`),
    /// the original Phase 20 rationale applies: mutual exclusion with Heavy, unload
    /// after each query to free VRAM.
    ///
    /// All other tiers (Fast, Primary, Code, Embed) benefit from staying warm.
    ///
    /// Unloading is done via Ollama's `keep_alive: 0` parameter on the final
    /// request, not via a separate API call.
    pub fn unload_after_use(&self, cfg: &ModelConfig) -> bool {
        match self {
            ModelId::Heavy => true,
            ModelId::Vision => cfg.vision != cfg.primary,
            _ => false,
        }
    }

    /// Returns true if this tier must have its KV-cache context window capped
    /// (via Ollama's `num_ctx` option) before dispatch.
    ///
    /// Phase 37.7: previously orchestrator dispatch used `unload_after_use()`
    /// as a proxy for "this model has a huge native context, cap it" — a silent
    /// conflation. The two properties are orthogonal: Heavy unloads AND needs
    /// capping; Code stays warm AND needs capping; Primary stays warm and does
    /// NOT need capping. Using `unload_after_use` as the predicate meant Code
    /// dispatches silently used deepseek-coder-v2:16b's native 163,840-token
    /// context window, allocating ~20 GiB of KV cache and CPU-spilling on a
    /// 36 GiB unified-memory budget. Observed symptom: first-token latency
    /// exceeded the 90 s `GENERATION_WALL_TIMEOUT_SECS` ceiling and every CODE
    /// query aborted with `stuck-think timeout`.
    ///
    /// Returns true for tiers whose native context defaults are too large to
    /// fit alongside the always-warm stack (FAST + PRIMARY + EMBED ≈ 24 GiB):
    /// Heavy (deepseek-r1:32b, 131k native) and Code (deepseek-coder-v2:16b,
    /// 163k native). Fast (qwen3:8b, 40k native) and Primary (gemma4:26b MoE,
    /// 128k native but low active-param KV footprint) stay at Ollama's default.
    ///
    /// The cap value itself lives in `constants::LARGE_MODEL_NUM_CTX`.
    pub fn needs_context_cap(&self) -> bool {
        matches!(self, ModelId::Heavy | ModelId::Code)
    }
}

// ── ModelInfo ─────────────────────────────────────────────────────────────────

/// Inventory record for a single model returned by `InferenceEngine::list_available_models()`.
///
/// Field names mirror Ollama's `/api/tags` response shape (via the `OllamaTagsModel`
/// serde struct in engine.rs) but are re-exposed here as a clean public type without
/// the serde annotations. Callers get a plain Rust struct; the wire format is an
/// implementation detail of engine.rs.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// The model tag as it appears in `ollama list`. This is the identifier used in
    /// all Ollama API requests.
    pub name: String,

    /// Size on disk in bytes. Useful for disk space accounting and for verifying
    /// that a pull completed successfully (size > 0 means the model is not corrupt).
    pub size_bytes: u64,

    /// Content-addressable digest. Can be used to detect if a model has been
    /// updated in place (same name, different digest).
    pub digest: String,

    /// Human-readable parameter count, e.g. `"8B"`, `"32B"`. Sourced from Ollama's
    /// model details — may be empty string if Ollama does not report it.
    pub parameter_size: String,

    /// Quantization level, e.g. `"Q4_K_M"`, `"F16"`. Sourced from Ollama's model
    /// details — may be empty string if Ollama does not report it.
    pub quantization: String,

    /// Architecture families, e.g. `["llama"]`, `["bert"]`. Used to detect whether
    /// a model supports multimodal input — vision models typically include `"clip"`
    /// in their families list.
    pub families: Vec<String>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelConfig;

    fn default_cfg() -> ModelConfig {
        ModelConfig::default()
    }

    #[test]
    fn ollama_name_resolves_all_tiers() {
        let cfg = default_cfg();
        assert_eq!(ModelId::Fast.ollama_name(&cfg), "qwen3:8b");
        assert_eq!(ModelId::Primary.ollama_name(&cfg), "gemma4:26b");
        assert_eq!(ModelId::Heavy.ollama_name(&cfg), "deepseek-r1:32b");
        assert_eq!(ModelId::Code.ollama_name(&cfg), "deepseek-coder-v2:16b");
        assert_eq!(ModelId::Vision.ollama_name(&cfg), "gemma4:26b");
        assert_eq!(ModelId::Embed.ollama_name(&cfg), "mxbai-embed-large");
    }

    #[test]
    fn heavy_always_unloads_after_use() {
        // Heavy (~19GB) must always unload to free VRAM for PRIMARY to reload.
        let cfg = default_cfg();
        assert!(ModelId::Heavy.unload_after_use(&cfg));
    }

    #[test]
    fn vision_does_not_unload_when_aliased_to_primary() {
        // When Vision resolves to the same Ollama model as PRIMARY (the default
        // when PRIMARY is a multimodal model like gemma4:26b), unloading would
        // evict PRIMARY from VRAM — defeating the 30m keep-alive and causing a
        // cold reload on the next chat turn. Vision must piggyback on PRIMARY's
        // keep-alive in that configuration.
        let cfg = default_cfg();
        assert_eq!(
            cfg.vision, cfg.primary,
            "default config must alias vision to primary"
        );
        assert!(
            !ModelId::Vision.unload_after_use(&cfg),
            "aliased Vision must NOT unload — would evict warm PRIMARY"
        );
    }

    #[test]
    fn vision_unloads_when_distinct_from_primary() {
        // When operator configures a dedicated vision model (e.g., qwen3-vl:8b),
        // the Phase 20 rationale applies: unload after use to maintain VRAM
        // headroom for Heavy (~19GB) on 36GB hardware.
        let mut cfg = default_cfg();
        cfg.vision = "qwen3-vl:8b".to_string();
        assert_ne!(cfg.vision, cfg.primary);
        assert!(
            ModelId::Vision.unload_after_use(&cfg),
            "distinct Vision model must unload — mutual exclusion with Heavy"
        );
    }

    #[test]
    fn non_unloading_tiers_stay_resident() {
        let cfg = default_cfg();
        assert!(!ModelId::Fast.unload_after_use(&cfg));
        assert!(!ModelId::Primary.unload_after_use(&cfg));
        assert!(!ModelId::Code.unload_after_use(&cfg));
        assert!(!ModelId::Embed.unload_after_use(&cfg));
    }

    #[test]
    fn needs_context_cap_covers_heavy_and_code() {
        // Phase 37.7: large-native-context tiers must have num_ctx capped before
        // dispatch or Ollama allocates 20–32 GiB of KV cache and CPU-spills.
        // Previously the predicate was conflated with `unload_after_use`, which
        // excluded Code (stays warm but still has a 163k native context).
        assert!(
            ModelId::Heavy.needs_context_cap(),
            "Heavy (deepseek-r1:32b, 131k native) must be capped"
        );
        assert!(ModelId::Code.needs_context_cap(),
            "Code (deepseek-coder-v2:16b, 163k native) must be capped — regression guard for CODE stuck-think timeout");
    }

    #[test]
    fn needs_context_cap_excludes_small_context_tiers() {
        // Fast (qwen3:8b, 40k native) and Primary (gemma4:26b MoE) fit under
        // Ollama's default allocation. Embed has no text-generation path so
        // num_ctx is irrelevant. Vision aliases Primary by default and must
        // not introduce a separate cap path.
        assert!(!ModelId::Fast.needs_context_cap());
        assert!(!ModelId::Primary.needs_context_cap());
        assert!(!ModelId::Vision.needs_context_cap());
        assert!(!ModelId::Embed.needs_context_cap());
    }

    #[test]
    fn needs_context_cap_is_orthogonal_to_unload_after_use() {
        // The whole point of Phase 37.7's split is that these two properties
        // are independent. Heavy: both true. Code: cap but no unload. Primary/Fast:
        // neither. Pin the full truth table so future refactors can't re-conflate.
        let cfg = default_cfg();
        assert_eq!(
            (
                ModelId::Heavy.unload_after_use(&cfg),
                ModelId::Heavy.needs_context_cap()
            ),
            (true, true)
        );
        assert_eq!(
            (
                ModelId::Code.unload_after_use(&cfg),
                ModelId::Code.needs_context_cap()
            ),
            (false, true)
        );
        assert_eq!(
            (
                ModelId::Primary.unload_after_use(&cfg),
                ModelId::Primary.needs_context_cap()
            ),
            (false, false)
        );
        assert_eq!(
            (
                ModelId::Fast.unload_after_use(&cfg),
                ModelId::Fast.needs_context_cap()
            ),
            (false, false)
        );
    }

    #[test]
    fn tier_names_are_lowercase_ascii() {
        let all = [
            ModelId::Fast,
            ModelId::Primary,
            ModelId::Heavy,
            ModelId::Code,
            ModelId::Vision,
            ModelId::Embed,
        ];
        for tier in &all {
            let name = tier.tier_name();
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase()),
                "tier_name '{}' contains non-lowercase-ascii",
                name
            );
        }
    }

    #[test]
    fn ollama_name_reflects_config_override() {
        // Verify that ollama_name() returns whatever the config holds, not a hardcoded value.
        // If an operator overrides fast = "llama3.2:1b" in their config.toml, ModelId::Fast
        // must route to their choice.
        let mut cfg = default_cfg();
        cfg.fast = "llama3.2:1b".to_string();
        assert_eq!(ModelId::Fast.ollama_name(&cfg), "llama3.2:1b");
        // Other tiers unaffected.
        assert_eq!(ModelId::Primary.ollama_name(&cfg), "gemma4:26b");
    }
}
