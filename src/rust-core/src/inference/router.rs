/// ModelRouter and ConversationContext for Dexter's inference tier selection.
///
/// ## ModelRouter
///
/// Routes each request to the appropriate inference tier using two-stage deterministic routing:
///
/// Stage 1 — Category classification (rule-based keyword matching on the last REAL
/// operator message — `MessageOrigin::User`, not synthetic role="user" injections):
///   Chat   — default for conversational interaction and information requests
///   Code   — detected via programming-intent keywords in the last user message
///   Vision — detected via image/screen-reference keywords in the text ONLY; actual
///            image-attachment presence is validated downstream by the orchestrator,
///            which demotes Vision → Primary when no image lands on a user turn.
///   RetrievalFirst — detected via factual-currency signals (dates, versions, people)
///
/// Stage 2 — Complexity scoring (0–3, based on linguistic signals and message length):
///   0 = trivial   (single-concept, short, answer-obvious)
///   1 = simple    (clear-scope, single-concept, no deep reasoning required)
///   2 = moderate  (multi-part, explanation-required, some technical depth)
///   3 = deep      (multi-step reasoning, design/architecture, proof, full system)
///
/// Routing policy (from IMPLEMENTATION_PLAN.md §3.3):
///   code  + 0-1 → CODE
///   code  + 2-3 → PRIMARY (fallback: CODE)
///   vision      → VISION
///   chat  + 0-1 → FAST
///   chat  + 2   → PRIMARY
///   chat  + 3   → HEAVY
///
/// Every routing decision is logged with a structured reasoning string so operators can
/// see and tune the routing behavior without adding print statements.
///
/// ## Sticky follow-up inheritance (Phase 37.7)
///
/// The base classifier looks only at the last user message. That produces catastrophic
/// dumb-feeling misroutes when an operator says "implement a Rust parser" (Code) and
/// then follows up with "make it faster" (classifies as Chat on its own → FAST). The
/// follow-up is obviously still code; the router forgot.
///
/// After direct classification, `maybe_inherit_category` runs an asymmetric,
/// conservative sticky-inheritance pass:
///
///   - Inheritance ONLY activates when the direct classification is `Chat` AND the
///     current utterance is *ambiguous* (short, not a topic shift, contains a strong
///     continuation cue or a continuation-imperative).
///   - The inherited category is drawn from the most recent *direct* category signal
///     in prior user turns, walking backward. Chain breaks on either: a prior direct
///     non-Chat signal (→ source), or an explicit-Chat turn that is not itself
///     ambiguous (→ topic was already conversational, no inheritance).
///   - Hard cap: 3 consecutive inherited hops. On turn N+4 after a single direct
///     Code signal, ambiguity alone stops being sufficient — a fresh direct signal
///     is required. Prevents inheritance from becoming self-propagating sludge.
///   - Provenance is logged on `RoutingDecision.inherited_category` and threaded into
///     the reasoning string ONLY when inheritance materially changes the outcome. No-op
///     inheritance (Chat→Chat) is suppressed to keep logs signal-dense.
///
/// ## ConversationContext
///
/// Maintains the message history for a session. Provides deterministic truncation that
/// always preserves:
///   - The system message at index 0 (the personality prompt)
///   - The last `max_turns` user+assistant exchange pairs
///
/// Truncation drops the oldest pairs first. A pair is always dropped atomically (never
/// just the user or just the assistant turn) to prevent models that enforce strict
/// role-alternation from getting confused by orphaned messages.
use tracing::{debug, info};

use super::engine::{Message, MessageOrigin};
use super::models::ModelId;

// ── Category ─────────────────────────────────────────────────────────────────

/// The semantic category of a request, used in stage 1 of routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Category {
    /// General conversational interaction, information requests, Q&A.
    /// Routes to FAST or PRIMARY depending on complexity.
    Chat,

    /// Code generation, review, refactor, debugging, test writing.
    /// Routes to CODE or PRIMARY depending on complexity.
    Code,

    /// Image or screenshot interpretation. Routes to VISION — but only conditionally.
    ///
    /// IMPORTANT: the router classifies textual intent ONLY. It has no access to
    /// attachment metadata and cannot tell whether an image is actually present.
    /// The orchestrator is responsible for (a) capturing the screen after the router
    /// returns Vision and attaching the image to the last real user turn, and
    /// (b) demoting Vision → Primary with an explicit log line when no image lands.
    /// See orchestrator.rs "Vision demotion" for that enforcement. If you're reading
    /// this and thinking "surely the router guards against no-image vision" — it
    /// does not, and cannot, without attachment state being passed in.
    Vision,

    /// Query type where the model is likely to hallucinate (current dates, versions,
    /// recent events, people's current roles). Routes to PRIMARY with retrieval context.
    /// The retrieval pipeline (Phase 9) injects results before generation.
    RetrievalFirst,
}

// ── Complexity ────────────────────────────────────────────────────────────────

/// Estimated reasoning depth of a request, 0–3.
///
/// Used in stage 2 of routing to decide between FAST/PRIMARY/HEAVY for Chat
/// and between CODE/PRIMARY for Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Complexity(pub u8);

impl Complexity {
    pub const TRIVIAL: Self = Self(0);
    /// Phase 37.5 / B4: no longer referenced in the routing table match arms
    /// (Chat/complexity=1 is now folded into the `≤ MODERATE` bucket), but kept
    /// as part of the public 0–3 scale documentation for tests, future tuning,
    /// and external callers reasoning about complexity values.
    #[allow(dead_code)]
    pub const SIMPLE: Self = Self(1);
    pub const MODERATE: Self = Self(2);
    pub const DEEP: Self = Self(3);

    pub fn value(&self) -> u8 {
        self.0
    }
}

// ── RoutingDecision ───────────────────────────────────────────────────────────

/// The output of `ModelRouter::route()`.
///
/// Contains everything needed to log and understand why a particular model was chosen.
/// The `reasoning` string is written to the structured log on every call — operators
/// can tune routing behavior by watching these logs and adjusting the keyword sets
/// or complexity thresholds.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// The primary model tier selected.
    pub model: ModelId,

    /// Detected category from stage 1.
    pub category: Category,

    /// Complexity score from stage 2.
    pub complexity: Complexity,

    /// Human-readable explanation of the routing decision.
    /// Written to tracing logs. Format: "category=code complexity=2 → PRIMARY (fallback: CODE)"
    pub reasoning: String,

    /// Fallback model if the primary tier is unavailable.
    /// Only set when the primary choice is a heavier model that might not be loaded.
    #[allow(dead_code)] // Phase 7 — availability checks before model dispatch
    pub fallback: Option<ModelId>,

    /// Provenance marker for sticky inheritance.
    ///
    /// `Some(cat)` means the final `category` was NOT produced by direct classification
    /// of the current utterance — it was inherited from a prior turn because the current
    /// utterance was ambiguous/underspecified. The value is the inherited category (same
    /// as `category`, duplicated for clarity in structured logs).
    ///
    /// `None` means direct classification produced the final category. No-op inheritance
    /// (direct=Chat, inherited=Chat, no change) is NOT logged here — the field records
    /// *causal deltas*, not possibilities. If you see `inherited_category: None` in logs,
    /// the classifier looked at the utterance alone and was confident.
    pub inherited_category: Option<Category>,
}

// ── ModelRouter ───────────────────────────────────────────────────────────────

/// Stateless, deterministic tier selector.
///
/// `ModelRouter` has no fields. Every routing decision is a pure function of the
/// message list passed to `route()`. This makes it trivially unit-testable with
/// fixtures and allows it to be used without synchronization from any async context.
///
/// The routing algorithm is keyword-based in v1. A future phase will replace the
/// category classifier with a small local model (e.g., Phi-3 Mini) while keeping
/// the same `route()` API contract intact.
#[derive(Debug, Default)]
pub struct ModelRouter;

impl ModelRouter {
    #[allow(dead_code)] // Explicit constructor — `ModelRouter` (ZST) is also constructible via `Default`
    pub fn new() -> Self {
        Self
    }

    /// Route the current request to an inference tier.
    ///
    /// Examines the last user message in `messages` as the primary signal.
    /// Earlier turns provide context for complexity scoring but do not override
    /// category classification.
    ///
    /// Returns a `RoutingDecision` with the selected `ModelId`, the intermediate
    /// classification results, and a human-readable reasoning string for logging.
    pub fn route(&self, messages: &[Message]) -> RoutingDecision {
        // Phase 37.7: classify on the last REAL operator message, not the last
        // role=="user" message. Tool results and retrieval injections serialize
        // as role="user" for Ollama compatibility (per MessageOrigin docs), so
        // walking `role == "user"` would let synthetic content poison direct
        // classification whenever a tool result happened to be the most recent
        // role="user" message in the buffer.
        let last_user_text: &str = messages
            .iter()
            .rev()
            .find(|m| m.origin == MessageOrigin::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");

        let direct_category = self.classify_category(last_user_text);
        let complexity = self.score_complexity(last_user_text);

        // Sticky-inheritance pass: only considered when direct classification is Chat.
        // If the current utterance carries its own strong signal (Code/Vision/Retrieval),
        // inheritance never fires — direct wins. This is the asymmetry: inheritance only
        // fills in for ambiguity, never overrides a confident direct read.
        let inherited_category =
            self.maybe_inherit_category(direct_category.clone(), last_user_text, messages);

        // Resolved category: inherited if inheritance fired, otherwise direct.
        let category = inherited_category
            .clone()
            .unwrap_or_else(|| direct_category.clone());

        let (model, fallback, base_reasoning) = self.select_model(&category, complexity);

        // Thread inheritance provenance into the reasoning string ONLY when it
        // materially changed the outcome (inherited != direct). No-op inheritance
        // is suppressed here and on the `inherited_category` field — logs record
        // causal deltas, not possibilities.
        let reasoning = if let Some(ref inh) = inherited_category {
            format!(
                "{base_reasoning} (inherited from prior turn: direct={:?} → {:?})",
                direct_category, inh
            )
        } else {
            base_reasoning
        };

        debug!(
            ?model,
            ?category,
            inherited_category = ?inherited_category,
            complexity         = complexity.value(),
            reasoning          = %reasoning,
            "Routing decision"
        );

        // Phase 37.7 diagnostic: emit the exact text the classifier scored, so
        // discrepancies between operator-typed input and router-observed input
        // (clipboard concat, context injection, Swift-side preprocessing) can
        // be caught without guessing. Truncated to 200 chars to keep log lines
        // bounded; char_count surfaces length inflation separately.
        info!(
            last_user_text_preview = %last_user_text.chars().take(200).collect::<String>(),
            last_user_text_chars   = last_user_text.chars().count(),
            direct_category        = ?direct_category,
            "Router input snapshot"
        );

        RoutingDecision {
            model,
            category,
            complexity,
            reasoning,
            fallback,
            inherited_category,
        }
    }

    // ── Sticky inheritance (Phase 37.7) ───────────────────────────────────────

    /// Decide whether the current turn should inherit category from a prior turn.
    ///
    /// Returns `Some(cat)` ONLY when inheritance materially changes the routed
    /// category — i.e. direct classification was Chat and a prior turn carried a
    /// non-Chat direct signal within the hop cap. Returns `None` in all other
    /// cases (direct signal strong enough, not ambiguous, topic shift, cap
    /// exceeded, no eligible source).
    fn maybe_inherit_category(
        &self,
        direct_category: Category,
        current_utterance: &str,
        messages: &[Message],
    ) -> Option<Category> {
        // Gate 1: inheritance only fills in when the direct read is Chat.
        // A directly-classified Code/Vision/Retrieval turn wins outright.
        if direct_category != Category::Chat {
            return None;
        }

        // Gate 2: topic-shift denylist kills inheritance regardless of shape.
        // Even a short "by the way, fix this" would pass the strong-cue test
        // ("fix") but should clearly NOT inherit the prior category.
        let lower = current_utterance.to_lowercase();
        if is_topic_shift(&lower) {
            return None;
        }

        // Gate 3: ambiguity predicate. The utterance must be short AND carry
        // a continuation signal (strong cue, or weak cue + very short).
        if !is_ambiguous_followup(current_utterance, &lower) {
            return None;
        }

        // Gate 4: walk backward through prior REAL operator turns to find the
        // most recent direct non-Chat signal. Phase 37.7: filter by `origin`,
        // not `role` — tool results and retrieval injections (which serialize
        // as role="user") must not participate in the walk. If they did, a
        // synthetic retrieved sentence could silently break inheritance by
        // classifying as non-ambiguous Chat, or a tool-result text could be
        // mistaken for the source of an inherited category.
        let prior_user_texts: Vec<&str> = messages
            .iter()
            .filter(|m| m.origin == MessageOrigin::User)
            .map(|m| m.content.as_str())
            .collect();

        // Exclude the current utterance (last in the list) from the walk.
        // If there are fewer than 2 real user messages, there's nothing to inherit from.
        if prior_user_texts.len() < 2 {
            return None;
        }
        let predecessors = &prior_user_texts[..prior_user_texts.len() - 1];

        // Count ambiguous predecessor operator turns between the current turn
        // and the last direct non-Chat source. Does NOT count the current
        // utterance itself. Cap semantics: the cap is on how many ambiguous
        // predecessors may stand between the current turn and its source —
        // so `MAX_INHERITED_HOPS = 3` permits turns N+1, N+2, N+3 after a
        // direct signal at turn N to inherit (each having 0, 1, 2 ambiguous
        // predecessors respectively). Turn N+4 has 3 ambiguous predecessors
        // and fails the `>= MAX_INHERITED_HOPS` check.
        let mut ambiguous_predecessor_count: usize = 0;
        for prior in predecessors.iter().rev() {
            let prior_lower = prior.to_lowercase();
            let prior_direct = self.classify_category(prior);

            if prior_direct != Category::Chat {
                // Found the source. If there are already >= MAX ambiguous
                // predecessors between source and current, refuse.
                if ambiguous_predecessor_count >= MAX_INHERITED_HOPS {
                    return None;
                }
                return Some(prior_direct);
            }

            // Prior was directly Chat. Two sub-cases:
            //   (a) prior was itself ambiguous → it was likely also an inherited
            //       turn; keep walking, count it as an ambiguous predecessor.
            //   (b) prior was NOT ambiguous → it was an explicit Chat turn;
            //       the chain is broken by a real topic, no inheritance.
            //
            // Note: limitation of the stateless walk — a prior Chat turn that
            // happened to contain a strong continuation cue ("why?") is
            // indistinguishable from one that was itself inheriting. The
            // conservative policy is to treat ambiguous-looking prior turns
            // as chain members. If this ever bites, the fix is to store prior
            // RoutingDecision on the session rather than re-derive from text.
            if is_topic_shift(&prior_lower) || !is_ambiguous_followup(prior, &prior_lower) {
                return None;
            }
            ambiguous_predecessor_count += 1;
        }

        // Walked off the start of the conversation without finding a direct signal.
        None
    }

    // ── Stage 1: Category classification ─────────────────────────────────────

    fn classify_category(&self, text: &str) -> Category {
        let lower = text.to_lowercase();

        // Vision signals — checked first because some vision queries can overlap with
        // code (e.g., "look at this screenshot of a compile error").
        // Phase 20: extended with natural-language screen-reference phrases that users
        // say when they want Dexter to look at what's on the display.
        if contains_any(
            &lower,
            &[
                "screenshot",
                "image",
                "picture",
                "photo",
                "look at this",
                "what do you see",
                "what's on screen",
                "on the screen",
                "see my screen",
                "look at the screen",
                "look at my screen",
            ],
        ) {
            return Category::Vision;
        }

        // Retrieval-first signals — queries where the model's training data is likely
        // stale or the answer changes frequently. Route to retrieval before generation.
        //
        // Phase 37.7: expanded from the original narrow set ("current version of",
        // "latest version of", etc.) to cover weather, current office-holders, markets,
        // sports, and release notes. Bare `"latest"` / `"current"` / `"today"` are
        // intentionally NOT added — too many false positives ("latest update to my
        // story", "current focus of the meeting", "today is my birthday").
        //
        // NOTE: "what time is it" / "what day is it" are intentionally excluded.
        // Those are system-state queries the model can answer via a `date` shell action —
        // routing them to DuckDuckGo returns useless results. They fall through to Chat/FAST.
        if contains_any(
            &lower,
            &[
                // Versions / releases
                "current version of",
                "latest version of",
                "what is the current",
                "what's the current",
                "latest release",
                "release notes",
                "what changed in",
                "ships with",
                // News / events
                "what are the latest",
                "recent news",
                "breaking news",
                "happening today",
                "happening now",
                // Weather
                "today's weather",
                "weather today",
                "weather forecast",
                "what's the weather",
                "what is the weather",
                // Office-holders / leadership
                "current price of",
                "who is the current",
                "current ceo",
                "current president",
                "current leader",
                "who runs",
                "who leads",
                // Markets
                "stock price of",
                "share price of",
                "exchange rate",
                // Sports
                "sports score",
                "game score",
                "game tonight",
                "standings",
                "who won",
            ],
        ) {
            return Category::RetrievalFirst;
        }

        // Code signals — intent-to-program markers.
        // Order matters: "implement a function" and "debug this error" are unambiguous;
        // "error" alone is too broad (operator might just say "there's an error in my code").
        //
        // Phase 37.7 overhaul: removed "rewrite " (false-positive on "rewrite this
        // paragraph"). Added code-fences, file extensions, syntax markers, toolchain
        // terms, error-shape patterns, and API/system-design phrases. These domain
        // signals are far more robust than English verbs — `fn `, `.rs`, `cargo`,
        // and ``` almost never appear in non-code conversation.
        if contains_any(
            &lower,
            &[
                // Direct programming intent
                "implement ",
                "write a function",
                "write a class",
                "write a struct",
                "write a test",
                "write unit test",
                "write integration test",
                "refactor ",
                "write the code",
                "generate code",
                "code this",
                "code the ",
                "debug this",
                "fix this bug",
                "fix the bug",
                "fix this error",
                "fix this warning",
                "fix this test",
                "compile error",
                "runtime error",
                "type error",
                "syntax error",
                "this function",
                "this method",
                "this class",
                "this struct",
                "update this function",
                "add a function",
                "add a method",
                "how do i implement",
                "how do i write",
                // Code fences (unambiguous programming context)
                "```",
                // File extensions — space/punctuation-delimited to avoid false matches
                // inside unrelated words. `.rs `, `.rs,`, `.rs.`, `.rs?` etc.
                ".rs ",
                ".rs,",
                ".rs.",
                ".rs?",
                ".rs:",
                ".rs\n",
                ".py ",
                ".py,",
                ".py.",
                ".py?",
                ".py:",
                ".py\n",
                ".ts ",
                ".ts,",
                ".ts.",
                ".ts?",
                ".ts:",
                ".ts\n",
                ".tsx",
                ".jsx",
                ".js ",
                ".js,",
                ".js.",
                ".js?",
                ".js:",
                ".js\n",
                ".go ",
                ".go,",
                ".go.",
                ".go?",
                ".go:",
                ".go\n",
                ".swift",
                ".cpp",
                ".hpp",
                ".java ",
                ".kt ",
                ".rb ",
                ".scala",
                // Language-specific syntax markers. Deliberately pruned: bare
                // "class ", "interface ", "trait " are dropped because they're
                // common in non-code English ("world class", "user interface",
                // "personality trait"). Keeping only markers that are either
                // symbol-flavored ("pub fn", "async fn") or rare enough in prose
                // ("struct ", "enum ", "impl ") to be safe.
                "fn ",
                "def ",
                "impl ",
                "pub fn",
                "async fn",
                "struct ",
                "enum ",
                "func ",
                // Toolchain / compilers / package managers — qualified patterns only.
                // Bare "cargo ", "make ", "ld ", "yarn ", "vite ", "gcc ", "clang ",
                // "tsc ", "npm ", "pnpm " all had catastrophic false-positive rates
                // ("cargo ship", "make it faster", "old " matching "ld ", "knitting
                // yarn", "invite ") so each is replaced with the command-shaped
                // pattern Dexter operators would actually type.
                "cargo build",
                "cargo run",
                "cargo check",
                "cargo test",
                "cargo install",
                "cargo doc",
                "cargo.toml",
                "cargo add",
                "rustc ",
                "rustup ",
                "npm install",
                "npm run",
                "npm test",
                "npm start",
                "package.json",
                "yarn add",
                "yarn install",
                "pnpm install",
                "pnpm add",
                "pip install",
                "tsconfig.json",
                "webpack.config",
                "vitest",
                "jest ",
                // Error-shape patterns
                "won't compile",
                "wont compile",
                "won't build",
                "wont build",
                "failing build",
                "build fails",
                "cargo check fails",
                "cargo build fails",
                "panicking",
                "panics at",
                "panic!",
                "stack trace",
                "traceback",
                "segfault",
                "segmentation fault",
                "null pointer",
                "nullpointerexception",
                "undefined is not a function",
                // API / system-design intent (these used to miss the Code classifier
                // and fall through to Chat — design a REST API is clearly Code work)
                "design a rest api",
                "design an api",
                "design the api",
                "rest api",
                "graphql api",
                "api endpoint",
                "database schema",
                "data model ",
                "data schema",
                "microservice",
                "backend service",
                // Phase 37.9 / T4 fix: per-language "<lang> function/class/struct"
                // patterns. Live smoke T4 — "write a Rust function that uses rayon's
                // parallel iterator to compute prime counts in a range" — missed
                // "write a function" because the infix "Rust" broke contiguity
                // (contains_any is substring-based, not token-based). Enumerating
                // <lang>+<code-noun> pairs catches this class of query without a
                // tokenizer refactor. "rust " alone is too broad (oxidation false
                // positives); "<lang> function/class/…" is safe.
                "rust function",
                "rust class",
                "rust struct",
                "rust enum",
                "rust trait",
                "rust macro",
                "python function",
                "python class",
                "python script",
                "python method",
                "go function",
                "golang function",
                "swift function",
                "swift class",
                "swift struct",
                "typescript function",
                "typescript class",
                "javascript function",
                "javascript class",
                "java function",
                "java class",
                "java method",
                "c++ function",
                "c++ class",
                "kotlin function",
                "kotlin class",
                "ruby function",
                "ruby class",
                "ruby method",
                // Rust crate signals — survive operator typos in the word
                // "function" (the T4 query had "funcrion"). "use rayon" / "rayon::"
                // etc. are specific enough to not match "crayon" (which contains
                // "rayon" as a substring but never appears next to "use" or "::").
                "use rayon",
                "rayon::",
                "use tokio",
                "tokio::",
                "use serde",
                "serde::",
            ],
        ) {
            return Category::Code;
        }

        // Default: conversational.
        Category::Chat
    }

    // ── Stage 2: Complexity scoring ───────────────────────────────────────────

    fn score_complexity(&self, last_user_text: &str) -> Complexity {
        let lower = last_user_text.to_lowercase();
        let mut score: u8 = 0;

        // ── Length heuristics ───────────────────────────────────────────────
        // Longer messages correlate with more complex requests. These are soft signals;
        // keyword analysis (below) can override them.
        //
        // Phase 37.7: use `.chars().count()` (not `.len()`) so non-ASCII text
        // (em-dashes, accented chars, CJK, emoji) isn't inflated by the byte-per-codepoint
        // gap. A 250-char question in English and a 250-char question in Japanese should
        // get the same length score.
        let char_count = last_user_text.chars().count();
        if char_count > 800 {
            score = score.saturating_add(2);
        } else if char_count > 250 {
            score = score.saturating_add(1);
        }

        // ── Strong DEEP signals (immediate complexity=3) ──────────────────────
        // These phrases are direct, unambiguous requests for extensive multi-step
        // reasoning. A single match is strong enough to skip further scoring and
        // route to HEAVY. Phase 37.7: narrowed from the original list — phrases like
        // "from scratch" and "compare and contrast" were moved to the weak tier
        // because they attach to trivial requests ("hello world from scratch")
        // where HEAVY is massive overkill.
        //
        // Phase 37.9 / T2 fix: added "walk through" and "step of" paraphrases.
        // Live smoke T2 — "walk through every step an attacker would use to persist
        // on a hardened macOS host" — scored complexity 0 because none of the
        // existing DEEP phrases matched. "walk through every step" is a direct
        // paraphrase of "step by step" and a classic operator framing for
        // multi-step reasoning. Same story for "every step of the attack chain",
        // "each step of the process", and "one step at a time".
        if contains_any(
            &lower,
            &[
                "step by step",
                "think step by step",
                "walk me through",
                "walk through every",
                "walk through each",
                "walk through the",
                "every step of",
                "each step of",
                "one step at a time",
                "prove that",
                "prove why",
                "prove this",
                "formal proof",
                "mathematical proof",
                "derive the",
                "reason about",
                "think through",
                "design an architecture",
                "architect a system",
                "system design",
                "end to end",
                "deep analysis",
                "thoroughly analyze",
                "in-depth analysis",
            ],
        ) {
            return Complexity::DEEP;
        }

        // ── Weak DEEP signals (+2 to score, NOT early return) ─────────────────
        // Phase 37.7: phrases that often indicate deep work but attach to trivial
        // requests too often to trust unilaterally. A weak-deep match on a short
        // utterance lands at complexity=2 (PRIMARY, not HEAVY); a weak-deep match
        // on a long utterance or alongside a moderate signal escalates to 3.
        //
        // Example: "hello world from scratch" (< 250 chars, no other signals)
        //   → +2 = complexity 2 → PRIMARY. Not HEAVY — correct.
        // Example: "implement a payment processor from scratch in Rust with
        //   distributed transactions and audit logging" (> 250 chars)
        //   → +1 (length) +2 (weak-deep) = 3 → HEAVY. Correct.
        if contains_any(
            &lower,
            &["from scratch", "compare and contrast", "full system"],
        ) {
            score = score.saturating_add(2);
        }

        // ── Moderate-reasoning signals (medium weight: +1 each) ─────────────
        // Phase 37.7: removed bare "compare " / "contrast " — these are already
        // captured by the weak-DEEP "compare and contrast" signal above, and
        // having both fire on the same utterance double-counted ("compare and
        // contrast tabs vs spaces" was landing at complexity 3 → HEAVY when it
        // should be 2 → PRIMARY). "compare Rust and Go" without "and contrast"
        // is typically short enough to stay at complexity 0 → FAST on its own,
        // which is the right default.
        if contains_any(
            &lower,
            &[
                "explain how",
                "explain why",
                "explain what",
                "explain the",
                "how does",
                "why does",
                "how do",
                "why do",
                "analyze ",
                "analyse ",
                "design ",
                "plan ",
                "outline ",
                "structure ",
                "trade-off",
                "tradeoff",
                "pros and cons",
                "multiple",
                "several",
                "various",
                "each of",
                "in detail",
                "in depth",
                // Phase 37.7: literal-phrase escape hatches for common operator
                // variants that don't match "in depth" or "in detail" as substrings.
                // The ret2libc test case — "explain in technical depth how …" —
                // missed both "explain how" (non-adjacent) and "in depth"
                // (non-contiguous: "in _technical_ depth") and scored 0. These
                // entries catch the variants directly.
                "in technical depth",
                "in technical detail",
                "technically how",
                "technically why",
            ],
        ) {
            score = score.saturating_add(1);
        }

        // ── Multi-file / systemic code signals ───────────────────────────────
        // These are specific to Code category but raise complexity regardless.
        if contains_any(
            &lower,
            &[
                "across multiple files",
                "entire codebase",
                "all files",
                "full implementation",
                "complete implementation",
                "production-ready",
                "production ready",
            ],
        ) {
            score = score.saturating_add(1);
        }

        // ── Conversation depth ────────────────────────────────────────────────
        // Phase 37.7: REMOVED the "user_turn_count > 10 → +1 complexity" bump.
        // Accumulated conversation length is not the same thing as semantic
        // reasoning depth — the previous rule silently upgraded routine
        // follow-ups in long casual threads to PRIMARY/HEAVY for no substantive
        // reason. If context-load ever needs to influence routing, it should be
        // a separate axis, not mutated into the semantic complexity score.

        Complexity(score.min(3))
    }

    // ── Tier selection (the routing policy table) ─────────────────────────────

    fn select_model(
        &self,
        category: &Category,
        complexity: Complexity,
    ) -> (ModelId, Option<ModelId>, String) {
        match (category, complexity) {

            // ── Vision ─────────────────────────────────────────────────────────
            (Category::Vision, _) => (
                ModelId::Vision,
                None,
                format!("category=vision → VISION (complexity={} irrelevant; attachment validated by orchestrator, demotes to PRIMARY on no-image)",
                        complexity.value()),
            ),

            // ── Code ───────────────────────────────────────────────────────────
            // Phase 37.5 / B3: all code-category queries route to CODE regardless
            // of complexity. The previous split (complexity ≥ 2 → PRIMARY) was
            // written when PRIMARY was assumed to be the smarter generalist —
            // but the operator explicitly said "write code," so the programming
            // specialist (deepseek-coder-v2:16b, trained on Git/StackOverflow
            // corpora) is the right tool even for multi-file / architectural
            // code tasks. Domain specialization beats general capability on the
            // thing the specialist was trained for. A complex-code request that
            // also needs reasoning over non-code context (rare) can be escalated
            // via retrieval or by the operator explicitly asking a chat-shaped
            // follow-up.
            (Category::Code, c) => (
                ModelId::Code,
                None,
                format!("category=code complexity={} → CODE", c.value()),
            ),

            // ── RetrievalFirst ──────────────────────────────────────────────────
            // Route to PRIMARY: retrieval results inject facts into context,
            // and PRIMARY handles synthesis better than FAST.
            // Phase 9 will hook the retrieval pipeline before this model runs.
            (Category::RetrievalFirst, _) => (
                ModelId::Primary,
                Some(ModelId::Fast),
                format!("category=retrieval_first complexity={} → PRIMARY (retrieval context injected by Phase 9)",
                        complexity.value()),
            ),

            // ── Chat ────────────────────────────────────────────────────────────
            // Phase 37.5 / B4: shift the FAST→PRIMARY boundary down one notch.
            // Previously (Chat, 0|1) → FAST; (Chat, 2) → PRIMARY. In live testing
            // substantive questions with exactly one moderate-reasoning signal
            // ("explain how X", "analyze Y") scored complexity=1 and landed on
            // FAST (qwen3:8b), producing visibly shallow answers. A single
            // reasoning signal is sufficient evidence that FAST will underperform.
            //
            // Under the new table:
            //   complexity 0 → FAST     (trivial: jokes, yes/no, "what time")
            //   complexity 1 → PRIMARY  (any moderate signal or 250+ char query)
            //   complexity 2 → PRIMARY  (multiple moderate signals)
            //   complexity 3 → HEAVY    (explicit deep-reasoning escalation)
            //
            // FAST retains its role as the latency tier for genuinely
            // context-free questions, but stops being the default chat target.
            (Category::Chat, Complexity::TRIVIAL) => (
                ModelId::Fast,
                None,
                format!("category=chat complexity=0 → FAST"),
            ),
            (Category::Chat, c) if c <= Complexity::MODERATE => (
                ModelId::Primary,
                Some(ModelId::Fast),
                format!("category=chat complexity={} (≥1) → PRIMARY (fallback: FAST)", c.value()),
            ),
            (Category::Chat, _) => (
                ModelId::Heavy,
                Some(ModelId::Primary),
                format!("category=chat complexity=3 → HEAVY (explicit deep-reasoning escalation; fallback: PRIMARY)"),
            ),
        }
    }
}

// ── ConversationContext ───────────────────────────────────────────────────────

/// Session message buffer with deterministic truncation.
///
/// Maintains the full message history for one conversation session. When the history
/// exceeds `max_turns`, the oldest user+assistant pairs are dropped. The system message
/// (always at index 0) is never dropped — it carries the personality prompt that must
/// be present on every inference call.
///
/// Typical call pattern:
/// ```ignore
/// let mut ctx = ConversationContext::new(session_id, 20);
/// ctx.push_user("What's 2+2?");
/// // ... call InferenceEngine with personality.apply_to_messages(ctx.messages()) ...
/// ctx.push_assistant("4.");
/// ```
#[derive(Debug, Clone)]
pub struct ConversationContext {
    session_id: String,
    /// The raw message buffer. Index 0 is always a system message if one was set via
    /// `set_system_message()`. User and assistant messages follow in order.
    messages: Vec<Message>,
    /// Maximum number of user+assistant exchange pairs to retain.
    /// System messages don't count toward this limit.
    max_turns: usize,
}

impl ConversationContext {
    /// Create a new empty context with no system message and the given turn limit.
    ///
    /// Typical values: `CONVERSATION_MAX_TURNS` (4, for the orchestrator's live
    /// context — chosen to preserve KV-cache stability across turns) or higher
    /// for tests that exercise the trimming logic.
    ///
    /// `max_turns` counts user+assistant *pairs*. System messages don't count
    /// and are never evicted. Older turns are dropped in pair order once the
    /// limit is exceeded; see `trim_to_max_turns`.
    pub fn new(session_id: impl Into<String>, max_turns: usize) -> Self {
        Self {
            session_id: session_id.into(),
            messages: Vec::new(),
            max_turns,
        }
    }

    /// Return a reference to the full message buffer.
    ///
    /// The PersonalityLayer wraps this before passing to InferenceEngine:
    /// `personality.apply_to_messages(ctx.messages())`
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Number of messages in the buffer (including any system message).
    #[allow(dead_code)] // Phase 7 — context observer uses this for context-window management
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Number of real operator turns in the buffer.
    ///
    /// Phase 37.7: counts `MessageOrigin::User` only — NOT synthetic injections
    /// that serialize as role="user" (tool results, retrieval). The previous
    /// implementation counted all `role == "user"` messages, which silently
    /// inflated the turn count with synthetic messages and caused `max_turns`
    /// trimming to evict real history prematurely.
    pub fn turn_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.origin == MessageOrigin::User)
            .count()
    }

    /// Session identifier. Logged on every inference call for tracing.
    #[allow(dead_code)] // Phase 7 — context observer attaches session_id to all emitted events
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Push a user message and trim history if over `max_turns`.
    pub fn push_user(&mut self, content: impl Into<String>) {
        self.messages.push(Message::user(content));
        self.trim_if_needed();
    }

    /// Push an assistant response message.
    ///
    /// Should always follow a corresponding `push_user()` call. The context does not
    /// enforce this contract (the orchestrator does) but the truncation algorithm
    /// assumes pairs and may produce an orphaned assistant message if this invariant
    /// is violated.
    pub fn push_assistant(&mut self, content: impl Into<String>) {
        self.messages.push(Message::assistant(content));
    }

    /// Replace the system message. If a system message already exists at index 0,
    /// it is replaced. Otherwise one is prepended.
    ///
    /// The system message does not count toward `max_turns` and is never truncated.
    /// Calling this mid-conversation is valid — the orchestrator may update the system
    /// message to inject retrieved facts.
    #[allow(dead_code)] // Phase 9 — retrieval pipeline injects retrieved facts into system message
    pub fn set_system_message(&mut self, content: impl Into<String>) {
        if self
            .messages
            .first()
            .map(|m| m.role == "system")
            .unwrap_or(false)
        {
            self.messages[0] = Message::system(content);
        } else {
            self.messages.insert(0, Message::system(content));
        }
    }

    /// Inject a tool result into the conversation as a synthetic user message.
    ///
    /// ## Why role="user"
    ///
    /// Ollama's `/api/chat` contract accepts only `"system"`, `"user"`, and `"assistant"`
    /// roles for models without native tool-calling support (qwen3:8b base, llama3, etc.).
    /// Custom roles like `"tool"` or `"retrieval"` are **silently dropped** by such models:
    /// they may appear in the request payload but never influence generation. That caused
    /// Round 3 test failures where sqlite3 results, retrieval injections, and
    /// `(normalized to macOS BSD: X)` annotations on tool results were invisible to the
    /// model — it would then fall back to its own prior assistant prose when asked
    /// "what did you do?", producing confidently wrong answers.
    ///
    /// Injecting tool output as a `"user"` message with an explicit `[Action result]` /
    /// `[Retrieved]` prefix in the content gives three guarantees:
    ///   1. Every model sees the content (universal role).
    ///   2. The prefix distinguishes synthetic injections from real operator input, so
    ///      the model won't confuse them when summarizing conversation.
    ///   3. Turn alternation remains valid (assistant → user → assistant), which some
    ///      chat templates enforce strictly.
    ///
    /// ## Truncation
    ///
    /// Does NOT call `trim_if_needed()` — these synthetic "user" messages represent
    /// tool-result injections, not operator turns, and must not be counted toward
    /// the `max_turns` budget or evicted by it.
    pub fn push_tool_result(&mut self, content: &str) {
        // Phase 37.7: use `Message::tool_result` so the pushed message carries
        // `MessageOrigin::ToolResult`. `turn_count()` skips these, so they can
        // accumulate without evicting real operator history.
        self.messages.push(Message::tool_result(content));
    }

    /// Clear all messages except the system message (if any).
    ///
    /// Used when starting a new conversation topic while preserving identity context.
    #[allow(dead_code)] // Phase 7 — explicit context reset on operator command
    pub fn clear_history(&mut self) {
        if self
            .messages
            .first()
            .map(|m| m.role == "system")
            .unwrap_or(false)
        {
            self.messages.truncate(1);
        } else {
            self.messages.clear();
        }
    }

    // ── Truncation ────────────────────────────────────────────────────────────

    /// Drop oldest turns until `turn_count() <= max_turns`.
    ///
    /// ## Policy: a "turn" is one real operator message PLUS all following
    /// non-user messages up to the next real operator message.
    ///
    /// That means the trimmer evicts, as a single atomic unit:
    ///   - the oldest `MessageOrigin::User` message
    ///   - the assistant response that follows it
    ///   - any tool-result injections produced while answering it
    ///   - any retrieval injections attached to it
    ///
    /// This is a stronger semantic commitment than the pre-Phase-37.7 code,
    /// which only tracked user+assistant pairs. If a future phase introduces
    /// synthetic context messages that should persist across trims
    /// independently of turn ownership (e.g. a global preference injection),
    /// this logic will evict them along with their associated user turn —
    /// which is wrong for that case. Either such messages need to live in the
    /// system-message area (never trimmed) or the trimmer needs a new
    /// "persist-across-trims" origin flag.
    ///
    /// Pairs are always dropped atomically.
    fn trim_if_needed(&mut self) {
        while self.turn_count() > self.max_turns {
            // Phase 37.7: find the first REAL user message (Origin::User) and
            // drop it plus any immediately-following assistant/tool-result/retrieval
            // messages that belong to the same turn. Previously this code assumed
            // the first non-system message was always a user message — a fragile
            // invariant that would break the moment a retrieval injection landed
            // between the system prompt and the first user turn.
            let user_idx = match self
                .messages
                .iter()
                .position(|m| m.origin == MessageOrigin::User)
            {
                Some(idx) => idx,
                None => break, // No real user turn to trim.
            };

            self.messages.remove(user_idx);

            // Drop contiguous non-User messages that follow (the assistant response
            // and any tool-result / retrieval injections produced while answering
            // this turn). Stop at the next real user turn or the end of the buffer.
            //
            // This is the correct pairing behavior: a turn is "one operator message
            // + the model's response and any tool outputs generated for it." All
            // of those belong together and must be evicted as a unit.
            while let Some(m) = self.messages.get(user_idx) {
                if m.origin == MessageOrigin::User {
                    break;
                }
                self.messages.remove(user_idx);
            }

            info!(
                session_id = %self.session_id,
                turn_count = self.turn_count(),
                max_turns  = self.max_turns,
                "Trimmed oldest conversation turn (max_turns limit reached)"
            );
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> ModelRouter {
        ModelRouter::new()
    }

    fn user_msg(text: &str) -> Message {
        Message::user(text.to_string())
    }

    // ── Category classification ────────────────────────────────────────────────

    #[test]
    fn route_simple_chat_returns_fast() {
        // Genuinely context-free question with no retrieval, code, or vision signals.
        let decision = router().route(&[user_msg("tell me a joke about programmers")]);
        assert_eq!(decision.model, ModelId::Fast);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(decision.complexity, Complexity::TRIVIAL);
    }

    #[test]
    fn route_current_time_is_fast_chat() {
        // "What time is it?" must NOT route to RetrievalFirst (that hits DuckDuckGo,
        // which returns useless results for time queries). The model should answer
        // via a `date` shell action. Time is a system-state query, not a web query.
        let decision = router().route(&[user_msg("What time is it?")]);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(decision.model, ModelId::Fast);
    }

    #[test]
    fn route_implement_function_returns_code() {
        let decision =
            router().route(&[user_msg("implement a function to sort a list of integers")]);
        assert_eq!(decision.category, Category::Code);
        assert_eq!(decision.model, ModelId::Code);
    }

    #[test]
    fn route_screenshot_returns_vision() {
        let decision =
            router().route(&[user_msg("look at this screenshot and tell me what's wrong")]);
        assert_eq!(decision.category, Category::Vision);
        assert_eq!(decision.model, ModelId::Vision);
    }

    // ── Phase 20: Vision keyword coverage tests ────────────────────────────────

    #[test]
    fn route_see_my_screen_returns_vision() {
        // "see my screen" is a new Phase 20 keyword — natural operator phrasing.
        let decision = router().route(&[user_msg("can you see my screen?")]);
        assert_eq!(
            decision.category,
            Category::Vision,
            "\"see my screen\" must route to Vision"
        );
        assert_eq!(decision.model, ModelId::Vision);
    }

    #[test]
    fn route_look_at_the_screen_returns_vision() {
        // "look at the screen" is a new Phase 20 keyword.
        let decision =
            router().route(&[user_msg("look at the screen and tell me what app is open")]);
        assert_eq!(
            decision.category,
            Category::Vision,
            "\"look at the screen\" must route to Vision"
        );
        assert_eq!(decision.model, ModelId::Vision);
    }

    #[test]
    fn route_picture_keyword_returns_vision() {
        // "picture" is a pre-existing vision keyword; this test documents coverage.
        let decision = router().route(&[user_msg("take a picture of what's on screen")]);
        assert_eq!(
            decision.category,
            Category::Vision,
            "\"picture\" keyword must route to Vision"
        );
    }

    #[test]
    fn route_current_version_returns_retrieval_first() {
        let decision = router().route(&[user_msg("what is the current version of Rust?")]);
        assert_eq!(decision.category, Category::RetrievalFirst);
        assert_eq!(decision.model, ModelId::Primary);
    }

    // ── Complexity thresholds ─────────────────────────────────────────────────

    #[test]
    fn step_by_step_proof_returns_heavy() {
        let decision = router().route(&[user_msg(
            "prove step by step why the halting problem is undecidable",
        )]);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(decision.complexity, Complexity::DEEP);
        assert_eq!(decision.model, ModelId::Heavy);
    }

    #[test]
    fn system_design_question_returns_heavy() {
        let decision = router().route(&[user_msg(
            "design an architecture for a distributed task queue that handles 1M jobs/sec",
        )]);
        assert_eq!(decision.complexity, Complexity::DEEP);
        assert_eq!(decision.model, ModelId::Heavy);
    }

    #[test]
    fn complex_code_routes_to_code() {
        // Phase 37.5 / B3: all code-category queries route to CODE regardless
        // of complexity. The previous "complex code → PRIMARY" rule assumed
        // PRIMARY was the smarter generalist, but for code specifically the
        // programming specialist (deepseek-coder-v2:16b) wins.
        let decision = router().route(&[user_msg(
            "design a complete authentication system: implement JWT refresh tokens, \
                      database schema, error handling, and unit tests from scratch",
        )]);
        assert_eq!(decision.category, Category::Code);
        assert_eq!(decision.model, ModelId::Code);
    }

    #[test]
    fn simple_code_still_routes_to_code() {
        // Simple code queries (complexity 0 or 1) should also hit CODE — the
        // collapse of the split must not demote simple code requests.
        let decision = router().route(&[user_msg("implement a function to reverse a string")]);
        assert_eq!(decision.category, Category::Code);
        assert_eq!(decision.model, ModelId::Code);
    }

    #[test]
    fn explain_how_returns_primary_for_chat() {
        // Phase 37.5 / B4: a single moderate-reasoning signal is enough to push
        // a Chat query from FAST → PRIMARY. "explain how" scores +1, landing at
        // complexity=1 which now routes to PRIMARY (was FAST).
        let decision = router().route(&[user_msg(
            "explain how the Rust borrow checker prevents data races",
        )]);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(
            decision.model,
            ModelId::Primary,
            "single moderate signal must now route to PRIMARY, got {:?}",
            decision.model
        );
    }

    #[test]
    fn trivial_chat_still_routes_to_fast() {
        // The B4 shift must not push EVERY chat to PRIMARY — complexity=0
        // (no keywords, short) still belongs on FAST for latency.
        let decision = router().route(&[user_msg("tell me a joke about programmers")]);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(decision.complexity, Complexity::TRIVIAL);
        assert_eq!(decision.model, ModelId::Fast);
    }

    // ── Phase 37.7 Step 4: Classifier signal cleanup ──────────────────────────

    #[test]
    fn rewrite_paragraph_is_not_code() {
        // "rewrite " used to live in the Code keyword list and falsely classified
        // "rewrite this paragraph to sound less aggressive" as Code. Must now
        // route as Chat.
        let decision =
            router().route(&[user_msg("rewrite this paragraph to sound less aggressive")]);
        assert_eq!(
            decision.category,
            Category::Chat,
            "rewriting prose must not be classified as Code"
        );
    }

    #[test]
    fn code_fence_routes_to_code() {
        let decision = router().route(&[user_msg(
            "what does this do: ```fn main() { println!(\"hi\"); }```",
        )]);
        assert_eq!(
            decision.category,
            Category::Code,
            "code fences must classify as Code"
        );
    }

    #[test]
    fn file_extension_routes_to_code() {
        let decision = router().route(&[user_msg("why is main.rs not compiling")]);
        assert_eq!(decision.category, Category::Code);
    }

    #[test]
    fn syntax_marker_routes_to_code() {
        let decision = router().route(&[user_msg("what does pub fn new do here")]);
        assert_eq!(
            decision.category,
            Category::Code,
            "`pub fn` is an unambiguous Rust syntax marker"
        );
    }

    #[test]
    fn toolchain_routes_to_code() {
        let decision = router().route(&[user_msg("cargo build is giving me a weird error")]);
        assert_eq!(decision.category, Category::Code);
    }

    #[test]
    fn error_shape_routes_to_code() {
        let decision = router().route(&[user_msg("my code is panicking every time i call this")]);
        assert_eq!(decision.category, Category::Code);

        let decision2 = router().route(&[user_msg("the build is failing build in CI")]);
        assert_eq!(decision2.category, Category::Code);

        let decision3 = router().route(&[user_msg("I got a stack trace but can't read it")]);
        assert_eq!(decision3.category, Category::Code);
    }

    #[test]
    fn design_rest_api_routes_to_code() {
        let decision = router().route(&[user_msg("design a REST API for user authentication")]);
        assert_eq!(
            decision.category,
            Category::Code,
            "API/system-design intent should classify as Code, not Chat"
        );
    }

    // ── Phase 37.7 Step 5: char-count vs byte-count ───────────────────────────

    #[test]
    fn chars_count_not_bytes_for_length() {
        // 200 em-dashes = 600 bytes (each em-dash is 3 bytes in UTF-8) but only
        // 200 chars. Under `.len()` this would exceed 250 bytes and bump
        // complexity; under `.chars().count()` it stays at 200 chars and does
        // not. This test pins the char-count behavior.
        let em_dashes: String = "—".repeat(200);
        assert!(em_dashes.len() > 250, "sanity: byte count exceeds 250");
        assert_eq!(em_dashes.chars().count(), 200, "sanity: char count is 200");

        let decision = router().route(&[user_msg(&em_dashes)]);
        // No keywords, 200 chars (< 250) → complexity 0 → FAST.
        assert_eq!(
            decision.complexity,
            Complexity::TRIVIAL,
            "200 chars of non-ASCII must not bump complexity via byte inflation"
        );
    }

    // ── Phase 37.7 Step 6: gated DEEP escalation ──────────────────────────────

    #[test]
    fn from_scratch_short_prompt_is_not_heavy() {
        // "from scratch" used to unilaterally promote to HEAVY. A short throwaway
        // "hello world HTTP server from scratch" is not HEAVY work — weak DEEP
        // + short length should land at complexity 2 (PRIMARY).
        let decision = router().route(&[user_msg("write a hello world HTTP server from scratch")]);
        assert_ne!(
            decision.model,
            ModelId::Heavy,
            "short 'from scratch' must not unilaterally route to HEAVY: reasoning={}",
            decision.reasoning
        );
    }

    #[test]
    fn from_scratch_long_prompt_does_escalate() {
        // Same weak-DEEP signal on a long utterance SHOULD escalate HEAVY.
        // Fixture note: must not contain Code-classifier keywords (e.g.
        // "implement", "write a function", toolchain terms) — otherwise
        // classify_category returns Code, routing to CODE regardless of
        // complexity. We want to exercise the Chat complexity table, so this
        // prompt is phrased as a reasoning question.
        let long_text = "walk me through the full reasoning chain, from scratch, for \
            why a payment processor's distributed-transaction design needs \
            idempotency keys AND audit logs AND retry-with-backoff AND PCI DSS \
            compliance all at once end to end, including the failure modes that \
            arise when any one of those layers is missing";
        let decision = router().route(&[user_msg(long_text)]);
        // "end to end" is a strong DEEP trigger — this should land HEAVY.
        assert_eq!(
            decision.model,
            ModelId::Heavy,
            "long Chat utterance with 'from scratch' + 'end to end' should HEAVY: \
             reasoning={}",
            decision.reasoning
        );
    }

    #[test]
    fn compare_and_contrast_trivial_is_not_heavy() {
        let decision = router().route(&[user_msg("compare and contrast tabs vs spaces")]);
        assert_ne!(
            decision.model,
            ModelId::Heavy,
            "trivial 'compare and contrast' must not unilaterally light HEAVY"
        );
    }

    // ── Phase 37.7 Step 7: conversation depth no longer mutates complexity ────

    #[test]
    fn long_casual_thread_does_not_inflate_complexity() {
        // Build a thread with > 10 user turns of trivial chat. The final turn is
        // a simple question that would have complexity 0 on its own. Under the
        // old rule, turn 11+ would bump complexity to 1 → PRIMARY. Under the
        // Step-7 fix, complexity stays 0 → FAST.
        let mut msgs = Vec::new();
        for i in 1..=12 {
            msgs.push(user_msg(&format!("trivial question {i}")));
            msgs.push(assistant_msg(&format!("trivial answer {i}")));
        }
        // Replace last user with something directly classified as trivial Chat.
        msgs.push(user_msg("ok thanks"));

        let decision = router().route(&msgs);
        assert_eq!(
            decision.complexity,
            Complexity::TRIVIAL,
            "accumulated conversation turns must not inflate semantic complexity: \
             reasoning={}",
            decision.reasoning
        );
        assert_eq!(
            decision.model,
            ModelId::Fast,
            "trivial utterance in long thread must still route FAST"
        );
    }

    // ── Phase 37.7 Step 8: expanded retrieval-first coverage ──────────────────

    #[test]
    fn weather_routes_to_retrieval_first() {
        let decision = router().route(&[user_msg("what's the weather today")]);
        assert_eq!(decision.category, Category::RetrievalFirst);
    }

    #[test]
    fn current_ceo_routes_to_retrieval_first() {
        let decision = router().route(&[user_msg("who is the current CEO of Nvidia")]);
        assert_eq!(decision.category, Category::RetrievalFirst);
    }

    #[test]
    fn release_notes_route_to_retrieval_first() {
        let decision = router().route(&[user_msg(
            "show me the release notes for the new rust toolchain",
        )]);
        assert_eq!(decision.category, Category::RetrievalFirst);
    }

    #[test]
    fn ret2libc_prompt_routes_to_primary_not_fast() {
        // The Cluster-A/B regression case from the Phase 37.5 live test:
        // "Explain in technical depth how a ret2libc attack bypasses NX on
        // x86_64 Linux with ASLR enabled, including the role of a leaked
        // libc address."
        // Under the old scoring: 161 chars (≤ 250), "explain how" non-adjacent,
        // "in depth" not a substring of "in technical depth" → complexity 0
        // → FAST. Under Phase 37.7: "in technical depth" fires moderate +1
        // → complexity 1 → PRIMARY (substantive explanation, not trivia).
        let decision = router().route(&[user_msg(
            "Explain in technical depth how a ret2libc attack bypasses NX on \
             x86_64 Linux with ASLR enabled, including the role of a leaked \
             libc address.",
        )]);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(
            decision.model,
            ModelId::Primary,
            "technical-depth explanation must route to PRIMARY (not FAST): \
             reasoning={}",
            decision.reasoning
        );
    }

    #[test]
    fn what_time_is_it_still_chat_not_retrieval() {
        // Regression guard: the RetrievalFirst expansion must NOT start catching
        // "what time is it" — that's a system-state query answered via `date`.
        let decision = router().route(&[user_msg("what time is it")]);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(decision.model, ModelId::Fast);
    }

    // ── Sticky follow-up inheritance (Phase 37.7) ─────────────────────────────

    fn assistant_msg(text: &str) -> Message {
        Message::assistant(text.to_string())
    }

    #[test]
    fn inheritance_follow_up_after_code_routes_to_code() {
        // "implement a Rust parser" → Code; then "make it faster" alone classifies
        // as Chat → FAST, which is the dumb-feeling misroute. Sticky inheritance
        // must flip it to Code.
        let decision = router().route(&[
            user_msg("implement a Rust parser"),
            assistant_msg("Here's a parser..."),
            user_msg("make it faster"),
        ]);
        assert_eq!(
            decision.category,
            Category::Code,
            "ambiguous follow-up after Code must inherit Code, got {:?}",
            decision.category
        );
        assert_eq!(decision.model, ModelId::Code);
        assert_eq!(
            decision.inherited_category,
            Some(Category::Code),
            "provenance field must record inheritance"
        );
        assert!(
            decision.reasoning.contains("inherited from prior turn"),
            "reasoning should surface inheritance: {}",
            decision.reasoning
        );
    }

    #[test]
    fn inheritance_topic_shift_breaks_chain() {
        // Even though "tell me a joke" is short and follows a Code turn, the explicit
        // topic-shift phrase must kill inheritance. No inheritance, pure Chat routing.
        let decision = router().route(&[
            user_msg("implement a Rust parser"),
            assistant_msg("Here's a parser..."),
            user_msg("tell me a joke now"),
        ]);
        assert_eq!(
            decision.category,
            Category::Chat,
            "topic-shift phrase must defeat inheritance"
        );
        assert_eq!(decision.model, ModelId::Fast);
        assert_eq!(
            decision.inherited_category, None,
            "no-op/refused inheritance must leave provenance field None"
        );
    }

    #[test]
    fn inheritance_weak_cue_only_short_utterance() {
        // "and make it faster" — weak cue ("and") + strong cue ("make it", "faster")
        // should inherit. But "and now explain quantum mechanics to me in detail"
        // is long enough that no signal should trigger inheritance.
        let decision_short = router().route(&[
            user_msg("write a function to sort a list"),
            assistant_msg("Here..."),
            user_msg("and faster please"),
        ]);
        assert_eq!(
            decision_short.category,
            Category::Code,
            "short utterance with strong cue after Code should inherit"
        );

        let decision_long = router().route(&[
            user_msg("write a function to sort a list"),
            assistant_msg("Here..."),
            user_msg("and now explain quantum mechanics to me in some detail"),
        ]);
        // Long follow-up with no strong continuation cue — must NOT inherit Code.
        // "in detail" triggers complexity=1 → PRIMARY (Chat route, not Code).
        assert_eq!(
            decision_long.category,
            Category::Chat,
            "long follow-up with no strong cue must not inherit: reasoning={}",
            decision_long.reasoning
        );
        assert_eq!(decision_long.inherited_category, None);
    }

    #[test]
    fn inheritance_hop_cap_enforced_at_three() {
        // Turn 1 direct Code is the source. Turns 2, 3, 4 each inherit Code —
        // each having 0, 1, 2 ambiguous predecessors respectively between
        // them and turn 1. Turn 5 would have 3 ambiguous predecessors (turns
        // 2, 3, 4), which meets the `>= MAX_INHERITED_HOPS = 3` refusal
        // condition. So turn 5 falls back to direct-Chat routing.
        //
        // The cap counts ambiguous predecessors standing BETWEEN the current
        // turn and the source — not "how many inherited turns there have
        // been including this one." Easy to misread; if you're editing this,
        // re-check the `ambiguous_predecessor_count >= MAX_INHERITED_HOPS`
        // guard in maybe_inherit_category.
        let decision = router().route(&[
            user_msg("implement a Rust parser"), // direct Code
            assistant_msg("..."),
            user_msg("make it faster"), // hop 1 → Code
            assistant_msg("..."),
            user_msg("make it smaller"), // hop 2 → Code
            assistant_msg("..."),
            user_msg("make it cleaner"), // hop 3 → Code
            assistant_msg("..."),
            user_msg("simplify it again"), // would be hop 4 — refused
        ]);
        assert_eq!(
            decision.category,
            Category::Chat,
            "4th ambiguous hop must exceed cap and fall back to Chat: reasoning={}",
            decision.reasoning
        );
        assert_eq!(
            decision.inherited_category, None,
            "refused inheritance must leave provenance None"
        );
    }

    #[test]
    fn inheritance_hop_cap_allows_third_hop() {
        // Boundary check: the 3rd consecutive ambiguous follow-up is still allowed.
        let decision = router().route(&[
            user_msg("implement a Rust parser"), // direct Code
            assistant_msg("..."),
            user_msg("make it faster"), // hop 1 → Code
            assistant_msg("..."),
            user_msg("make it smaller"), // hop 2 → Code
            assistant_msg("..."),
            user_msg("make it cleaner"), // hop 3 — still allowed
        ]);
        assert_eq!(
            decision.category,
            Category::Code,
            "3rd hop is within cap and must inherit Code"
        );
        assert_eq!(decision.inherited_category, Some(Category::Code));
    }

    #[test]
    fn inheritance_explicit_chat_breaks_chain() {
        // Prior turn was directly and clearly Chat (not itself ambiguous). Inheritance
        // must NOT reach past it to find an older Code turn — the Chat turn represents
        // a resolved topic shift.
        //
        // Fixture note: the intermediate Chat turn must contain NO strong/weak
        // continuation cues, or the walk-back will treat it as just another
        // inherited-ambiguous hop and pass through it. This is a limitation of
        // stateless walk-back (no access to prior routing decisions) — a Chat
        // turn that happens to contain "why" or "make it" is indistinguishable
        // from one that was itself inheriting. The conservative policy: if the
        // intermediate turn LOOKS ambiguous by text alone, assume inheritance
        // chain; if it does not, treat as a hard break.
        let decision = router().route(&[
            user_msg("implement a Rust parser"),
            assistant_msg("..."),
            // No continuation cues; ambiguity predicate fails → chain breaks here.
            user_msg("what's your favorite ice cream flavor"),
            assistant_msg("..."),
            user_msg("make it faster"),
        ]);
        assert_eq!(decision.category, Category::Chat,
            "inheritance must not reach past a directly-classified Chat turn with no continuation cues");
        assert_eq!(decision.inherited_category, None);
    }

    #[test]
    fn inheritance_noop_when_direct_is_already_code() {
        // If the current utterance itself classifies as Code, inheritance must NOT
        // fire — inherited_category stays None even though the prior turn was Code.
        // The field records causal deltas, not possibilities.
        let decision = router().route(&[
            user_msg("write a sorting function"),
            assistant_msg("..."),
            user_msg("implement a binary search too"),
        ]);
        assert_eq!(decision.category, Category::Code);
        assert_eq!(
            decision.inherited_category, None,
            "direct Code signal means no inheritance event — provenance stays None"
        );
        assert!(
            !decision.reasoning.contains("inherited"),
            "reasoning must not mention inheritance when it didn't fire: {}",
            decision.reasoning
        );
    }

    #[test]
    fn inheritance_single_turn_conversation_no_inherit() {
        // A lone user message has no prior turn to inherit from.
        let decision = router().route(&[user_msg("make it faster")]);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(decision.inherited_category, None);
    }

    #[test]
    fn router_ignores_tool_result_when_finding_last_user_text() {
        // Phase 37.7 Step 2b: the router must find the last REAL user message,
        // not the last role="user" message. A tool-result injection between
        // the operator's last turn and routing must NOT be classified as the
        // query. Under the pre-fix code (filtering by `role`), the router
        // would have tried to classify "[Action result] cargo check failed"
        // as the user's question, producing a silent misroute.
        let decision = router().route(&[
            Message::user("implement a Rust parser"),
            Message::assistant("Here's a parser..."),
            Message::tool_result("[Action result] cargo check failed"),
            Message::user("make it faster"),
        ]);
        assert_eq!(
            decision.category,
            Category::Code,
            "router must find 'make it faster' (last User-origin), inherit from \
             'implement a Rust parser', and route to Code — not classify the \
             tool-result content"
        );
        assert_eq!(decision.inherited_category, Some(Category::Code));
    }

    #[test]
    fn inheritance_walk_ignores_synthetic_user_role_messages() {
        // Phase 37.7 Step 2b: the predecessor walk in maybe_inherit_category
        // must filter by `origin == User`, not `role == "user"`. A retrieval
        // injection between the source turn and the current ambiguous turn
        // must NOT be treated as a prior user turn — otherwise a synthetic
        // retrieved sentence that classifies as non-ambiguous Chat would
        // silently break the inheritance chain.
        let decision = router().route(&[
            Message::user("who is the current CEO of Nvidia?"),
            Message::assistant("Jensen Huang."),
            Message::retrieval("[Retrieved] Jensen Huang has served since 1993."),
            Message::user("why"),
        ]);
        assert_eq!(
            decision.category,
            Category::RetrievalFirst,
            "retrieval injection must not interrupt inheritance walk"
        );
        assert_eq!(decision.inherited_category, Some(Category::RetrievalFirst));
    }

    #[test]
    fn inheritance_retrieval_first_follow_up() {
        // "who is the current CEO of Nvidia?" → RetrievalFirst. Follow-up
        // "how long has he been there" is a classic pronoun-resolved continuation
        // that should inherit RetrievalFirst, not regress to Chat.
        // Note: current predicate may not catch this without "why"/"make it"/etc.
        // This test documents the gap; see "how long" strong-cue consideration.
        let decision = router().route(&[
            user_msg("who is the current CEO of Nvidia?"),
            assistant_msg("Jensen Huang."),
            user_msg("why though"), // "why" is a strong continuation cue
        ]);
        assert_eq!(
            decision.category,
            Category::RetrievalFirst,
            "'why' after retrieval turn should inherit RetrievalFirst"
        );
        assert_eq!(decision.inherited_category, Some(Category::RetrievalFirst));
    }

    #[test]
    fn reasoning_string_contains_category_and_complexity() {
        let decision = router().route(&[user_msg("implement a sorting function")]);
        assert!(
            decision.reasoning.contains("category=code"),
            "reasoning should include category: {}",
            decision.reasoning
        );
        assert!(
            decision.reasoning.contains("complexity="),
            "reasoning should include complexity: {}",
            decision.reasoning
        );
    }

    // ── ConversationContext ────────────────────────────────────────────────────

    #[test]
    fn context_starts_empty() {
        let ctx = ConversationContext::new("test-session", 10);
        assert_eq!(ctx.message_count(), 0);
        assert_eq!(ctx.turn_count(), 0);
    }

    #[test]
    fn context_push_user_increments_turn_count() {
        let mut ctx = ConversationContext::new("s1", 10);
        ctx.push_user("hello");
        assert_eq!(ctx.turn_count(), 1);
        ctx.push_assistant("hi");
        assert_eq!(ctx.turn_count(), 1); // assistant doesn't add a turn
        ctx.push_user("how are you?");
        assert_eq!(ctx.turn_count(), 2);
    }

    #[test]
    fn context_truncation_drops_oldest_pair() {
        let mut ctx = ConversationContext::new("s1", 2); // only 2 turns
        ctx.push_user("turn 1 user");
        ctx.push_assistant("turn 1 assistant");
        ctx.push_user("turn 2 user");
        ctx.push_assistant("turn 2 assistant");
        // Now at limit. One more push should drop turn 1.
        ctx.push_user("turn 3 user");

        assert_eq!(ctx.turn_count(), 2, "should have trimmed to max_turns=2");
        // turn 1 should be gone.
        let messages = ctx.messages();
        assert!(
            !messages.iter().any(|m| m.content.contains("turn 1")),
            "oldest turn should have been dropped: {messages:?}"
        );
        // turn 2 and turn 3 should remain.
        assert!(messages.iter().any(|m| m.content.contains("turn 2 user")));
        assert!(messages.iter().any(|m| m.content.contains("turn 3 user")));
    }

    #[test]
    fn context_truncation_preserves_system_message() {
        let mut ctx = ConversationContext::new("s1", 1); // only 1 turn
        ctx.set_system_message("You are Dexter.");
        ctx.push_user("turn 1");
        ctx.push_assistant("resp 1");
        // Trigger truncation.
        ctx.push_user("turn 2");

        let messages = ctx.messages();
        // System message must survive.
        assert_eq!(
            messages[0].role, "system",
            "system message must be preserved after truncation"
        );
        assert_eq!(messages[0].content, "You are Dexter.");
        // turn 2 should survive; turn 1 should be gone.
        assert!(
            messages.iter().any(|m| m.content == "turn 2"),
            "current turn must survive"
        );
        assert!(
            !messages.iter().any(|m| m.content == "turn 1"),
            "old user turn must be dropped"
        );
        assert!(
            !messages.iter().any(|m| m.content == "resp 1"),
            "old assistant turn must be dropped with its user turn"
        );
    }

    #[test]
    fn context_set_system_message_replaces_existing() {
        let mut ctx = ConversationContext::new("s1", 10);
        ctx.set_system_message("original system");
        ctx.set_system_message("updated system");
        // Should still have exactly one system message.
        let system_count = ctx.messages().iter().filter(|m| m.role == "system").count();
        assert_eq!(system_count, 1);
        assert_eq!(ctx.messages()[0].content, "updated system");
    }

    #[test]
    fn context_clear_history_keeps_system_message() {
        let mut ctx = ConversationContext::new("s1", 10);
        ctx.set_system_message("Dexter system prompt.");
        ctx.push_user("hello");
        ctx.push_assistant("hi");
        ctx.clear_history();

        assert_eq!(ctx.message_count(), 1, "only system message should remain");
        assert_eq!(ctx.messages()[0].role, "system");
    }

    // ── Phase 37.7: MessageOrigin budget-accounting tests ─────────────────────

    #[test]
    fn tool_result_does_not_consume_turn_budget() {
        // The Phase 37.7 fix: tool-result injections must NOT count toward
        // `max_turns`. Before the MessageOrigin fix, `turn_count()` counted all
        // `role == "user"` messages, and three tool results would burn three
        // turns of real-history budget, evicting older real user turns.
        let mut ctx = ConversationContext::new("s1", 4);
        ctx.push_user("question 1");
        ctx.push_assistant("answer 1");
        ctx.push_tool_result("[Action result] ls → foo.txt bar.txt");
        ctx.push_tool_result("[Action result] cat foo.txt → hello");
        ctx.push_tool_result("[Action result] date → 2026-04-18");
        ctx.push_user("question 2");
        ctx.push_assistant("answer 2");

        // Only 2 real user turns exist; tool results must not inflate the count.
        assert_eq!(
            ctx.turn_count(),
            2,
            "turn_count must ignore tool-result origins; got {}",
            ctx.turn_count()
        );

        // And turn 1 must still be present — the budget did NOT trigger a trim.
        let has_turn_1 = ctx
            .messages()
            .iter()
            .any(|m| m.origin == MessageOrigin::User && m.content == "question 1");
        assert!(
            has_turn_1,
            "question 1 must survive — tool results should not evict real history"
        );
    }

    #[test]
    fn many_tool_results_do_not_trigger_trim() {
        // Stress variant: 20 tool-result injections between two real turns
        // would have evicted turn 1 under the old accounting. Under the
        // MessageOrigin fix, turn_count stays at 2 throughout.
        let mut ctx = ConversationContext::new("s1", 4);
        ctx.push_user("real turn 1");
        ctx.push_assistant("resp 1");
        for i in 0..20 {
            ctx.push_tool_result(&format!("[Action result {i}]"));
        }
        ctx.push_user("real turn 2");

        assert_eq!(ctx.turn_count(), 2);
        assert!(
            ctx.messages()
                .iter()
                .any(|m| m.origin == MessageOrigin::User && m.content == "real turn 1"),
            "real turn 1 must survive a flood of tool results"
        );
    }

    #[test]
    fn trim_evicts_tool_results_attached_to_oldest_turn() {
        // When trimming finally DOES fire (because real user turns exceeded
        // max_turns), the trimmer must evict the oldest user turn AND all the
        // intermediate non-user messages (assistant responses, tool results,
        // retrieval injections) that belong to that turn — as a single unit.
        let mut ctx = ConversationContext::new("s1", 1);
        ctx.push_user("turn 1");
        ctx.push_assistant("resp 1");
        ctx.push_tool_result("tool result for turn 1");
        ctx.push_user("turn 2"); // triggers trim of turn 1 + its children

        assert_eq!(ctx.turn_count(), 1);
        let has_turn_1 = ctx.messages().iter().any(|m| m.content == "turn 1");
        let has_turn_1_tool = ctx
            .messages()
            .iter()
            .any(|m| m.content == "tool result for turn 1");
        let has_turn_1_resp = ctx.messages().iter().any(|m| m.content == "resp 1");
        assert!(!has_turn_1, "turn 1 evicted");
        assert!(
            !has_turn_1_resp,
            "turn 1's assistant response evicted with it"
        );
        assert!(!has_turn_1_tool, "turn 1's tool result evicted with it");
        assert!(
            ctx.messages().iter().any(|m| m.content == "turn 2"),
            "turn 2 survives the trim"
        );
    }

    #[test]
    fn trim_handles_retrieval_between_system_and_first_user() {
        // Robustness test for the rewritten trimmer: even if a retrieval
        // injection lands BEFORE the first user turn (breaking the old
        // "first non-system is always user" invariant), trimming must find
        // the first Origin::User via search, not by position assumption.
        let mut ctx = ConversationContext::new("s1", 1);
        ctx.set_system_message("personality");
        // Retrieval ahead of any real turn — an invariant the old code
        // would have broken against.
        ctx.messages
            .insert(1, Message::retrieval("[Retrieved: boot context]"));
        ctx.push_user("turn 1");
        ctx.push_assistant("resp 1");
        ctx.push_user("turn 2"); // triggers trim

        assert_eq!(ctx.turn_count(), 1, "should have trimmed to 1 real turn");
        // turn 1 should be gone; turn 2 and system and retrieval should remain.
        assert!(!ctx.messages().iter().any(|m| m.content == "turn 1"));
        assert!(ctx.messages().iter().any(|m| m.content == "turn 2"));
        assert!(ctx.messages().iter().any(|m| m.role == "system"));
    }

    #[test]
    fn context_clear_history_without_system_clears_all() {
        let mut ctx = ConversationContext::new("s1", 10);
        ctx.push_user("hello");
        ctx.push_assistant("hi");
        ctx.clear_history();
        assert_eq!(ctx.message_count(), 0);
    }

    /// Phase 36 / N1 fix verification: with `CONVERSATION_MAX_TURNS = 16`, the
    /// operator can ask up to 16 questions and the FIRST question's content is
    /// still recoverable from the context. The previous limit (8) evicted turn 1
    /// after the 9th question — surfaced to the operator as the "memory cliff."
    ///
    /// This test pins the behavior to the live constant so a future change to
    /// `CONVERSATION_MAX_TURNS` triggers an obvious fail here.
    #[test]
    fn n1_memory_cliff_recall_at_phase_36_limit() {
        use crate::constants::CONVERSATION_MAX_TURNS;
        assert_eq!(
            CONVERSATION_MAX_TURNS, 16,
            "Phase 36 sets CONVERSATION_MAX_TURNS=16; if you change it, update this test"
        );

        let mut ctx = ConversationContext::new("n1-session", CONVERSATION_MAX_TURNS);
        // Push 16 user+assistant turn pairs — the maximum the new limit allows
        // without any eviction. Tag each turn so we can prove turn-1 survives.
        for i in 1..=16 {
            ctx.push_user(format!("question {i}"));
            ctx.push_assistant(format!("answer {i}"));
        }

        // No eviction has happened yet — turn count is exactly at the limit.
        assert_eq!(ctx.turn_count(), 16, "16 user turns must all be present");

        // The decisive assertion: the very first question is still in context.
        // Under the old limit (8), turn 1 would have been evicted by turn 9 and
        // the operator would not get a recall match here.
        let first_user_present = ctx
            .messages()
            .iter()
            .any(|m| m.role == "user" && m.content == "question 1");
        assert!(
            first_user_present,
            "N1 regression: 'question 1' must still be in context after 16 turns"
        );

        // Sanity: pushing turn 17 evicts the oldest pair — confirms trim still works.
        ctx.push_user("question 17".to_string());
        let first_user_after_17 = ctx
            .messages()
            .iter()
            .any(|m| m.role == "user" && m.content == "question 1");
        assert!(
            !first_user_after_17,
            "Eviction must trigger at turn_count > max_turns (defensive — proves the limit is real)"
        );
    }

    // ── Phase 37.9: live-smoke T2/T4 regression guards ───────────────────────
    //
    // T2 live failure: "walk through every step an attacker would use to persist
    // on a hardened macOS host" scored complexity 0 → FAST. That query is a
    // textbook HEAVY candidate — multi-step adversarial reasoning on a domain
    // topic. Root cause: none of the Strong DEEP phrases matched. Fix: added
    // "walk through" and "step of" paraphrases to the DEEP list.
    //
    // T4 live failure: "write a Rust function that uses rayon's parallel
    // iterator to compute prime counts in a range" classified as Chat →
    // complexity 0 → FAST. Root cause: the Code classifier's "write a function"
    // keyword misses when an infix like "Rust" breaks substring contiguity
    // (contains_any is substring-based, not token-based). Fix: added
    // per-language "<lang> function/class/struct" patterns and Rust crate
    // signals ("use rayon", "rayon::", etc.) that survive typos in "function".

    #[test]
    fn walk_through_every_step_routes_heavy() {
        // The T2 live-smoke query verbatim (minus offsec wording — behavior is
        // identical to the classifier since DEEP signals fire pre-domain).
        let decision = router().route(&[user_msg(
            "walk through every step an attacker would use to persist on a hardened macOS host",
        )]);
        assert_eq!(decision.category, Category::Chat);
        assert_eq!(
            decision.complexity,
            Complexity::DEEP,
            "'walk through every step' must trigger Strong DEEP; got reasoning={}",
            decision.reasoning
        );
        assert_eq!(
            decision.model,
            ModelId::Heavy,
            "DEEP Chat must route HEAVY; got {:?} reasoning={}",
            decision.model,
            decision.reasoning
        );
    }

    #[test]
    fn walk_me_through_routes_heavy() {
        // Common operator paraphrase: "walk me through the threat model".
        let decision = router().route(&[user_msg(
            "walk me through the threat model for a public DNS resolver",
        )]);
        assert_eq!(decision.complexity, Complexity::DEEP);
        assert_eq!(decision.model, ModelId::Heavy);
    }

    #[test]
    fn each_step_of_routes_heavy() {
        // Another paraphrase family: "each step of" / "every step of".
        let decision = router().route(&[user_msg(
            "describe each step of the TLS handshake including key derivation",
        )]);
        assert_eq!(
            decision.complexity,
            Complexity::DEEP,
            "'each step of' must match the new DEEP paraphrase set"
        );
    }

    #[test]
    fn rust_function_query_routes_to_code() {
        // The T4 live-smoke query shape (without the "funcrion" typo — typos
        // remain an open problem; this test pins the clean phrasing).
        let decision = router().route(&[
            user_msg("write a Rust function that uses rayon's parallel iterator to compute prime counts in a range")
        ]);
        assert_eq!(
            decision.category,
            Category::Code,
            "'<lang> function' must classify as Code; got reasoning={}",
            decision.reasoning
        );
        assert_eq!(decision.model, ModelId::Code);
    }

    #[test]
    fn python_class_query_routes_to_code() {
        let decision = router().route(&[user_msg(
            "write a Python class that wraps a SQLite connection with context manager support",
        )]);
        assert_eq!(decision.category, Category::Code);
        assert_eq!(decision.model, ModelId::Code);
    }

    #[test]
    fn swift_struct_query_routes_to_code() {
        let decision = router().route(&[user_msg(
            "design a Swift struct for tracking window state across spaces",
        )]);
        assert_eq!(decision.category, Category::Code);
    }

    #[test]
    fn use_rayon_routes_to_code() {
        // Rust crate signal — survives a typo'd "function" / "funcrion".
        // The live T4 query had "funcrion"; this test simulates that by using
        // the crate signal alone as the classifier hook.
        let decision = router().route(&[
            user_msg("show me how to use rayon to parallelize a hot loop (funcrion signature: fn count_primes)")
        ]);
        assert_eq!(
            decision.category,
            Category::Code,
            "'use rayon' must classify as Code even when 'function' is mis-spelled; reasoning={}",
            decision.reasoning
        );
    }

    #[test]
    fn tokio_path_signal_routes_to_code() {
        // `tokio::` path separator is an unambiguous Rust code marker.
        let decision = router().route(&[user_msg("why does tokio::spawn not await my future")]);
        assert_eq!(decision.category, Category::Code);
    }

    #[test]
    fn bare_rust_word_does_not_route_to_code() {
        // Guard: adding per-language patterns must not sweep in "rust" alone.
        // "There's rust on my bike chain" is not a code query.
        let decision = router().route(&[user_msg(
            "there's rust on my bike chain, how do I remove it",
        )]);
        assert_eq!(
            decision.category,
            Category::Chat,
            "bare word 'rust' without a code-noun must stay Chat; reasoning={}",
            decision.reasoning
        );
    }

    #[test]
    fn trait_word_alone_does_not_route_to_code() {
        // Guard: "personality trait" must not match "rust trait".
        let decision = router().route(&[user_msg(
            "which personality trait do you think matters most in a leader",
        )]);
        assert_eq!(decision.category, Category::Chat);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns true if `text` contains any of the given keyword substrings.
/// All comparisons are already lowercased at the call site.
fn contains_any(text: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|kw| text.contains(kw))
}

// ── Sticky inheritance constants (Phase 37.7) ────────────────────────────────

/// Maximum consecutive inherited-category resolutions before inheritance stops.
///
/// After this many ambiguous follow-ups in a row, the conversation has drifted
/// too far from the last direct signal — the router forces re-classification
/// from the current utterance alone, which will typically land on Chat/FAST.
/// Prevents inheritance from becoming self-propagating sludge.
const MAX_INHERITED_HOPS: usize = 3;

/// Maximum character length for an utterance to be considered ambiguous.
/// Long utterances carry their own signal; they don't need inheritance help.
/// Counted in chars (not bytes) to handle non-ASCII cleanly.
const AMBIGUITY_MAX_CHARS: usize = 60;

/// Maximum char length under which a "weak" continuation cue (and/but/also)
/// is sufficient on its own. Above this, weak cues are dismissed — "and now
/// explain quantum mechanics" should not inherit just because it starts with "and".
const WEAK_CUE_MAX_CHARS: usize = 20;

/// Explicit topic-shift phrases. If any hit, inheritance is refused regardless
/// of other signals — even a short "by the way, fix this" (which would otherwise
/// pass the strong-cue test via "fix") must NOT inherit.
const TOPIC_SHIFT_PHRASES: &[&str] = &[
    "tell me a joke",
    "new topic",
    "unrelated",
    "different question",
    "by the way",
    "separately",
    "another question",
    "change topic",
    "forget that",
    "never mind that",
    "ignore that",
    "off-topic",
    "off topic",
    "new question",
];

/// Strong continuation cues — unambiguous signals that the current utterance
/// refers back to the prior turn's subject. One hit is sufficient to qualify
/// as ambiguity-eligible (combined with the length gate).
///
/// Deliberately excludes weak coordinators ("and"/"but"/"also") which appear
/// in topic-shift sentences far too often to trust on their own.
const STRONG_CONTINUATION_CUES: &[&str] = &[
    // Questioning markers.
    //
    // Policy note: `"why"` is deliberately permissive. It will make any short
    // utterance containing "why" in any multi-turn thread eligible for
    // inheritance, including cases where the operator is reacting to something
    // unrelated ("why though"). This is an accepted tradeoff: operators
    // overwhelmingly use "why" as a pronoun-elided follow-up ("why is it slow",
    // "why does it fail") after a technical turn, and the alternative —
    // demanding a co-occurring subject reference — breaks the natural short
    // form. If false-positive inheritance from a stray "why" becomes a real
    // problem in live use, tighten to "why does" / "why is" / "why would".
    "why",
    "how about",
    "what about",
    "instead",
    "again",
    // Reference + command patterns
    "make it",
    "make that",
    "make them",
    // Bare imperatives (typically subject-elided from prior turn)
    "refactor",
    "simplify",
    "expand",
    "shorten",
    "fix ",
    "rename",
    "optimize",
    "add ",
    "remove",
    "delete this",
    "tweak",
    "rewrite it",
    "rewrite this",
    "rewrite that",
    // Comparative follow-ups
    "faster",
    "slower",
    "smaller",
    "bigger",
    "shorter",
    "longer",
    "cleaner",
    "simpler",
    "better",
];

/// Weak continuation cues — coordinators that MAY imply continuation but more
/// often appear in topic shifts. Only valid as inheritance triggers when the
/// utterance is extremely short (`WEAK_CUE_MAX_CHARS`).
const WEAK_CONTINUATION_CUES: &[&str] = &["and ", "but ", "also "];

/// True if `text` contains any explicit topic-shift phrase.
fn is_topic_shift(lower: &str) -> bool {
    contains_any(lower, TOPIC_SHIFT_PHRASES)
}

/// True if `text` qualifies as an ambiguous follow-up eligible for category inheritance.
///
/// Requires: short (char count under cap) AND contains a strong continuation cue,
/// OR very short AND contains a weak continuation cue.
///
/// Intentionally does NOT infer grammar ("starts with a bare verb") — that swallows
/// too many non-continuation imperatives. The strong-cue list is a hand-picked
/// continuation-imperative set instead.
fn is_ambiguous_followup(raw: &str, lower: &str) -> bool {
    let char_count = raw.chars().count();
    if char_count >= AMBIGUITY_MAX_CHARS {
        return false;
    }
    if contains_any(lower, STRONG_CONTINUATION_CUES) {
        return true;
    }
    if char_count < WEAK_CUE_MAX_CHARS && contains_any(lower, WEAK_CONTINUATION_CUES) {
        return true;
    }
    false
}
