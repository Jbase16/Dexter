//! Uncertainty sentinel interception for the streaming token pipeline.
//!
//! The model is instructed to emit `[UNCERTAIN: <query>]` when it encounters genuine
//! factual uncertainty. This module scans the token stream, intercepts the marker,
//! and returns the extracted query and any pre-marker text that should be flushed.
//!
//! The interceptor is single-use per generation call — it intercepts the first marker
//! and then reverts to passthrough. The orchestrator is responsible for restarting
//! the generation loop after retrieval completes and re-prompting.
//!
//! ## State machine
//!
//! ```text
//! Passthrough ─── sees "[UNCERTAIN:" ──► Capturing
//! Capturing   ─── sees "]"           ──► Intercepted (resets to Passthrough on next call)
//! Capturing   ─── buffer > MAX_QUERY_LEN ──► Passthrough (flush as literal text)
//! ```

const SENTINEL_PREFIX: &str = "[UNCERTAIN:";
const SENTINEL_CLOSE: char = ']';
/// Maximum characters allowed in the query portion of the sentinel.
/// Keeps the rolling buffer bounded; a longer "query" is almost certainly not a sentinel.
const MAX_QUERY_LEN: usize = 200;

/// State of the uncertainty scanner.
#[derive(Debug, PartialEq)]
enum State {
    /// Forwarding tokens to the caller unchanged.
    Passthrough,
    /// We saw `[UNCERTAIN:` — accumulating the query up to `]`.
    Capturing,
    /// A complete sentinel was intercepted. Returned once, then reset to Passthrough.
    Intercepted,
}

/// Wraps a streaming token source and intercepts `[UNCERTAIN: <query>]` markers.
///
/// Designed for a single model generation call. After the sentinel fires once the
/// interceptor returns to `Passthrough`, ensuring that a second marker in a follow-up
/// response can be detected fresh (the orchestrator constructs a new interceptor per
/// primary generation call — this is enforced by the ownership model: the interceptor
/// is created inside `generate_primary()` which is called once per primary generation).
pub struct UncertaintyInterceptor {
    state: State,
    buffer: String,
}

/// Result of processing a single token.
#[derive(Debug)]
pub enum InterceptorOutput {
    /// Text to forward to the UI / TTS pipeline immediately.
    /// May be empty — callers should skip empty strings rather than sending no-ops.
    Passthrough(String),
    /// Sentinel detected. `flush` is any pre-marker text that should be forwarded;
    /// `query` is the extracted retrieval query.
    Intercepted {
        flush: Option<String>,
        query: String,
    },
}

impl UncertaintyInterceptor {
    pub fn new() -> Self {
        Self {
            state: State::Passthrough,
            buffer: String::with_capacity(64),
        }
    }

    /// Process one token from the model's output stream.
    ///
    /// Returns `InterceptorOutput::Passthrough` for normal text and
    /// `InterceptorOutput::Intercepted` when the sentinel is fully received.
    ///
    /// After an `Intercepted` result, the interceptor resets to `Passthrough`
    /// (single-use: the orchestrator restarts generation for the re-prompted response).
    pub fn process(&mut self, token: &str) -> InterceptorOutput {
        match self.state {
            State::Passthrough | State::Intercepted => {
                // Reset state after a previous interception before accumulating.
                if self.state == State::Intercepted {
                    self.state = State::Passthrough;
                    self.buffer.clear();
                }
                self.buffer.push_str(token);
                self.scan_for_prefix()
            }
            State::Capturing => {
                self.buffer.push_str(token);
                self.scan_for_close()
            }
        }
    }

    // ── private helpers ────────────────────────────────────────────────────────

    /// Scan the buffer for the full `SENTINEL_PREFIX`.
    ///
    /// On finding the prefix: transition to Capturing, hand off to `scan_for_close()`.
    /// On finding a potential partial prefix at the tail: flush safe prefix, retain tail.
    /// On finding nothing: flush the entire buffer.
    fn scan_for_prefix(&mut self) -> InterceptorOutput {
        if let Some(idx) = self.buffer.find(SENTINEL_PREFIX) {
            // Everything before the sentinel prefix is safe to flush immediately.
            let pre_marker: String = self.buffer[..idx].to_string();
            // Retain everything AFTER "[UNCERTAIN:" for close-bracket scanning.
            let after = self.buffer[idx + SENTINEL_PREFIX.len()..].to_string();
            self.buffer = after;
            self.state = State::Capturing;

            // Check if the close bracket is already in the carried-over tail.
            let capturing_result = self.scan_for_close();
            return match capturing_result {
                InterceptorOutput::Intercepted { flush: _, query } => {
                    // Merge pre_marker into the flush field (it must be forwarded before retrieval).
                    InterceptorOutput::Intercepted {
                        flush: if pre_marker.is_empty() {
                            None
                        } else {
                            Some(pre_marker)
                        },
                        query,
                    }
                }
                other => {
                    // scan_for_close() returned Passthrough(text). Two sub-cases:
                    //
                    // (a) text.is_empty() — still accumulating the query (state stays
                    //     Capturing). Return pre_marker so the caller can forward it to
                    //     the UI while we continue waiting for the close bracket.
                    //
                    // (b) text.is_non_empty() — overflow guard fired (buffer exceeded
                    //     MAX_QUERY_LEN with no close bracket). State was reset to
                    //     Passthrough. The overflow text is the reconstructed literal
                    //     "[UNCERTAIN: ..." and must be forwarded verbatim. Combine with
                    //     pre_marker. Occurs when a single large token contains the prefix
                    //     plus more than MAX_QUERY_LEN chars (e.g. in tests; unusual in
                    //     production tokenisers but must be handled correctly).
                    if let InterceptorOutput::Passthrough(overflow) = other {
                        let combined = if pre_marker.is_empty() {
                            overflow
                        } else if overflow.is_empty() {
                            pre_marker
                        } else {
                            format!("{}{}", pre_marker, overflow)
                        };
                        InterceptorOutput::Passthrough(combined)
                    } else {
                        // Unreachable: Intercepted was handled by the first match arm above.
                        debug_assert!(false, "unreachable: Intercepted must be handled above");
                        InterceptorOutput::Passthrough(pre_marker)
                    }
                }
            };
        }

        // Check for a potential in-progress prefix at the tail of the buffer.
        // e.g., buffer ends with "[UNCE" — might be the start of "[UNCERTAIN:".
        // We must not flush those bytes yet; they may complete the prefix next token.
        let prefix_tail = Self::longest_prefix_tail(&self.buffer, SENTINEL_PREFIX);
        if prefix_tail > 0 {
            // Flush everything before the potential prefix start — it is definitely safe.
            let safe_end = self.buffer.len() - prefix_tail;
            let flush: String = self.buffer[..safe_end].to_string();
            self.buffer = self.buffer[safe_end..].to_string();
            InterceptorOutput::Passthrough(flush)
        } else {
            // No sentinel in sight — flush the whole buffer.
            let flush = self.buffer.clone();
            self.buffer.clear();
            InterceptorOutput::Passthrough(flush)
        }
    }

    /// Scan the buffer for the `SENTINEL_CLOSE` bracket while in Capturing state.
    fn scan_for_close(&mut self) -> InterceptorOutput {
        if let Some(close_pos) = self.buffer.find(SENTINEL_CLOSE) {
            let query: String = self.buffer[..close_pos].trim().to_string();
            // Drop everything up to and including the close bracket.
            self.buffer = self.buffer[close_pos + 1..].to_string();
            self.state = State::Intercepted;
            return InterceptorOutput::Intercepted { flush: None, query };
        }

        // Buffer overflow guard: if we've accumulated more than MAX_QUERY_LEN characters
        // without seeing a close bracket, this is not a real sentinel — flush it all.
        if self.buffer.len() > MAX_QUERY_LEN {
            let flush = format!("{}{}", SENTINEL_PREFIX, self.buffer.clone());
            self.buffer.clear();
            self.state = State::Passthrough;
            return InterceptorOutput::Passthrough(flush);
        }

        // Still waiting for the close bracket — accumulate, return nothing.
        InterceptorOutput::Passthrough(String::new())
    }

    /// Returns the length of the longest suffix of `haystack` that is also a prefix of `needle`.
    ///
    /// Used to detect in-progress sentinel prefix matches at the tail of the buffer.
    /// If `haystack` ends with "[UNCE" and `needle` is "[UNCERTAIN:", returns 5.
    fn longest_prefix_tail(haystack: &str, needle: &str) -> usize {
        let hb = haystack.as_bytes();
        let nb = needle.as_bytes();
        let max_check = nb.len().min(hb.len());
        for len in (1..=max_check).rev() {
            if hb[hb.len() - len..] == nb[..len] {
                return len;
            }
        }
        0
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a sequence of tokens through a fresh interceptor and collect results.
    /// Returns (all_flushed_text_joined, Option<intercepted_query>).
    fn run_stream(tokens: &[&str]) -> (Vec<String>, Option<String>) {
        let mut ic = UncertaintyInterceptor::new();
        let mut output = Vec::new();
        let mut query = None;
        for token in tokens {
            match ic.process(token) {
                InterceptorOutput::Passthrough(t) => {
                    if !t.is_empty() {
                        output.push(t);
                    }
                }
                InterceptorOutput::Intercepted { flush, query: q } => {
                    if let Some(f) = flush {
                        if !f.is_empty() {
                            output.push(f);
                        }
                    }
                    query = Some(q);
                }
            }
        }
        (output, query)
    }

    #[test]
    fn interceptor_passes_through_clean_token_stream() {
        let tokens = &["The ", "answer ", "is ", "42."];
        let (flushed, query) = run_stream(tokens);
        // All tokens should pass through unchanged; no sentinel in input.
        assert_eq!(flushed.join(""), "The answer is 42.");
        assert!(
            query.is_none(),
            "no sentinel should yield no intercepted query"
        );
    }

    #[test]
    fn interceptor_detects_sentinel_mid_stream() {
        let tokens = &[
            "The answer is ",
            "[UNCERTAIN: current year]",
            " and nothing more.",
        ];
        let (flushed, query) = run_stream(tokens);
        // Intercepted query must be extracted precisely.
        assert_eq!(query.as_deref(), Some("current year"));
        // Pre-marker text ("The answer is ") must be flushed before the sentinel.
        assert!(
            flushed.iter().any(|t| t.contains("The answer is")),
            "pre-marker text must be flushed: got {:?}",
            flushed
        );
        // Post-sentinel text is dropped — orchestrator restarts generation for re-prompt.
    }

    #[test]
    fn interceptor_handles_split_token_sentinel() {
        // Sentinel split across multiple tokens — common in practice with sub-word tokenisers.
        let tokens = &["The ", "[UNCE", "RTAIN: latest rust version", "]", " done."];
        let (_, query) = run_stream(tokens);
        assert_eq!(
            query.as_deref(),
            Some("latest rust version"),
            "sentinel split across tokens must be correctly reassembled"
        );
    }

    #[test]
    fn interceptor_ignores_overlong_capture_buffer() {
        // No close bracket after [UNCERTAIN: prefix — overflow guard flushes as passthrough.
        let long_content: String = "x".repeat(250);
        let token = format!("[UNCERTAIN: {}", long_content); // no closing ]
        let tokens: Vec<&str> = vec![&token];
        let (flushed, query) = run_stream(&tokens);
        assert!(
            query.is_none(),
            "missing close bracket must not produce an intercepted query"
        );
        assert!(
            !flushed.is_empty(),
            "overlong buffer must be flushed as passthrough, not silently dropped"
        );
    }
}
