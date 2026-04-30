/// Sentence splitter for streaming TTS.
///
/// Buffers tokens from the inference stream; emits complete sentences when a
/// punctuation boundary is detected AND the text meets the minimum length.
/// The minimum-length guard prevents splitting on "Mr. Smith" or "e.g. a case"
/// where short leading text precedes `. `.
///
/// Boundaries: `. `, `! `, `? `, `\n\n`

use crate::constants::TTS_SENTENCE_MIN_CHARS;

pub struct SentenceSplitter {
    buffer: String,
}

impl SentenceSplitter {
    pub fn new() -> Self { Self { buffer: String::new() } }

    /// Push one inference token. Returns 0 or more complete sentences.
    pub fn push(&mut self, token: &str) -> Vec<String> {
        self.buffer.push_str(token);
        self.try_split()
    }

    /// Call when inference is complete. Returns remaining buffered text if non-empty.
    pub fn flush(&mut self) -> Option<String> {
        let s = self.buffer.trim().to_string();
        self.buffer.clear();
        if s.is_empty() { None } else { Some(s) }
    }

    fn try_split(&mut self) -> Vec<String> {
        let boundaries = [". ", "! ", "? ", "\n\n"];
        let mut results = Vec::new();
        loop {
            // Find the earliest boundary in the buffer.
            let earliest = boundaries.iter().filter_map(|b| {
                self.buffer.find(b).map(|pos| (pos + b.len(), *b))
            }).min_by_key(|(pos, _)| *pos);

            match earliest {
                Some((end, _)) if end >= TTS_SENTENCE_MIN_CHARS => {
                    let sentence = self.buffer[..end].trim().to_string();
                    self.buffer = self.buffer[end..].to_string();
                    if !sentence.is_empty() { results.push(sentence); }
                }
                _ => break,
            }
        }
        results
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_empty_token_returns_empty_vec() {
        let mut s = SentenceSplitter::new();
        assert!(s.push("").is_empty());
    }

    #[test]
    fn push_no_boundary_accumulates_without_splitting() {
        let mut s = SentenceSplitter::new();
        let result = s.push("Hello world");
        assert!(result.is_empty());
        // flush should return the accumulated text
        assert_eq!(s.flush(), Some("Hello world".to_string()));
    }

    #[test]
    fn push_boundary_at_or_above_min_length_returns_sentence() {
        let mut s = SentenceSplitter::new();
        // "Hello world. " is 13 chars including the ". " boundary — above TTS_SENTENCE_MIN_CHARS
        let result = s.push("Hello world. ");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "Hello world.");
    }

    #[test]
    fn push_boundary_below_min_length_does_not_split() {
        let mut s = SentenceSplitter::new();
        // "Mr. " is 4 chars — below TTS_SENTENCE_MIN_CHARS (10), should NOT split
        let result = s.push("Mr. ");
        assert!(result.is_empty(), "Short boundary 'Mr. ' should not trigger split");
    }

    #[test]
    fn push_multi_sentence_returns_all_in_order() {
        let mut s = SentenceSplitter::new();
        let result = s.push("Hello world. How are you? Fine thanks! ");
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "Hello world.");
        assert_eq!(result[1], "How are you?");
        assert_eq!(result[2], "Fine thanks!");
    }

    #[test]
    fn flush_with_remaining_text_returns_it() {
        let mut s = SentenceSplitter::new();
        s.push("Incomplete sentence");
        assert_eq!(s.flush(), Some("Incomplete sentence".to_string()));
        // Second flush of empty buffer returns None
        assert_eq!(s.flush(), None);
    }

    #[test]
    fn flush_empty_buffer_returns_none() {
        let mut s = SentenceSplitter::new();
        assert_eq!(s.flush(), None);
    }
}
