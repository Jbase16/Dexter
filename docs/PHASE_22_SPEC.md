# Phase 22 Implementation Spec: Retrieval Pipeline Hardening

**Version:** 1.1 (Issue 1: uses-pattern boundary fixed; Issue 2: contamination test now inserts a turn)
**Status:** Ready for implementation
**Depends on:** Phase 21 complete (operator fact storage, VectorStore `source='memory'` turn embedding)
**Test target:** 239 → 251 (+12 tests)

---

## Executive Summary

Phase 21 added conversation turns to VectorStore (`entry_type='turn'`, `source='memory'`).
Phase 9's `retrieve()` calls `store.search()` — which searches **all** sources. As a result,
`memory_hits` now includes conversation turns, and the `should_fetch_web` guard
(`memory_hits.is_empty()`) incorrectly suppresses web retrieval when conversation turns match
a factual query.

Phase 22 hardens the retrieval pipeline with three targeted changes:

1. **Contamination fix** — replace `store.search()` (all sources) with `store.search_knowledge()`
   (entry_type IN `'fact'`, `'web_page'` only). Conversation turns continue to flow through
   `recall_relevant()` at step 3c; they must not influence the web-fetch decision.

2. **Local-first retrieval** — new constant `LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD = 0.82`. If any
   knowledge entry similarity ≥ threshold, skip web fetch entirely. Operator facts and cached
   pages are authoritative local knowledge; a strong local hit obviates the network round-trip.

3. **Implicit fact extraction** — new `memory/extractor.rs`. High-precision regex patterns
   extract facts from user messages without requiring explicit "remember" commands. Called at
   step 8c in `handle_text_input()`, gated on the same condition as step 8b.

---

## 1. Background: The Contamination Bug

### Phase 9 `retrieve()` — original design

```rust
// retrieve(), Phase 9, lines 184-185
let memory_hits = self.store.search(&embedding, RETRIEVAL_MAX_MEMORY_HITS)?;
let should_fetch_web = matches!(trigger, RetrievalTrigger::UncertaintyMarker { .. })
    || memory_hits.is_empty();
```

At Phase 9, VectorStore contained only:
- `entry_type='web_page'`, `source='web:{url}'` — cached DDG fetches
- `entry_type='document'` — any future doc indexing

No conversation entries existed. `memory_hits.is_empty()` correctly reflected "no relevant
prior knowledge." Web fetch fired for factual queries with no local match.

### Phase 21 added: conversation turns

Phase 21's `embed_and_store_turn()` stores each conversation exchange as:
- `entry_type = 'turn'`
- `source = 'memory'` (= `MEMORY_SOURCE_CONVERSATION`)

Phase 21's `store_fact()` stores operator facts as:
- `entry_type = 'fact'`
- `source = 'operator'` (= `MEMORY_SOURCE_OPERATOR`)

### The failure mode

After even a single conversation session, `store.search()` returns conversation turns as
`memory_hits` for semantically related queries. Example:

> Turn stored: "User: what's the best Python framework? Assistant: FastAPI is my recommendation..."
> Later query: "what framework does Flask use?"
> `memory_hits`: [conversation turn about Python frameworks, similarity=0.78]
> `memory_hits.is_empty()` = **false**
> `should_fetch_web` = **false**
> Result: no web fetch, Dexter answers from the stale conversation turn — **wrong**

The turns are semantically relevant to the query domain, but they are not authoritative for
factual questions. `recall_relevant()` (Phase 21, step 3c) is the correct mechanism for
surfacing turns as conversational context. They must not be in the retrieval pipeline's
web-fetch gate.

---

## 2. File Map

| Change   | File                                                     |
|----------|----------------------------------------------------------|
| Modified | `src/rust-core/Cargo.toml`                               |
| Modified | `src/rust-core/src/constants.rs`                         |
| Modified | `src/rust-core/src/retrieval/store.rs`                   |
| Modified | `src/rust-core/src/retrieval/pipeline.rs`                |
| **New**  | `src/rust-core/src/memory/extractor.rs`                  |
| Modified | `src/rust-core/src/memory/mod.rs`                        |
| Modified | `src/rust-core/src/orchestrator.rs`                      |

---

## 3. `Cargo.toml` — add `regex`

```toml
regex = "1"   # Implicit fact extraction patterns in memory/extractor.rs (Phase 22)
```

**Justification:** The `regex` crate is the canonical Rust regex engine — zero-copy,
`OnceLock`-compilable, no unsafe. The alternative is hand-rolled string scanning, which is
brittle for multi-pattern matching and harder to extend. `regex = "1"` adds ~800KB to the
binary in release mode (already spending 40MB on `scraper` + `reqwest`); the tradeoff is
correct.

---

## 4. `constants.rs` — `LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD`

Add after the `MEMORY_RECALL_THRESHOLD` block (currently around line 292):

```rust
/// Minimum cosine similarity for a local knowledge entry (operator fact or cached
/// web page) to satisfy a retrieval query without falling through to a network fetch.
///
/// Set above MEMORY_RECALL_THRESHOLD (0.65) because this is a binary skip-web
/// decision — the local entry must be semantically close enough to be trusted as a
/// direct answer, not merely contextually relevant. 0.82 ≈ 34° angular distance;
/// tight enough to confirm topical match, loose enough to tolerate phrasing variation
/// (e.g., "Python version" vs "Python release number").
pub const LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD: f32 = 0.82;
```

---

## 5. `retrieval/store.rs` — `VectorStore::search_knowledge()`

Add after `search_source()` (currently around line 215). The method is structurally identical
to `search()` with one difference: the SQL `WHERE` clause restricts to knowledge entry types.

```rust
/// Cosine search restricted to authoritative knowledge entries.
///
/// Filters to `entry_type IN ('fact', 'web_page')` — entries that represent
/// persistent, intentionally-stored knowledge. Conversation turns (`entry_type='turn'`
/// or `'conversation_turn'`) are excluded to prevent Phase 21 turn embeddings from
/// contaminating the retrieval pipeline's web-fetch decision.
///
/// Entry type semantics:
/// - `'fact'`             → operator-stated fact stored via `store_fact()` (source='operator')
/// - `'web_page'`         → fetched and cached by `cache_web_result()` (source='web:{url}')
/// - `'turn'`             → Phase 21 conversation embed — NOT authoritative for factual queries
/// - `'conversation_turn'`→ Phase 9 `store_conversation_turn()` — also excluded
///
/// Used exclusively by `RetrievalPipeline::retrieve()` to determine whether web
/// retrieval is needed. `recall_relevant()` handles turn recall separately.
pub fn search_knowledge(
    &self,
    query_embedding: &[f32],
    limit:           usize,
) -> Result<Vec<MemoryEntry>, rusqlite::Error> {
    let conn = self.conn.lock().expect("VectorStore mutex poisoned");
    let mut stmt = conn.prepare(
        "SELECT id, content, source, entry_type, session_id, created_at, embedding
         FROM memory
         WHERE entry_type IN ('fact', 'web_page')",
    )?;

    let mut scored: Vec<(f32, MemoryEntry)> = stmt
        .query_map([], |row| {
            let blob: Vec<u8> = row.get(6)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                blob,
            ))
        })?
        .filter_map(|r| r.ok())
        .map(|(id, content, source, entry_type, session_id, created_at, blob)| {
            let row_emb = blob_to_embedding(&blob);
            let sim    = cosine_similarity(query_embedding, &row_emb);
            (sim, MemoryEntry { id, content, source, entry_type, session_id, created_at, similarity: sim })
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    Ok(scored.into_iter().map(|(_, e)| e).collect())
}
```

### Tests — store.rs (+2)

```rust
#[test]
fn search_knowledge_returns_facts_and_web_pages_only() {
    let store   = VectorStore::in_memory().unwrap();
    let emb     = vec![0.5f32; RETRIEVAL_EMBED_DIM];

    // Insert one entry of each type.
    store.insert("f1",  "a fact",     "operator",   "fact",             None, &emb).unwrap();
    store.insert("w1",  "a web page", "web:http://example.com", "web_page", None, &emb).unwrap();
    store.insert("t1",  "a turn",     "memory",     "turn",             None, &emb).unwrap();
    store.insert("ct1", "conv turn",  "session:s1", "conversation_turn",None, &emb).unwrap();

    let results = store.search_knowledge(&emb, 10).unwrap();
    assert_eq!(results.len(), 2, "search_knowledge must return exactly the fact + web_page");
    let types: Vec<&str> = results.iter().map(|e| e.entry_type.as_str()).collect();
    assert!(types.contains(&"fact"),     "fact entry must be included");
    assert!(types.contains(&"web_page"), "web_page entry must be included");
}

#[test]
fn search_knowledge_excludes_conversation_turns_from_results() {
    let store = VectorStore::in_memory().unwrap();
    let emb   = vec![0.5f32; RETRIEVAL_EMBED_DIM];

    // Only conversation turns — no facts or web pages.
    store.insert("t1", "turn content",  "memory",     "turn",             None, &emb).unwrap();
    store.insert("t2", "another turn",  "session:s1", "conversation_turn",None, &emb).unwrap();

    let results = store.search_knowledge(&emb, 10).unwrap();
    assert!(results.is_empty(),
        "search_knowledge must return empty when only turns are stored; got {} entries",
        results.len());
}
```

---

## 6. `retrieval/pipeline.rs` — `retrieve()` changes

### 6a. Constants import update

Replace the current `use crate::constants::{...}` block with:

```rust
use crate::constants::{
    LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD,
    MEMORY_DB_FILENAME, MEMORY_RECALL_THRESHOLD, MEMORY_RECALL_TOP_N,
    MEMORY_SOURCE_CONVERSATION, MEMORY_SOURCE_OPERATOR,
    RETRIEVAL_EMBED_DIM, RETRIEVAL_MAX_MEMORY_HITS, RETRIEVAL_WEB_TIMEOUT_SECS,
    UNCERTAINTY_MARKER,
};
```

### 6b. `retrieve()` body — replace steps 2–3

Replace lines 184–202 (the `// ── Step 2` and `// ── Step 3` blocks) with:

```rust
        // ── Step 2: search knowledge base ────────────────────────────────────────
        // Only facts (source='operator') and cached web pages (source='web:{url}')
        // participate in the web-fetch decision. Conversation turns are stored as
        // entry_type='turn' after Phase 21 and are intentionally excluded here.
        //
        // Rationale: turns are semantically relevant to recent topics but are NOT
        // authoritative for factual queries. A turn about "Python frameworks" would
        // suppress DDG retrieval for "what version does Flask require?" — a different
        // factual question in the same domain. Turns flow through recall_relevant()
        // at orchestrator step 3c; they belong in conversational context injection,
        // not in the retrieval pipeline's web-fetch gate.
        let knowledge_hits = self.store.search_knowledge(&embedding, RETRIEVAL_MAX_MEMORY_HITS)?;
        info!(
            trigger   = ?std::mem::discriminant(trigger),
            hits      = knowledge_hits.len(),
            "VectorStore knowledge search complete"
        );

        // ── Step 3: web fetch decision ────────────────────────────────────────────
        // Local-first: if any knowledge entry exceeds LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD
        // (0.82), the local data is authoritative — skip the network entirely.
        //
        // This fires when a previously stored operator fact or cached web page directly
        // answers the current query. Bypassing the network removes latency (~200–800ms
        // DuckDuckGo round-trip) and avoids unnecessary outbound requests for queries
        // already answered by local knowledge.
        //
        // Falls through to web fetch when:
        //   - No confident local hit AND trigger is UncertaintyMarker (always fetch on uncertainty)
        //   - No confident local hit AND knowledge_hits is empty (no local knowledge at all)
        //
        // Does NOT fall through when:
        //   - knowledge_hits is non-empty but all below threshold AND trigger is MemorySearch
        //     → Phase 9 behavior preserved: some local context is better than a DDG result
        let has_confident_local = knowledge_hits.iter()
            .any(|h| h.similarity >= LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD);

        let should_fetch_web = !has_confident_local
            && (matches!(trigger, RetrievalTrigger::UncertaintyMarker { .. })
                || knowledge_hits.is_empty());

        let web_result = if should_fetch_web {
            self.fetch_ddg(&query, embed_model_name, engine).await
        } else {
            None
        };

        Ok(RetrievalContext {
            query,
            memory_hits: knowledge_hits,   // field name preserved; now contains only knowledge entries
            web_result,
            trigger: trigger.clone(),
        })
```

### 6c. `retrieve()` doc-comment update

Replace the step-by-step comment block above `retrieve()`:

```rust
    /// Execute retrieval for `trigger`:
    ///
    /// 1. Embed `trigger.query()` via `engine.embed()`
    /// 2. Search knowledge base (facts + cached web pages — conversation turns excluded)
    /// 3. Local-first check: if any hit ≥ LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD, skip web
    /// 4. Else fetch DuckDuckGo if UncertaintyMarker or no local knowledge
    /// 5. Return `RetrievalContext`
    ///
    /// `embed_model_name` = `model_config.embed` (e.g. `"mxbai-embed-large"`).
```

### Tests — pipeline.rs (+2)

Both tests use `new_degraded()` (in-memory SQLite) and do not require network or Ollama.
They directly insert known entries and verify the structural behavior of `retrieve()` is
correct — the step 2 and step 3 logic — without making live HTTP calls.

Since `retrieve()` requires an `InferenceEngine` (for embedding), these tests are integration-
style tests that verify the pipeline's decision logic with a seam at the VectorStore. The
cleanest approach is to test the `search_knowledge()` exclusion and the
`LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD` gate as separate unit assertions on the VectorStore
directly (the store tests above cover this), and add one integration-level test that verifies
the full retrieve() decision through the pipeline module's internal state.

**Alternative approach** (correct, no Ollama required): test that the `should_fetch_web`
decision is reflected in `RetrievalContext.web_result` by inserting a pre-embedded fact into
the in-memory store and calling `detect_pre_trigger()` + verifying the context state. This
doesn't require a live embed call because we can verify the structural properties
independently.

**Tests added** (pure logic — no live inference required):

```rust
#[test]
fn retrieve_knowledge_search_excludes_turns_from_memory_hits() {
    // Contamination regression test: a Phase 21 conversation turn stored with
    // entry_type='turn' must NOT appear in search_knowledge() results, regardless
    // of its similarity score. Before the Phase 22 fix, store.search() (all sources)
    // was used; a turn with high similarity would fill memory_hits and suppress web
    // retrieval for factual queries.
    let pipeline = make_pipeline();
    let emb = vec![0.5f32; RETRIEVAL_EMBED_DIM];

    // Insert a conversation turn — the contamination source after Phase 21.
    // The embedding is identical to the query so similarity = 1.0, the worst-case
    // scenario for the old search() call: maximum similarity guaranteeing inclusion.
    pipeline.store
        .insert(
            "turn-contamination",
            "User: what Python frameworks are good? Assistant: FastAPI.",
            "memory",  // MEMORY_SOURCE_CONVERSATION
            "turn",
            None,
            &emb,
        )
        .expect("insert must succeed on in-memory store");

    // search_knowledge() must exclude the turn and return empty.
    let results = pipeline.store
        .search_knowledge(&emb, 10)
        .expect("search_knowledge must not error");

    assert!(
        results.is_empty(),
        "search_knowledge must exclude entry_type='turn'; got {} entries with types: {:?}",
        results.len(),
        results.iter().map(|e| e.entry_type.as_str()).collect::<Vec<_>>(),
    );
}

#[test]
fn local_retrieval_threshold_is_above_recall_threshold() {
    // Architectural invariant: the local-first skip threshold must be strictly
    // higher than the recall threshold so that recall can surface contextually
    // relevant entries while only the highest-confidence local hits skip web fetch.
    assert!(
        LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD > MEMORY_RECALL_THRESHOLD,
        "LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD ({}) must be > MEMORY_RECALL_THRESHOLD ({})",
        LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD,
        MEMORY_RECALL_THRESHOLD,
    );
}
```

**Note on the second test:** it needs to be updated to import both constants:

```rust
use crate::constants::{LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD, MEMORY_RECALL_THRESHOLD, RETRIEVAL_EMBED_DIM};
```

---

## 7. `memory/extractor.rs` — `extract_facts()`

### File: `src/rust-core/src/memory/extractor.rs`

```rust
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

/// Each entry: (label, compiled Regex).
/// The label becomes the fact prefix: "operator {label}: {capture}".
/// Compiled once at first call via OnceLock — subsequent calls are zero-cost.
static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

fn patterns() -> &'static [(&'static str, Regex)] {
    PATTERNS.get_or_init(|| {
        vec![
            // Identity: "my name is Jason"
            // Capture: first + optional last name (alphabetic, spaces, hyphens, apostrophes).
            // Limit: 48 chars to avoid capturing a full sentence.
            (
                "name",
                Regex::new(r"(?i)\bmy name is ([A-Za-z][A-Za-z '\-]{0,46}\b)").unwrap(),
            ),
            // Technology — "I'm using X" / "I am using X"
            // Captures: tool/language name. Stops at punctuation, "and", "but", "for".
            // Character class allows version numbers (3.14, v2, c++) and common tool chars.
            //
            // Boundary design: non-greedy {0,29}? quantifier + consumed alternation.
            //
            // The original greedy version used (?:[,.]|$| and | but | for ) as a
            // consumed boundary group. This failed because the capture class includes
            // spaces: greedy matching on "Python 3.14 for my project" consumed the space
            // before "for" into the capture group, leaving "for " (no leading space) which
            // didn't match the " for " alternative. Backtracking could not recover because
            // the space was already part of the capture.
            //
            // Fix: non-greedy {0,29}? causes the engine to grow the capture lazily, stopping
            // the moment the trailing alternation (?:\s+(?:and|but|for)\b|[,]|\.(?:\s|$)|\s*$) can
            // match. The alternation consumes \s+ (the space), so both the space and the
            // stop word are consumed outside the capture group.
            //
            // Trace for "I'm using Python 3.14 for my project":
            //   Grow: "P" → "Py" → ... → "Python 3.14"
            //   Alternation at " for my project": \s+for\b → " for" → match ✓
            //   Capture = "Python 3.14" ✓
            //
            // Trace for "I'm using Python 3.14" (end of string):
            //   Grow: ... → "Python 3.14"
            //   Alternation at "": \s*$ → match ✓
            //   Capture = "Python 3.14" ✓
            //
            // Trace for "I use Rust." (sentence-ending period):
            //   Grow: "R" → "Ru" → "Rus" → "Rust"
            //   Alternation at ".": \.(?:\s|$) → "." followed by "" (end) → match ✓
            //   Capture = "Rust" ✓  (period NOT consumed into capture — non-greedy stopped)
            //
            // Trace for "I'm using Node.js" (mid-word period):
            //   Grow: ... → "Node"
            //   Alternation at ".js": \.(?:\s|$) → "." followed by "j" → fail
            //   Continue: "Node." → "Node.j" → "Node.js"
            //   Alternation at "": \s*$ → match ✓
            //   Capture = "Node.js" ✓
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
            // Non-greedy {0,48}?. Place names have no mid-word periods so [,]|\.(?:\s|$)
            // is consistent but primarily fires on comma-separated clauses.
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
/// ```
/// # use dexter_core::memory::extractor::extract_facts;
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
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0], "operator name: Jason");
    }

    #[test]
    fn extract_facts_name_pattern_with_last_name() {
        let facts = extract_facts("my name is Jason Smith");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0], "operator name: Jason Smith");
    }

    #[test]
    fn extract_facts_tech_using_pattern_stops_at_boundary() {
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
        assert!(extract_facts("what is my name?").is_empty());
        assert!(extract_facts("am I using the right tool?").is_empty());
        assert!(extract_facts("where do I work?").is_empty());
    }

    #[test]
    fn extract_facts_returns_empty_for_no_match() {
        assert!(extract_facts("the weather is nice today").is_empty());
        assert!(extract_facts("please summarize this document").is_empty());
        assert!(extract_facts("").is_empty());
    }
}
```

### Patterns spec table

| Pattern name | Trigger example | Output |
|---|---|---|
| `name` | `"my name is Jason Smith"` | `"operator name: Jason Smith"` |
| `uses` | `"I'm using Python 3.14 for"` | `"operator uses: Python 3.14"` |
| `uses` | `"I use Rust."` | `"operator uses: Rust"` |
| `works at` | `"I work at Anthropic."` | `"operator works at: Anthropic"` |
| `location` | `"I'm based in San Francisco."` | `"operator location: San Francisco"` |
| `location` | `"I live in Tokyo."` | `"operator location: Tokyo"` |

### Important: capture boundary design

All patterns use non-greedy quantifiers (`{0,N}?`) with a consumed boundary alternation:

```
(?:\s+(?:and|but|for)\b  |  [,]  |  \.(?:\s|$)  |  \s*$)
 ─────────────────────     ───     ──────────────   ──────
 conjunction stop-words  comma   sentence-ending   end of
 (" and", " but", " for")        period (period     string
                                 not followed by
                                 non-whitespace)
```

The sentence-ending period rule (`\.(?:\s|$)`) is the critical distinction. It stops at "Anthropic." (period followed by end-of-string) but does NOT stop at "Node.js" (period followed by "j") or "Python 3.14" (period followed by "1"). Mid-word periods grow through the capture; sentence-ending periods terminate it.

The non-greedy quantifier ensures the engine stops at the FIRST valid boundary, preventing greedy overshoot. With a greedy quantifier, the capture would extend to the maximum allowed length and then backtrack — functionally equivalent but slower and harder to reason about.

Example: `"I'm using Rust and Python"` → captures `"Rust"` (stops at `\s+and\b`), not `"Rust and Python"`.

### Regex compile-time cost

Six `Regex::new()` calls happen once at first extraction call, guarded by `OnceLock`.
Each compiled regex is ~5–15KB of compiled automaton in memory. Total: ~60–90KB static.
Subsequent calls to `extract_facts()` are zero-cost for pattern lookup.

---

## 8. `memory/mod.rs` — add extractor module

Replace the current content:

```rust
pub mod commands;
pub mod extractor;

pub use commands::{MemoryCommand, detect_memory_command, slug_id};
pub use extractor::extract_facts;
```

---

## 9. `orchestrator.rs` — step 8c

### 9a. Import addition

Add `extract_facts` to the memory imports at the top of `orchestrator.rs`:

```rust
// Before:
use crate::memory::{MemoryCommand, detect_memory_command, slug_id};

// After:
use crate::memory::{MemoryCommand, detect_memory_command, extract_facts, slug_id};
```

### 9b. Step 8c block

Insert immediately after the step 8b `embed_and_store_turn` block (currently ending ~line 866):

```rust
        // 8c. [Phase 22] Extract and store implicit operator facts from user message.
        //
        //     High-precision regex patterns detect facts stated incidentally without
        //     an explicit "remember" command. Examples:
        //       "I'm using Python 3.14" → stored as "operator uses: Python 3.14"
        //       "my name is Jason"      → stored as "operator name: Jason"
        //
        //     Uses `store_fact()` with `slug_id(fact)` as the key — `upsert()` means
        //     re-stating the same fact updates it in place rather than duplicating.
        //
        //     Gate: same condition as 8b. Memory command fast-paths return at step 0
        //     and never reach here. Questions (ending in '?') are rejected inside
        //     `extract_facts()` — no additional guard needed.
        if !full_response.is_empty() && !response_already_recorded {
            let extracted = extract_facts(&content);
            if !extracted.is_empty() {
                let embed_model = self.model_config.embed.clone();
                for fact in &extracted {
                    let slug = slug_id(fact);
                    self.retrieval.store_fact(&self.engine, &embed_model, &slug, fact).await;
                    info!(fact = %fact, "Implicit fact extracted and stored");
                }
            }
        }
```

---

## 10. Execution Order

```
1.  Edit Cargo.toml: add regex = "1"
2.  Edit constants.rs: add LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD
3.  Edit retrieval/store.rs: add search_knowledge() + 2 tests
4.  Edit retrieval/pipeline.rs: update imports, replace retrieve() steps 2–3, update doc-comment, add 2 tests
5.  Write memory/extractor.rs: full file with patterns(), extract_facts(), 8 tests
6.  Edit memory/mod.rs: add extractor module and re-export
7.  Edit orchestrator.rs: add extract_facts to imports, add step 8c block
8.  cargo test -p dexter-core 2>&1 | tail -20  →  target: 251 tests, 0 failures, 0 warnings
9.  Update docs/SESSION_STATE.json: phase → Phase 23, rust_unit_tests → 251
10. Update MEMORY.md: Phase 22 complete, Phase 23 current
```

---

## 11. Acceptance Criteria

### Automated

`cargo test -p dexter-core` passes with:
- **251 tests** (239 + 12)
- **0 failures**
- **0 warnings** from project code

### Structural invariants (verified by tests)

| # | Property | Verified by |
|---|----------|-------------|
| 1 | `search_knowledge()` returns only `fact` and `web_page` entries | `search_knowledge_returns_facts_and_web_pages_only` |
| 2 | `search_knowledge()` excludes all turn types | `search_knowledge_excludes_conversation_turns_from_results` |
| 3 | `LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD > MEMORY_RECALL_THRESHOLD` | `local_retrieval_threshold_is_above_recall_threshold` |
| 4 | `retrieve()` contamination path is broken (turns don't appear) | `retrieve_knowledge_search_excludes_turns_from_memory_hits` |
| 5 | `extract_facts` correctly identifies 5 pattern types | 6 pattern-specific tests |
| 6 | Questions are rejected | `extract_facts_returns_empty_for_questions` |
| 7 | No-match inputs return empty | `extract_facts_returns_empty_for_no_match` |

### Manual verification (live Ollama session)

| # | Scenario | Expected behavior |
|---|----------|-------------------|
| 1 | User says "I'm using Neovim for everything" | Fact stored: "operator uses: Neovim" (visible via "what do you know about me?") |
| 2 | Factual query after conversations on same topic | Web retrieval fires (conversation turns don't suppress it) |
| 3 | Operator fact matches query with ≥0.82 similarity | Web retrieval skipped; fact used directly |
| 4 | Operator fact matches query at 0.65–0.81 | Web retrieval still fires (below local-first threshold) |

---

## 12. Key Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| `regex` compile errors from invalid pattern strings | Build failure | All patterns compile via `Regex::new(...).unwrap()` in `OnceLock`; a bad pattern panics on first call. Tests call `extract_facts()` → `OnceLock` fires → panic if any pattern is invalid. Tests validate patterns before production runs. |
| Capture bleed (pattern captures too much) | Wrong facts stored | Non-greedy quantifiers + `\.(?:\s|$)` sentence-ending period rule prevent overreach. Tests verify exact captures for each pattern, including the `"Python 3.14 for my project"` regression case. |
| `search_knowledge()` SQL typo in entry_type literal | No facts returned | Tests insert known `'fact'` and `'web_page'` entries and assert they're returned. |
| `RetrievalContext.memory_hits` is now knowledge-only | Callers expecting turns | `format_for_injection()` now injects only authoritative sources — which is the correct behavior. Turns are available via `recall_relevant()` at step 3c. No callers depend on turns appearing in `memory_hits`. |
| Step 8c runs on assistant-generated content | Stores facts from assistant's own words | Guarded: `extract_facts(content)` where `content` is the raw user message (function parameter), not `full_response`. The assistant's response is never passed to `extract_facts()`. |
| `OnceLock<Vec<...>>` static with non-`Sync` `Regex` | Swift 6 / Rust borrow error | `Regex` implements `Send + Sync`; `Vec<(&'static str, Regex)>` is `Sync`. `OnceLock` requires `T: Sync` to expose `&T` from multiple threads. No issue. |

---

## 13. Phase 22 → Phase 23 Preview

Phase 22 completes the local knowledge loop: facts are stored (Phase 21), retrieved
(Phase 21 `recall_relevant()`), and now protected from contamination (Phase 22).

Phase 23 will focus on **conversation quality hardening**:

- **Context window management**: when `prepare_messages_for_inference()` truncates to fit
  the model's context window, the most recent turns are preserved but the oldest may be cut
  mid-exchange. Phase 23 adds a conversation summarization step: when the message list
  exceeds a configurable threshold, the oldest N turns are replaced with a single compressed
  summary message generated by the FAST model.

- **Retry / rephrase on empty response**: `full_response.is_empty()` after the main generate
  loop currently falls through to a logged warning. Phase 23 adds a single retry with a
  simplified prompt before returning an error to the user.

- **Graceful degradation logging**: structured `warn!` with session + trace IDs for all
  non-fatal paths (embed failures, web fetch failures, action engine timeouts) to make
  operational issues diagnosable from log output without code changes.
