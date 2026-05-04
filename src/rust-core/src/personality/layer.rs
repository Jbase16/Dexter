/// PersonalityLayer — loads and applies the operator's personality profile.
///
/// The personality is a first-class architectural parameter, not a hardcoded system prompt.
/// Every inference call receives the personality via an injected system message; no
/// component in the capability stack (routing, retrieval, action) contains personality logic.
///
/// ## Loading
///
/// Use `PersonalityLayer::load(path)` for explicit path control or
/// `PersonalityLayer::load_or_default_from(&cfg.core.personality_path)` to honor the
/// operator's `~/.dexter/config.toml` override with a graceful fallback to built-in
/// defaults if the file is absent. (Phase 38 wired `personality_path` through —
/// previously the loader ignored config and used a compile-time constant.)
///
/// ## Injection
///
/// `apply_to_messages()` returns a new `Vec<Message>` with the system prompt prepended.
/// If the caller's message list already contains a system message at index 0, the
/// personality prefix is prepended to that message's content — it is never replaced.
/// If there is no system message, a new one is inserted at index 0.
///
/// This design lets the orchestrator add domain-specific context (e.g., retrieved facts)
/// to the system message while still having personality applied on top.
use std::fmt;
use std::path::Path;

use serde::Deserialize;
use tracing::warn;

// Phase 38: PERSONALITY_CONFIG_PATH was used by the old `load_or_default()` wrapper
// (removed when personality_path got wired through to config). It's now only
// referenced from tests, so the import lives behind cfg(test) to keep the
// production binary warning-free without using #[allow] suppression.
#[cfg(test)]
use crate::constants::PERSONALITY_CONFIG_PATH;
use crate::inference::engine::Message;

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum PersonalityError {
    /// The personality YAML file does not exist at the given path.
    FileNotFound(String),
    /// The YAML content could not be parsed into `PersonalityProfile`.
    /// Carries the path and the parse error description so operators can locate
    /// and fix YAML syntax errors without reading the binary's stderr in a
    /// structured log tool.
    ParseError { path: String, source: String },
}

impl fmt::Display for PersonalityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PersonalityError::FileNotFound(p) => {
                write!(f, "Personality profile not found at '{p}'")
            }
            PersonalityError::ParseError { path, source } => {
                write!(
                    f,
                    "Failed to parse personality profile at '{path}': {source}"
                )
            }
        }
    }
}

impl std::error::Error for PersonalityError {}

// ── Serde structs (mirror the YAML shape in default.yaml) ────────────────────

/// The operator's response-style preferences.
///
/// These are injected into the system prompt as behavioral directives and do not
/// affect routing or model selection — they are purely generational guidance.
#[derive(Debug, Clone, Deserialize)]
pub struct ResponseStyle {
    /// Verbosity ceiling: "low", "medium", or "high". Medium means explain when
    /// explanation adds value; do not over-explain when the concept is clear.
    #[allow(dead_code)] // Phase 7 — response style applied via system prompt directives
    #[serde(default = "default_verbosity")]
    pub max_verbosity: String,

    /// If true, all code responses must use fenced code blocks with language tags.
    /// Prevents models from formatting code as plain text prose.
    #[serde(default = "default_true")]
    pub code_always_formatted: bool,

    /// If true, the system prompt explicitly prohibits padding responses with
    /// restatements, caveats, and "let me know if..." endings.
    #[serde(default = "default_true")]
    pub never_pad_to_seem_thorough: bool,
}

fn default_verbosity() -> String {
    "medium".to_string()
}
fn default_true() -> bool {
    true
}

impl Default for ResponseStyle {
    fn default() -> Self {
        Self {
            max_verbosity: default_verbosity(),
            code_always_formatted: default_true(),
            never_pad_to_seem_thorough: default_true(),
        }
    }
}

/// Round 3 / T1.2: a lazy-loaded domain-specific instruction block.
///
/// Purpose: keep the "core" system prompt small (identity, tone, action format,
/// general anti-patterns) and only inject heavy domain-specific guidance —
/// iMessage workflows, yt-dlp patterns, browser chains, macOS shell syntax —
/// when the operator's query actually concerns that domain.
///
/// Trigger matching is case-insensitive substring on the last user message.
/// If ANY trigger matches, the `content` is appended to the system prompt
/// for that request only. Domain blocks are otherwise invisible — the model
/// never sees a "You can do X but I didn't load instructions for it" seam
/// because the core prompt itself carries no references to domain names.
///
/// Cost model: a missed trigger costs nothing (no tokens injected). A false
/// positive costs the domain's token count once. Keep triggers specific
/// (nouns/verbs that only appear in genuine domain queries).
#[derive(Debug, Clone, Deserialize)]
pub struct DomainBlock {
    /// Operator-facing identifier for logs and debugging (e.g. "imessage",
    /// "download", "browser", "shell-macos"). Not shown to the model.
    #[serde(default)]
    pub name: String,

    /// Substring matchers, case-insensitive. If any matches the current user
    /// turn, this block's `content` is appended to the system prompt.
    ///
    /// Prefer verbs/nouns unique to the domain ("imessage", "text", "send a
    /// message to", "yt-dlp", "download", "safari", "ps aux"). Avoid words
    /// that span domains ("run", "open", "show").
    #[serde(default)]
    pub triggers: Vec<String>,

    /// The instruction block appended to the system prompt when triggered.
    /// Typically 20–100 lines of markdown-style guidance.
    #[serde(default)]
    pub content: String,

    /// Optional anti-patterns specific to this domain, appended to the
    /// system-prompt anti-patterns section when the block is triggered.
    #[serde(default)]
    pub anti_patterns: Vec<String>,
}

/// The full personality profile as deserialized from `default.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct PersonalityProfile {
    /// Display name for the entity. Used in UI and logging.
    #[serde(default = "default_name")]
    pub name: String,

    /// Profile schema version for forward-compatibility checks.
    #[allow(dead_code)] // Phase 7 — version-mismatch detection when loading updated profiles
    #[serde(default)]
    pub version: String,

    /// Core identity and behavioral description. This is the primary system prompt
    /// body — all other fields generate directives appended to this.
    #[serde(default = "default_system_prompt")]
    pub system_prompt_prefix: String,

    /// Explicit communication-style rules. Each directive becomes a bullet point
    /// in the generated system prompt under a "Communication rules:" heading.
    #[serde(default)]
    pub tone_directives: Vec<String>,

    /// Response formatting preferences. Applied via system prompt directives.
    #[serde(default)]
    pub response_style: ResponseStyle,

    /// Patterns the model must not exhibit. Each becomes a "Never do:" bullet.
    /// Negative examples are more effective than positive instruction for these
    /// specific behavioral failures.
    #[serde(default)]
    pub anti_patterns: Vec<String>,

    /// Round 3 / T1.2: lazy-loaded domain-specific blocks. See `DomainBlock`.
    ///
    /// The blocks are evaluated in declaration order; matching blocks are
    /// appended to the system prompt in the same order. Duplicate triggers
    /// across blocks are allowed — the model receives both blocks (extra
    /// tokens, but no semantic conflict if the author wrote them consistently).
    #[serde(default)]
    pub domains: Vec<DomainBlock>,

    /// Optional path to an operator-trained LoRA adapter. When set, the
    /// InferenceEngine creates a temporary Modelfile and applies the adapter
    /// before generation. `null` (YAML) → `None` (Rust) = base model only.
    #[serde(default)]
    pub lora_adapter_path: Option<String>,
}

fn default_name() -> String {
    "Dexter".to_string()
}

fn default_system_prompt() -> String {
    // Minimal fallback system prompt used when the YAML file is absent and
    // the default profile is constructed in-memory. Better than nothing.
    "You are Dexter, a persistent AI entity running at the system level on macOS. \
     You are aware of what's happening on the machine. Be direct and precise."
        .to_string()
}

impl Default for PersonalityProfile {
    fn default() -> Self {
        Self {
            name: default_name(),
            version: "1.0".to_string(),
            system_prompt_prefix: default_system_prompt(),
            tone_directives: vec![
                "Be direct. Avoid corporate hedging language.".to_string(),
                "Match the operator's register.".to_string(),
            ],
            response_style: ResponseStyle::default(),
            anti_patterns: vec![
                "Starting responses with 'Certainly!', 'Of course!', or 'Great!'".to_string(),
                "Ending with 'Let me know if you need anything else!'".to_string(),
            ],
            domains: Vec::new(),
            lora_adapter_path: None,
        }
    }
}

// ── PersonalityLayer ──────────────────────────────────────────────────────────

/// The runtime personality injector.
///
/// Holds a loaded `PersonalityProfile` and applies it to message lists before
/// they are sent to the InferenceEngine. The layer is immutable after construction.
#[derive(Debug, Clone)]
pub struct PersonalityLayer {
    profile: PersonalityProfile,
}

impl PersonalityLayer {
    /// Load a personality profile from the given YAML path.
    ///
    /// Returns `PersonalityError::FileNotFound` if the path does not exist.
    /// Returns `PersonalityError::ParseError` if the YAML is malformed.
    pub fn load(path: &Path) -> Result<Self, PersonalityError> {
        let path_str = path.display().to_string();

        let content = std::fs::read_to_string(path)
            .map_err(|_| PersonalityError::FileNotFound(path_str.clone()))?;

        let profile: PersonalityProfile =
            serde_yaml::from_str(&content).map_err(|e| PersonalityError::ParseError {
                path: path_str,
                source: e.to_string(),
            })?;

        Ok(Self { profile })
    }

    /// Load from the configured path, falling back to built-in defaults on error.
    ///
    /// Phase 38 / Codex finding [33]: `core.personality_path` was deserialized from
    /// `~/.dexter/config.toml` but never plumbed through to the loader — operators
    /// who set a custom path silently got the default. This entry point closes that
    /// gap. Same fallback semantics as `load_or_default()`:
    /// - If the file is absent → use defaults, log a warning.
    /// - If the YAML is malformed → use defaults, log a warning.
    /// The daemon should not refuse to start because the personality file is missing.
    pub fn load_or_default_from(path_str: &str) -> Self {
        let path = Path::new(path_str);
        match Self::load(path) {
            Ok(layer) => layer,
            Err(e @ PersonalityError::FileNotFound(_)) => {
                warn!(
                    path = path_str,
                    "Personality profile not found — using built-in defaults. \
                     Error: {e}"
                );
                Self::with_defaults()
            }
            Err(e @ PersonalityError::ParseError { .. }) => {
                warn!(
                    path = path_str,
                    "Personality profile YAML parse error — using built-in defaults. \
                     Error: {e}"
                );
                Self::with_defaults()
            }
        }
    }

    /// Construct a layer from the built-in default profile.
    ///
    /// Used as the fallback in `load_or_default()` and in unit tests that don't
    /// require a real YAML file on disk.
    pub fn with_defaults() -> Self {
        Self {
            profile: PersonalityProfile::default(),
        }
    }

    /// Return a reference to the loaded profile.
    pub fn profile(&self) -> &PersonalityProfile {
        &self.profile
    }

    /// Phase 19: Structured uncertainty sentinel instructions injected into every
    /// system prompt. When the model is genuinely uncertain about a factual claim,
    /// it emits `[UNCERTAIN: <query>]` — a machine-readable marker that the
    /// orchestrator intercepts mid-stream and routes to the retrieval pipeline.
    ///
    /// Placed AFTER the personality directives so identity/tone apply first.
    /// Placed BEFORE any dynamic context (app name, focused element) so the
    /// instructions are always present regardless of whether context is available.
    const UNCERTAINTY_PROTOCOL: &'static str = "\n\nUNCERTAINTY PROTOCOL:\n\
        When you are genuinely uncertain about a specific factual claim — a current date,\n\
        a software version number, a named person's current role, a recent event — output\n\
        exactly this marker and nothing else on that topic:\n\
        [UNCERTAIN: <query>]\n\
        where <query> is a precise, self-contained web search query that would retrieve the\n\
        missing fact.\n\n\
        Use the marker for:\n\
        - Current or recent dates, times, and events\n\
        - Software version numbers (they change)\n\
        - Named individuals' current titles, positions, or status\n\
        - Any statistic or quantity that changes over time\n\n\
        Do NOT use the marker for:\n\
        - Conceptual explanations, even if complex\n\
        - Code generation or debugging\n\
        - Architectural or design reasoning\n\
        - Content about the operator's local machine or codebase\n\n\
        After emitting the marker, stop generating. The retrieval result will be injected\n\
        into this conversation and you will continue your response from it.\n\n\
        This marker is intercepted automatically — the operator never sees it.";

    /// Build the complete system prompt without any domain blocks (core-only).
    ///
    /// Equivalent to `build_system_prompt_for(None)`; provided for call-sites
    /// and tests that don't have a user query to match against.
    #[allow(dead_code)] // Retained for tests + tooling that snapshot the core prompt
    pub fn build_system_prompt(&self) -> String {
        self.build_system_prompt_for(None)
    }

    /// Build the complete system prompt, conditionally appending domain blocks
    /// whose triggers match `user_query`.
    ///
    /// The generated prompt has these sections, in order:
    /// 1. `system_prompt_prefix` — core identity and behavioral description
    /// 2. "Communication rules:" — one bullet per `tone_directive`
    /// 3. "Anti-patterns to never use:" — core anti-patterns + any matching
    ///    domain anti-patterns (each domain's anti-patterns ride with its
    ///    content so they're loaded as a unit)
    /// 4. Response-style lines (fenced code, no-padding) when enabled
    /// 5. `UNCERTAINTY_PROTOCOL` — structured uncertainty sentinel (Phase 19)
    /// 6. Matching `DomainBlock.content` sections, declared order
    ///
    /// When `user_query` is `None` or empty, only the core prompt is produced —
    /// matches the pre-T1.2 behaviour exactly for callers that haven't been
    /// plumbed to pass a query yet.
    ///
    /// The function is deterministic for a given (profile, user_query) pair.
    pub fn build_system_prompt_for(&self, user_query: Option<&str>) -> String {
        let p = &self.profile;
        let mut prompt = p.system_prompt_prefix.trim_end().to_string();

        // Determine which domain blocks match so we can fold their anti-patterns
        // into the anti-pattern section BEFORE appending the content bodies.
        // Lowercasing once is cheaper than matching N domain blocks * M triggers.
        let query_lc: Option<String> = user_query
            .map(|q| q.to_lowercase())
            .filter(|q| !q.is_empty());

        let matching: Vec<&DomainBlock> = match &query_lc {
            Some(q) => p
                .domains
                .iter()
                .filter(|d| {
                    d.triggers
                        .iter()
                        .any(|t| !t.is_empty() && q.contains(&t.to_lowercase()))
                })
                .collect(),
            None => Vec::new(),
        };

        if !p.tone_directives.is_empty() {
            prompt.push_str("\n\nCommunication rules:");
            for directive in &p.tone_directives {
                prompt.push_str(&format!("\n- {directive}"));
            }
        }

        let has_core_aps = !p.anti_patterns.is_empty();
        let has_domain_aps = matching.iter().any(|d| !d.anti_patterns.is_empty());
        if has_core_aps || has_domain_aps {
            prompt.push_str("\n\nAnti-patterns to never use:");
            for pattern in &p.anti_patterns {
                prompt.push_str(&format!("\n- {pattern}"));
            }
            for d in &matching {
                for pattern in &d.anti_patterns {
                    prompt.push_str(&format!("\n- {pattern}"));
                }
            }
        }

        if p.response_style.code_always_formatted {
            prompt.push_str("\n\nAll code must use fenced code blocks with a language tag.");
        }

        if p.response_style.never_pad_to_seem_thorough {
            prompt.push_str("\nNever pad responses to seem thorough. Answer what is asked.");
        }

        // Phase 19: uncertainty protocol before domain blocks so its rules apply
        // to all domain-specific guidance that follows.
        prompt.push_str(Self::UNCERTAINTY_PROTOCOL);

        // Round 3 / T1.2: append matching domain content. Each block gets a
        // blank line separator so the model sees a clear section break —
        // prevents accidental concatenation of unrelated domain guidance.
        for d in &matching {
            if !d.content.trim().is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(d.content.trim_end());
            }
        }

        prompt
    }

    /// Round 3 / T1.2: names of domain blocks that would match `user_query`.
    /// Used by the orchestrator to emit a debug log on each turn showing which
    /// domain blocks were loaded — makes it cheap to verify trigger tuning.
    ///
    /// The orchestrator currently inlines the matching logic (to avoid cloning
    /// the triggers list twice per turn) but the standalone helper is kept for
    /// tests, debugging tools, and future callers.
    #[allow(dead_code)]
    pub fn matching_domain_names(&self, user_query: &str) -> Vec<String> {
        if user_query.is_empty() {
            return Vec::new();
        }
        let q = user_query.to_lowercase();
        self.profile
            .domains
            .iter()
            .filter(|d| {
                d.triggers
                    .iter()
                    .any(|t| !t.is_empty() && q.contains(&t.to_lowercase()))
            })
            .map(|d| d.name.clone())
            .collect()
    }

    /// Apply the personality to a message list and return the result (core-only).
    ///
    /// Equivalent to `apply_to_messages_for(messages, None)`. Use this when
    /// there's no meaningful user query to guide domain selection (e.g. the
    /// KV-cache prefill path or a synthetic test).
    pub fn apply_to_messages(&self, messages: &[Message]) -> Vec<Message> {
        self.apply_to_messages_for(messages, None)
    }

    /// Apply the personality to a message list and return the result, with
    /// domain blocks conditionally loaded based on `user_query`.
    ///
    /// Policy:
    /// - If `messages` is empty → return a single-element vec containing only the system message.
    /// - If `messages[0]` is a system message → prepend the personality prompt to it, separated
    ///   by a `\n\n`. This lets the orchestrator add domain-specific context (retrieved facts,
    ///   session state) to the system message while still having identity/tone applied first.
    /// - Otherwise → insert a new system message at index 0.
    ///
    /// `user_query`: the last operator utterance (not including tool-result
    /// prefixes). When `Some`, domain blocks whose triggers match are appended
    /// to the system prompt; see `build_system_prompt_for` for details.
    ///
    /// The returned Vec is owned — callers pass it directly to `InferenceEngine::generate_stream`.
    pub fn apply_to_messages_for(
        &self,
        messages: &[Message],
        user_query: Option<&str>,
    ) -> Vec<Message> {
        let personality_prompt = self.build_system_prompt_for(user_query);

        let mut result = Vec::with_capacity(messages.len() + 1);

        match messages.first() {
            None => {
                // No messages at all — just the system prompt.
                result.push(Message::system(personality_prompt));
            }
            Some(first) if first.role == "system" => {
                // Prepend to existing system message. Personality comes first so
                // operator-specified identity and tone aren't overridden by later context.
                let merged_content =
                    format!("{personality_prompt}\n\n{}", first.content.trim_start());
                result.push(Message::system(merged_content));
                result.extend_from_slice(&messages[1..]);
            }
            _ => {
                // No system message — insert one.
                result.push(Message::system(personality_prompt));
                result.extend_from_slice(messages);
            }
        }

        result
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::engine::Message;

    fn defaults() -> PersonalityLayer {
        PersonalityLayer::with_defaults()
    }

    #[test]
    fn build_system_prompt_includes_prefix() {
        let layer = defaults();
        let prompt = layer.build_system_prompt();
        // The default profile's system_prompt_prefix should appear verbatim.
        assert!(
            prompt.contains("Dexter"),
            "system prompt should include the entity name"
        );
    }

    #[test]
    fn build_system_prompt_includes_tone_directives() {
        let layer = defaults();
        let prompt = layer.build_system_prompt();
        assert!(
            prompt.contains("Communication rules:"),
            "should have a tone section"
        );
    }

    #[test]
    fn build_system_prompt_includes_anti_patterns() {
        let layer = defaults();
        let prompt = layer.build_system_prompt();
        assert!(
            prompt.contains("Anti-patterns"),
            "should have an anti-patterns section"
        );
    }

    #[test]
    fn apply_to_messages_empty_input_returns_only_system() {
        let layer = defaults();
        let result = layer.apply_to_messages(&[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "system");
    }

    #[test]
    fn apply_to_messages_inserts_system_before_user() {
        let layer = defaults();
        let messages = vec![Message::user("What time is it?".to_string())];
        let result = layer.apply_to_messages(&messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[1].role, "user");
    }

    #[test]
    fn apply_to_messages_prepends_to_existing_system() {
        let layer = defaults();
        let messages = vec![
            Message::system(
                "Operator-added context: the user is debugging a Rust binary.".to_string(),
            ),
            Message::user("What's wrong with this code?".to_string()),
        ];
        let result = layer.apply_to_messages(&messages);
        // Should still be 2 messages (system + user), not 3.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "system");
        // Personality prompt should appear before the operator context.
        let content = &result[0].content;
        let personality_pos = content.find("Dexter").unwrap_or(usize::MAX);
        let operator_pos = content
            .find("debugging a Rust binary")
            .unwrap_or(usize::MAX);
        assert!(
            personality_pos < operator_pos,
            "personality prompt must precede operator-added system context"
        );
        assert_eq!(result[1].role, "user");
    }

    #[test]
    fn apply_to_messages_preserves_conversation_history() {
        let layer = defaults();
        let messages = vec![
            Message::user("Hello".to_string()),
            Message::assistant("Hi".to_string()),
            Message::user("What is 2+2?".to_string()),
        ];
        let result = layer.apply_to_messages(&messages);
        // system + 3 history messages = 4 total
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[1].role, "user");
        assert_eq!(result[2].role, "assistant");
        assert_eq!(result[3].role, "user");
    }

    #[test]
    fn load_from_project_yaml_succeeds() {
        // This test verifies that the actual project YAML parses correctly.
        // It requires the test to be run from the project root (which cargo test does
        // by default when run from src/rust-core/).
        let path = Path::new("../../config/personality/default.yaml");
        if !path.exists() {
            // Tolerate missing file in CI where directory structure may differ.
            return;
        }
        let result = PersonalityLayer::load(path);
        assert!(
            result.is_ok(),
            "Project YAML should parse without errors: {:?}",
            result
        );
        let layer = result.unwrap();
        assert_eq!(layer.profile().name, "Dexter");
        assert!(
            layer
                .profile()
                .system_prompt_prefix
                .contains("system level"),
            "Loaded system_prompt_prefix should contain expected content"
        );
    }

    #[test]
    fn project_yaml_loads_vision_measurement_domain_for_size_queries() {
        let path = Path::new("../../config/personality/default.yaml");
        if !path.exists() {
            return;
        }
        let layer = PersonalityLayer::load(path).expect("project personality YAML should parse");

        let prompt = layer.build_system_prompt_for(Some("estimate the length using the soda can"));

        assert!(
            prompt.contains("VISION MEASUREMENT DOMAIN"),
            "size/reference queries should load the vision measurement honesty domain"
        );
        assert!(
            prompt.contains("Do not present image-only measurements as exact"),
            "measurement domain must warn against fake precision"
        );
    }

    #[test]
    fn project_yaml_does_not_load_vision_measurement_domain_for_plain_vision_queries() {
        let path = Path::new("../../config/personality/default.yaml");
        if !path.exists() {
            return;
        }
        let layer = PersonalityLayer::load(path).expect("project personality YAML should parse");

        let prompt = layer.build_system_prompt_for(Some("what color is the shirt in this image?"));

        assert!(
            !prompt.contains("VISION MEASUREMENT DOMAIN"),
            "plain visual description/color queries should not pay for measurement guidance"
        );
    }

    #[test]
    fn load_missing_file_returns_file_not_found() {
        let path = Path::new("/nonexistent/path/personality.yaml");
        match PersonalityLayer::load(path) {
            Err(PersonalityError::FileNotFound(_)) => {} // expected
            other => panic!("expected FileNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn load_or_default_from_does_not_panic_on_missing_file() {
        // PersonalityLayer::load_or_default_from(PERSONALITY_CONFIG_PATH) uses the
        // default personality path which points to ../../config/personality/default.yaml
        // relative to the test CWD. Whether or not the file exists, this function must
        // not panic. (Phase 38: was load_or_default(); the bare-default wrapper was
        // removed when personality_path got wired through to config.)
        let layer = PersonalityLayer::load_or_default_from(PERSONALITY_CONFIG_PATH);
        // Regardless of whether we got the real file or defaults, we should have a name.
        assert!(!layer.profile().name.is_empty());
    }

    // ── Round 3 / T1.2: domain-block lazy loading ─────────────────────────────

    fn profile_with_domain(name: &str, triggers: &[&str], content: &str) -> PersonalityProfile {
        PersonalityProfile {
            name: default_name(),
            version: "1.0".to_string(),
            system_prompt_prefix: "core prompt".to_string(),
            tone_directives: Vec::new(),
            response_style: ResponseStyle::default(),
            anti_patterns: Vec::new(),
            domains: vec![DomainBlock {
                name: name.to_string(),
                triggers: triggers.iter().map(|t| t.to_string()).collect(),
                content: content.to_string(),
                anti_patterns: Vec::new(),
            }],
            lora_adapter_path: None,
        }
    }

    #[test]
    fn domain_block_not_loaded_when_no_query_hint() {
        let layer = PersonalityLayer {
            profile: profile_with_domain("imessage", &["imessage"], "IMSG BLOCK"),
        };
        let prompt = layer.build_system_prompt();
        assert!(
            !prompt.contains("IMSG BLOCK"),
            "domain block must not load without a query hint"
        );
    }

    #[test]
    fn domain_block_loads_on_trigger_match() {
        let layer = PersonalityLayer {
            profile: profile_with_domain("imessage", &["imessage", "text"], "IMSG BLOCK"),
        };
        let prompt = layer.build_system_prompt_for(Some("send an iMessage to Mom"));
        assert!(
            prompt.contains("IMSG BLOCK"),
            "domain must load when user query contains a trigger"
        );
    }

    #[test]
    fn domain_block_trigger_match_is_case_insensitive() {
        let layer = PersonalityLayer {
            profile: profile_with_domain("imessage", &["IMESSAGE"], "IMSG BLOCK"),
        };
        let prompt = layer.build_system_prompt_for(Some("send an imessage to mom"));
        assert!(
            prompt.contains("IMSG BLOCK"),
            "trigger matching must be case-insensitive"
        );
    }

    #[test]
    fn domain_block_does_not_load_on_unrelated_query() {
        let layer = PersonalityLayer {
            profile: profile_with_domain("imessage", &["imessage"], "IMSG BLOCK"),
        };
        let prompt = layer.build_system_prompt_for(Some("what time is it?"));
        assert!(
            !prompt.contains("IMSG BLOCK"),
            "domain must not load when no trigger matches"
        );
    }

    #[test]
    fn empty_trigger_is_ignored_rather_than_matching_everything() {
        // Guard against a misconfigured YAML where `triggers: [""]` would
        // otherwise match every query — that would defeat the whole point of T1.2.
        let layer = PersonalityLayer {
            profile: profile_with_domain("bug", &[""], "LEAK"),
        };
        let prompt = layer.build_system_prompt_for(Some("anything"));
        assert!(
            !prompt.contains("LEAK"),
            "empty triggers must not match — would cause unconditional injection"
        );
    }

    #[test]
    fn domain_anti_patterns_loaded_with_content() {
        let profile = PersonalityProfile {
            name: default_name(),
            version: "1.0".to_string(),
            system_prompt_prefix: "core".to_string(),
            tone_directives: Vec::new(),
            response_style: ResponseStyle::default(),
            anti_patterns: vec!["core AP".to_string()],
            domains: vec![DomainBlock {
                name: "imessage".to_string(),
                triggers: vec!["imessage".to_string()],
                content: "IMSG CONTENT".to_string(),
                anti_patterns: vec!["IMSG AP".to_string()],
            }],
            lora_adapter_path: None,
        };
        let layer = PersonalityLayer { profile };

        let with_match = layer.build_system_prompt_for(Some("send imessage"));
        let without_match = layer.build_system_prompt_for(Some("what time is it"));

        assert!(
            with_match.contains("IMSG AP"),
            "domain anti-pattern must appear when the domain matches"
        );
        assert!(
            without_match.contains("core AP"),
            "core anti-pattern must always appear"
        );
        assert!(
            !without_match.contains("IMSG AP"),
            "domain anti-pattern must NOT appear without a trigger match"
        );
    }

    #[test]
    fn matching_domain_names_reports_what_loaded() {
        let profile = PersonalityProfile {
            name: default_name(),
            version: "1.0".to_string(),
            system_prompt_prefix: "core".to_string(),
            tone_directives: Vec::new(),
            response_style: ResponseStyle::default(),
            anti_patterns: Vec::new(),
            domains: vec![
                DomainBlock {
                    name: "imessage".to_string(),
                    triggers: vec!["imessage".to_string()],
                    content: "A".to_string(),
                    anti_patterns: Vec::new(),
                },
                DomainBlock {
                    name: "download".to_string(),
                    triggers: vec!["yt-dlp".to_string(), "download".to_string()],
                    content: "B".to_string(),
                    anti_patterns: Vec::new(),
                },
            ],
            lora_adapter_path: None,
        };
        let layer = PersonalityLayer { profile };

        assert_eq!(
            layer.matching_domain_names("download that video"),
            vec!["download"]
        );
        assert_eq!(
            layer.matching_domain_names("what time is it"),
            Vec::<String>::new()
        );
        // Multi-match: both trigger.
        let mut both = layer.matching_domain_names("imessage me after the download");
        both.sort();
        assert_eq!(both, vec!["download".to_string(), "imessage".to_string()]);
    }

    #[test]
    fn apply_to_messages_for_injects_domain_content_into_system_prompt() {
        let layer = PersonalityLayer {
            profile: profile_with_domain("download", &["yt-dlp"], "YT DLP CONTENT"),
        };
        let messages = vec![Message::user("run yt-dlp on this".to_string())];

        let with_hint = layer.apply_to_messages_for(&messages, Some("run yt-dlp on this"));
        let without_hint = layer.apply_to_messages_for(&messages, None);

        assert!(
            with_hint[0].content.contains("YT DLP CONTENT"),
            "domain content must appear in system message when hint matches"
        );
        assert!(
            !without_hint[0].content.contains("YT DLP CONTENT"),
            "domain content must NOT appear when hint is None"
        );
    }

    #[test]
    fn build_system_prompt_preserves_backward_compatibility() {
        // Pre-T1.2 call sites use build_system_prompt() with no hint.
        // The result must match build_system_prompt_for(None) exactly —
        // guards against accidental drift between the two APIs.
        let layer = PersonalityLayer::with_defaults();
        assert_eq!(
            layer.build_system_prompt(),
            layer.build_system_prompt_for(None)
        );
    }

    #[test]
    fn load_malformed_yaml_returns_parse_error() {
        use std::io::Write;
        // Write a tempfile with invalid YAML.
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        // write_all with a byte literal avoids Rust format-string escaping entirely.
        // The content is structurally invalid YAML (unclosed bracket) so serde_yaml
        // must return a parse error regardless of field matching.
        tmp.write_all(b"name: [unclosed bracket invalid yaml")
            .expect("write");
        let path = tmp.path().to_owned();
        match PersonalityLayer::load(&path) {
            Err(PersonalityError::ParseError { .. }) => {} // expected
            other => panic!("expected ParseError, got: {other:?}"),
        }
    }
}
