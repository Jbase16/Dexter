//! Retrieval-first query classifier.
//!
//! Identifies query types that are always better answered from fresh retrieval
//! than from model memory. For these queries, the orchestrator fires the retrieval
//! pipeline before the first model call and injects the result as context.
//!
//! ## Design rationale
//!
//! Rule-based matching covers the highest-frequency retrieval-first patterns with
//! zero latency and is trivially testable. The function signature is the abstraction
//! boundary: a model-based classifier can replace the body without changing any call
//! site in the orchestrator (see §3.4 of PHASE_19_SPEC.md).

/// Returns `true` if the query should be resolved by retrieval before model inference.
///
/// Called by the orchestrator at the start of `handle_text_input`, before routing.
/// A `true` result means: run `RetrievalPipeline::retrieve_web_only(input)` first and
/// inject the result into context before the first `generate_stream` call.
///
/// This check runs in addition to the router's `Category::RetrievalFirst` detection.
/// The classifier covers patterns that the router may not detect (more comprehensive
/// pattern sets), while the router's detection covers phrasing variants that the
/// classifier's static strings do not.
pub fn is_retrieval_first_query(input: &str) -> bool {
    let s = input.trim().to_lowercase();
    DATETIME_PATTERNS.iter().any(|p| s.contains(p))
        || VERSION_PATTERNS.iter().any(|p| s.contains(p))
        || PERSON_STATUS_PATTERNS.iter().any(|p| s.contains(p))
        || NEWS_PATTERNS.iter().any(|p| s.contains(p))
}

/// Queries about the current date, time, day, or year.
///
/// NOTE: deliberately empty. Time/date queries are answered by running the `date`
/// shell action — no web retrieval needed. DuckDuckGo returns useless results for
/// "what time is it?" and the local system clock is always more accurate. Any
/// phrasing that previously lived here now routes as Chat/FAST so the model can
/// use its shell access to call `date`.
const DATETIME_PATTERNS: &[&str] = &[];

/// Queries about the latest or current version of a software package.
///
/// Version numbers change frequently. Generating from training data produces stale
/// answers that may be multiple major versions behind.
const VERSION_PATTERNS: &[&str] = &[
    "latest version of",
    "current version of",
    "newest version of",
    "what version of",
    "which version of",
    "most recent version of",
];

/// Queries about who currently holds a named role or position.
///
/// Executive roles, political offices, and leadership positions change.
/// Generating from training data can name a previous holder as current.
const PERSON_STATUS_PATTERNS: &[&str] = &[
    "who is the current",
    "who is the ceo",
    "who is the president",
    "who is the prime minister",
    "who runs ",
    "who leads ",
    "who is cto",
    "who is cfo",
    "who is the head of",
];

/// Queries about recent events or news.
///
/// Training data cutoff means recent events are absent or incomplete.
const NEWS_PATTERNS: &[&str] = &[
    "what happened with",
    "latest news on",
    "latest news about",
    "recent news about",
    "what's happening with",
    "any updates on",
    "what's new with",
];

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_does_not_flag_datetime_queries() {
        // Time/date queries are answered via `date` shell action — no web retrieval.
        assert!(
            !is_retrieval_first_query("what time is it?"),
            "time query must NOT be retrieval-first (use shell date)"
        );
        assert!(
            !is_retrieval_first_query("What's the date today?"),
            "date query must NOT be retrieval-first (use shell date)"
        );
        assert!(
            !is_retrieval_first_query("What year is it currently?"),
            "year query must NOT be retrieval-first (use shell date)"
        );
    }

    #[test]
    fn classifier_detects_version_query() {
        assert!(
            is_retrieval_first_query("what is the latest version of rust?"),
            "latest version query must be retrieval-first"
        );
        assert!(
            is_retrieval_first_query("latest version of xcode"),
            "lowercase variant must be detected"
        );
        assert!(
            is_retrieval_first_query("Which version of swift is current?"),
            "which version query must be retrieval-first"
        );
    }

    #[test]
    fn classifier_passes_non_retrieval_query() {
        assert!(
            !is_retrieval_first_query("how does async rust work?"),
            "conceptual query must NOT be retrieval-first"
        );
        assert!(
            !is_retrieval_first_query("explain the borrow checker"),
            "explanation query must NOT be retrieval-first"
        );
        assert!(
            !is_retrieval_first_query("write me a function that sorts a vec"),
            "code request must NOT be retrieval-first"
        );
        assert!(
            !is_retrieval_first_query("what is polymorphism"),
            "definition query must NOT be retrieval-first"
        );
    }
}
