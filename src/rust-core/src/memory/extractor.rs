/// Implicit fact extraction from free-form user messages.
///
/// Detects facts the operator states incidentally — without explicit
/// "remember X" commands. Uses high-precision regex patterns against four
/// categories: identity, technology, occupation, location.
///
/// ## Design constraints
///
/// - **Precision over recall**: false positives (storing wrong facts as operator
///   facts) corrupt the memory store permanently. Patterns are narrow and require
///   definitive declarative phrasing. Interrogative forms are rejected early.
/// - **No inference**: pure regex — no model call, no embedding, sub-millisecond.
///   The caller (`handle_text_input()` step 8c) does the embedding via `store_fact()`.
/// - **Deduplication at storage**: `slug_id(fact_string)` produces a stable key;
///   `store_fact()` uses `upsert()` so re-stating the same fact replaces the
///   previous entry rather than creating a duplicate.
/// - **Not exhaustive**: four conservative categories. Extending requires new
///   patterns and corresponding tests — do not add patterns without tests.
use std::sync::OnceLock;

use regex::Regex;

// ── Pattern table ─────────────────────────────────────────────────────────────

/// Each entry: `(label, compiled Regex)`.
///
/// The label becomes the fact prefix: `"operator {label}: {capture}"`.
/// Compiled once at first call via `OnceLock` — subsequent calls are zero-cost.
static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

fn patterns() -> &'static [(&'static str, Regex)] {
    PATTERNS.get_or_init(|| {
        vec![
            // Identity: "my name is Jason" / "my name is Jason Smith"
            // Capture: alphabetic name including spaces, hyphens, apostrophes.
            // \b at the end prevents mid-word truncation on short inputs.
            (
                "name",
                Regex::new(r"(?i)\bmy name is ([A-Za-z][A-Za-z '\-]{0,46}\b)").unwrap(),
            ),

            // Technology — "I'm using X" / "I am using X"
            //
            // Boundary design: non-greedy {0,29}? quantifier + consumed alternation.
            //
            // The greedy approach with (?:[,.]|$| and | but | for ) fails because the
            // capture class includes spaces: greedy matching on "Python 3.14 for my
            // project" consumes the space before "for" into the capture group, leaving
            // "for " (no leading space) which doesn't match the " for " alternative.
            // Backtracking cannot recover because the space is already in the capture.
            //
            // Fix: non-greedy {0,29}? grows the capture lazily, stopping the moment the
            // trailing alternation matches. \s+ in the alternation consumes the space
            // between the captured text and the stop word — outside the capture group.
            //
            // \.(?:\s|$) matches sentence-ending periods only — period followed by
            // whitespace or end-of-string. This prevents stopping at mid-word periods
            // in tool names like "Node.js" (followed by "j") or "Python 3.14" (followed
            // by "1"), while correctly stopping at "Rust." (followed by end-of-string).
            //
            // Trace: "I'm using Python 3.14 for my project"
            //   Grow: "P"→"Py"→...→"Python 3.14"
            //   Alternation at " for my project": \s+for\b → " for" → match ✓
            //   Capture = "Python 3.14"
            //
            // Trace: "I'm using Node.js"
            //   Grow: ...→"Node" → alternation at ".js": \.(?:\s|$) → "j" ≠ \s → fail
            //   Continue: "Node.j"→"Node.js" → alternation at "": \s*$ → match ✓
            //   Capture = "Node.js"
            (
                "uses",
                Regex::new(r"(?i)\bI(?:'m| am) using ([A-Za-z0-9][A-Za-z0-9 .+#_\-]{0,29}?)(?:\s+(?:and|but|for)\b|[,]|\.(?:\s|$)|\s*$)").unwrap(),
            ),

            // Technology — "I use X"
            // Same semantics and boundary design as the "I'm using" pattern above.
            (
                "uses",
                Regex::new(r"(?i)\bI use ([A-Za-z0-9][A-Za-z0-9 .+#_\-]{0,29}?)(?:\s+(?:and|but|for)\b|[,]|\.(?:\s|$)|\s*$)").unwrap(),
            ),

            // Occupation: "I work at Anthropic" / "I work for Stripe Inc."
            // Non-greedy {0,48}? — same boundary design as uses patterns.
            // Comma is NOT in the capture class; it is a boundary (avoids "Foo, Inc." bleed).
            (
                "works at",
                Regex::new(r"(?i)\bI work (?:at|for) ([A-Za-z][A-Za-z0-9 &.\-]{0,48}?)(?:\s+(?:and|but|for)\b|[,]|\.(?:\s|$)|\s*$)").unwrap(),
            ),

            // Location: "I'm based in San Francisco" / "I am based at HQ"
            // Non-greedy {0,48}?. Place names have no mid-word periods so \.(?:\s|$)
            // is consistent but primarily fires on comma-separated clauses or sentence ends.
            (
                "location",
                Regex::new(r"(?i)\bI(?:'m| am) based (?:in|at) ([A-Za-z][A-Za-z0-9 \-]{0,48}?)(?:[,]|\.(?:\s|$)|\s*$)").unwrap(),
            ),

            // Location: "I live in Tokyo"
            (
                "location",
                Regex::new(r"(?i)\bI live in ([A-Za-z][A-Za-z0-9 \-]{0,48}?)(?:[,]|\.(?:\s|$)|\s*$)").unwrap(),
            ),
        ]
    })
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Extract zero or more operator facts from a free-form user message.
///
/// Returns normalized fact strings ready for storage via `store_fact()`.
/// The format is `"operator {label}: {capture}"` — human-readable in
/// `list_facts()` output and `[Memory: ...]` injection.
///
/// Returns `vec![]` for:
/// - Questions (text ending in `?`)
/// - Very short inputs (< 8 characters)
/// - Inputs matching no pattern
///
/// # Examples
///
/// ```ignore
/// let facts = extract_facts("my name is Jason");
/// assert_eq!(facts, vec!["operator name: Jason"]);
///
/// let facts = extract_facts("I'm using Python 3.14 for everything");
/// assert_eq!(facts, vec!["operator uses: Python 3.14"]);
///
/// let facts = extract_facts("what is my name?");
/// assert!(facts.is_empty());
/// ```
pub fn extract_facts(text: &str) -> Vec<String> {
    let trimmed = text.trim();

    // Early-exit guards — prevent false positives on questions and trivial inputs.
    if trimmed.ends_with('?') || trimmed.len() < 8 {
        return vec![];
    }

    let mut facts = Vec::new();
    for (label, regex) in patterns() {
        if let Some(cap) = regex.captures(trimmed) {
            let payload = cap.get(1).map_or("", |m| m.as_str()).trim();
            // Discard empty or single-character captures — they indicate a partial match
            // at a word boundary and would produce meaningless facts.
            if payload.len() >= 2 {
                facts.push(format!("operator {}: {}", label, payload));
            }
        }
    }
    facts
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_facts_name_pattern_captures_full_name() {
        let facts = extract_facts("my name is Jason");
        assert_eq!(facts.len(), 1, "expected 1 fact, got: {:?}", facts);
        assert_eq!(facts[0], "operator name: Jason");
    }

    #[test]
    fn extract_facts_name_pattern_with_last_name() {
        let facts = extract_facts("my name is Jason Smith");
        assert_eq!(facts.len(), 1, "expected 1 fact, got: {:?}", facts);
        assert_eq!(facts[0], "operator name: Jason Smith");
    }

    #[test]
    fn extract_facts_tech_using_pattern_stops_at_boundary() {
        // Regression test for the greedy boundary bug: "Python 3.14 for" must not
        // bleed into the capture. Non-greedy + \s+for\b stops before the conjunction.
        let facts = extract_facts("I'm using Python 3.14 for my project");
        assert_eq!(facts.len(), 1, "expected 1 fact, got: {:?}", facts);
        assert_eq!(facts[0], "operator uses: Python 3.14");
    }

    #[test]
    fn extract_facts_tech_use_pattern() {
        let facts = extract_facts("I use Rust.");
        assert_eq!(facts.len(), 1, "expected 1 fact, got: {:?}", facts);
        assert_eq!(facts[0], "operator uses: Rust");
    }

    #[test]
    fn extract_facts_work_at_pattern() {
        let facts = extract_facts("I work at Anthropic.");
        assert_eq!(facts.len(), 1, "expected 1 fact, got: {:?}", facts);
        assert_eq!(facts[0], "operator works at: Anthropic");
    }

    #[test]
    fn extract_facts_location_based_in_pattern() {
        let facts = extract_facts("I'm based in San Francisco.");
        assert_eq!(facts.len(), 1, "expected 1 fact, got: {:?}", facts);
        assert_eq!(facts[0], "operator location: San Francisco");
    }

    #[test]
    fn extract_facts_returns_empty_for_questions() {
        // Interrogative guard: ending in '?' suppresses all extraction.
        // This prevents "do you know my name?" from storing "operator name: [capture]".
        assert!(
            extract_facts("what is my name?").is_empty(),
            "question must produce no facts"
        );
        assert!(
            extract_facts("am I using the right tool?").is_empty(),
            "question must produce no facts"
        );
        assert!(
            extract_facts("where do I work?").is_empty(),
            "question must produce no facts"
        );
    }

    #[test]
    fn extract_facts_returns_empty_for_no_match() {
        assert!(
            extract_facts("the weather is nice today").is_empty(),
            "no-pattern input must produce no facts"
        );
        assert!(
            extract_facts("please summarize this document").is_empty(),
            "no-pattern input must produce no facts"
        );
        assert!(
            extract_facts("").is_empty(),
            "empty input must produce no facts"
        );
    }
}
