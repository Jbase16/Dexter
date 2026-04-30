# Phase 21 — Cross-Session Memory: Operator Fact Storage and Contextual Recall
## Spec version 1.1 — Session 021, 2026-03-15

> **Status:** NEXT PHASE.
> This document is the authoritative implementation guide for Phase 21.
> All architectural decisions are locked. Implement exactly as written.
>
> **Spec version history:**
> v1.0 — Initial spec
> v1.1 — Three corrections from retroactive review:
>   (1) `list_facts()` orphaned `let conn` line removed — `conn` is private, zero-vector
>       approach via `search_source()` is the sole implementation, no direct field access.
>   (2) Step 4c retrieval index calculation clarified — `take_while` from Phase 19 is
>       already adaptive; it automatically accounts for Phase 21's memory injection because
>       `prepare_messages_for_inference()` runs first. No index update needed in step 4c.
>   (3) `response_already_recorded` variable named precisely — declared at orchestrator.rs
>       line 651, set `true` at line 723 when the Phase 19 sentinel re-prompt path fires.

---

## 1. What Phase 21 Delivers

Dexter has no memory between sessions today. He knows what happened in the current
conversation (ConversationContext), but nothing about conversations from yesterday, last
week, or the first time you introduced yourself. The project proposal says: "He is already
running when you sit down. He has been paying attention." That statement is currently
false across session boundaries.

Phase 21 makes it true in two ways:

| Deliverable | What It Does |
|-------------|--------------|
| **Conversation turn embedding** | After each successful assistant response, the (user, assistant) exchange is embedded and stored in the VectorStore under `source='memory'`. This accumulates across sessions automatically. |
| **Recall injection** | Before each inference call, the current user message is embedded and the VectorStore is queried for relevant prior turns. Entries above a similarity threshold are injected as a `[Memory: ...]` system message. The model sees relevant past context without having to be told. |
| **Explicit memory commands** | "remember X" upserts a named fact. "forget X" deletes it. "what do you know about me?" lists all operator-specified facts. These are fast-pathed before routing — no inference overhead for storage/retrieval operations on the operator's terms. |
| **Phase 20 pre-work** | Two issues flagged in the Phase 20 retroactive review: (1) race condition in `capture_screen()` from fixed `/tmp/dexter_screen.png` path — fixed with per-invocation UUID path. (2) Missing test for combined vision + retrieval-first message ordering — added. |

**What this does NOT include:**
- Implicit fact extraction from conversation without explicit operator instruction — too
  noisy, too many false facts. Explicit "remember X" is the right activation for v1.
- Fact conflict detection / merging when contradictory facts exist for the same key.
- Memory decay / importance weighting over time — all stored turns are equal priority.
- Fine-tuning or LoRA adaptation based on stored conversations — out of scope entirely.
- Retrieval-from-local-store integration with Phase 9's `detect_pre_trigger` — Phase 22.

**Test count target:** 238 Rust passing (currently 224). 14 new tests.

---

## 2. What Already Exists (Do Not Rebuild)

| Component | Phase | Relevance |
|-----------|-------|-----------|
| `VectorStore` | 9 | Already has `source`, `entry_type`, `session_id`, `created_at`, and `embedding` columns. Schema was intentionally over-built for this moment. |
| `MemoryEntry` struct | 9 | Already exposes all needed fields including `source`, `entry_type`, `session_id`. |
| `VectorStore::insert()` | 9 | Used by retrieval pipeline for web docs. Phase 21 calls it for conversation turns. |
| `VectorStore::search()` | 9 | Full-table cosine search. Phase 21 adds `search_source()` filtering. |
| `InferenceEngine::embed()` | 9 | Used by retrieval pipeline. Phase 21 reuses it for turn embedding. |
| `RetrievalPipeline` | 9 | Single owner of `VectorStore`. Phase 21 adds memory methods to it; the orchestrator retains `Box<dyn RetrievalPipelineTrait>` unchanged. |
| `RetrievalPipelineTrait` | 19 | The trait that `MockRetrievalPipeline` implements. New memory methods added to the trait get default no-op implementations so existing mocks don't break. |
| `MEMORY_DB_FILENAME` | 9 | `"memory.db"` — the VectorStore already persists to this file. Memory turns land in the same DB, different `source` value. |
| `RETRIEVAL_EMBED_DIM` | 9 | 1024 — embedding dimension for `mxbai-embed-large`. Used by VectorStore for all embeddings including memory turns. |
| `prepare_messages_for_inference()` | 19 | Gets a new optional `recall` parameter. Signature change is backward-compatible: all call sites pass the result of `recall_relevant()` (which returns an empty vec when there are no matches). |

---

## 3. Architecture

### 3.1 Storage model

The VectorStore `source` column discriminates memory from retrieval:

| source | entry_type | Who writes it | What it contains |
|--------|------------|---------------|-----------------|
| `"retrieval"` | `"document"` | Phase 9 web retriever | Fetched web page text |
| `"memory"` | `"turn"` | Phase 21 turn embedder | `"User: ...\nAssistant: ..."` |
| `"operator"` | `"fact"` | Phase 21 explicit command | `"remember X"` content |

**Why one DB, three sources rather than separate files:**
All three share the same cosine similarity infrastructure. A single SQLite connection handles
the mutex contract. Separate files would require separate `VectorStore` instances and
separate embedded connections — more complexity, no benefit. The `source` field provides
full isolation where needed (`search_source()` filters to one source at query time).

**Why `source='operator'` for explicit facts rather than `'memory'`:**
`recall_relevant()` (auto-triggered at inference time) queries `source='memory'` — conversation
turns. Explicit facts use a different source so they can be independently listed and managed
via "what do you know about me?" without being mixed with turn history. The recall injection
queries both sources via two separate calls and merges results ranked by similarity.

### 3.2 Memory command fast-path

Before routing (new step 0 in `handle_text_input`), `detect_memory_command()` checks the
raw input. On a match, the command is dispatched immediately and the function returns —
no routing, no inference, no TTS for the store/delete operations.

```
"remember that I'm building Dexter on Apple Silicon"
→ MemoryCommand::Remember("I'm building Dexter on Apple Silicon")
→ slug_id("I'm building Dexter on Apple Silicon") → "i_m_building_dexter_on_apple_silicon"
→ embed content → upsert into VectorStore(source='operator', entry_type='fact', id=slug)
→ send_text("Got it. I'll remember that.", is_final=true, trace_id)
→ return Ok(())

"forget that I'm building Dexter"
→ MemoryCommand::Forget("I'm building Dexter")
→ slug_id("I'm building Dexter") → "i_m_building_dexter"
→ delete from VectorStore where id = slug
→ send_text("Forgotten." or "I don't have that.", is_final=true, trace_id)
→ return Ok(())

"what do you know about me?"
→ MemoryCommand::List
→ list_facts() → Vec<MemoryEntry>
→ format as markdown list → send_text(formatted, is_final=true, trace_id)
→ return Ok(())
```

**Why no inference for Remember/Forget/List:**
These are deterministic operations with deterministic responses. Routing them through the
inference engine would add 200–800ms latency and a TTS activation for simple state
changes. The operator's intent is unambiguous — a fast-path confirmation ("Got it.") is
the correct UX, not a generated paragraph.

**Remember response does NOT go through TTS:**
The confirmation "Got it. I'll remember that." is sent as `is_final=true` text response
and the entity returns to IDLE without TTS. This keeps the memory command flow fast and
quiet — the operator is confirming a data operation, not starting a dialogue.

### 3.3 Recall injection flow

```
handle_text_input:
  step 0:  detect_memory_command() → None (regular query)
  step 1:  push user message to context
  step 2:  transition to THINKING
  step 3:  route to model tier
  step 3a: is_retrieval_first_query? (Phase 19)
  step 3b: Phase 9 pre-retrieval (Phase 9)
  step 3c: [NEW] recall_relevant() — embed user msg → search VectorStore(source='memory' + 'operator')
             → Vec<MemoryEntry> with similarity >= MEMORY_RECALL_THRESHOLD
  step 4:  prepare_messages_for_inference(recall=&recall_entries)
           → injects "[Memory: ...]" system message when recall is non-empty
  step 4c: Phase 9 retrieval injection
  step 4d: Phase 20 vision capture
  step 5+6: generation + streaming
  step 7:  TTS finalization + IDLE
  step 8:  record assistant reply
  step 8b: [NEW] embed_and_store_turn() — embed (user + assistant) turn → VectorStore
  step 9:  action execution / IDLE
```

**Embedding at step 3c adds ~10–20ms before inference.** This is invisible — the THINKING
state is already active from step 2, and the model's own prefill takes longer than the
embed call. The user sees no additional delay.

**Turn embedding at step 8b is awaited inline.** This adds ~10–20ms after the response is
already displayed and before the entity returns to IDLE. The slight THINKING→IDLE delay
is imperceptible. A detached `tokio::spawn` would require `RetrievalPipeline` to be
`Clone` or `Arc`-wrapped — unnecessary complexity for a non-blocking operation that's
already off the display-critical path.

### 3.4 Memory injection format

When `recall_relevant()` returns 1–3 entries, `prepare_messages_for_inference()` injects
a single system message immediately after the context snapshot (index 2):

```
[Memory: {entry1.content} | {entry2.content} | {entry3.content}]
```

For facts (source='operator'), the content is the raw operator statement:
```
[Memory: I'm building Dexter on Apple Silicon | My name is Jason]
```

For conversation turns (source='memory'), the content is the stored `"User: ...\nAssistant: ..."` string.

The `|` separator keeps the injection compact. The model reads this as additional system
context — it does not affect the conversation turn count or trigger truncation.

**Why a single system message rather than separate messages per entry:**
Multiple injected system messages create ambiguity about message ordering and inflate
the apparent context size. A single `[Memory: ...]` message is unambiguous, easy to
identify in debug output, and clear to the model as a block of recalled context.

### 3.5 slug_id for fact deduplication

`slug_id(content: &str) -> String`:
1. Lowercase
2. Keep only alphanumeric + whitespace
3. Split on whitespace → join with `_`
4. Truncate to 48 chars

Examples:
- `"I'm building Dexter"` → `"i_m_building_dexter"`
- `"I'm building Dexter on Apple Silicon right now"` → `"i_m_building_dexter_on_apple_silicon_right_now_n"`
  (truncated to 48 — intentional, prevents unboundedly long IDs)

This provides natural deduplication: `"remember I'm building Dexter"` and `"remember that I'm
building Dexter right now"` share the same slug prefix and upsert rather than duplicate.
Imperfect — two genuinely different facts might collide at the 48-char cutoff — but collision
is better than an ever-growing fact table with duplicates.

### 3.6 Phase 20 pre-work details

**Race condition fix:**

Current `capture_screen()` writes to `/tmp/dexter_screen.png` — fixed path, race
condition under concurrent vision queries (second write clobbers first before read).

Fix:
1. Rename constant: `SCREEN_CAPTURE_PATH` → `SCREEN_CAPTURE_PATH_PREFIX = "/tmp/dexter_screen"`
2. In `capture_screen()`: generate per-invocation path:
   ```rust
   let path = format!("{SCREEN_CAPTURE_PATH_PREFIX}_{}.png", uuid::Uuid::new_v4().as_simple());
   ```
   Use `path` (local `String`) throughout the method instead of `SCREEN_CAPTURE_PATH`.
3. Update existing test `screen_capture_constants_are_valid` to reference the renamed constant.

**Combined path test (Phase 19 + Phase 20):**

Add test `vision_image_attaches_to_user_message_skipping_trailing_tool_result`:
- Construct a message list: `[system, user("look at this"), retrieval("Safari 18.3")]`
- Run `messages.iter_mut().rev().find(|m| m.role == "user")` and attach a fake image
- Assert the user message has `images = Some([...])`
- Assert the retrieval message has `images = None`
- Assert the system message has `images = None`

This confirms the combined Phase 19 + Phase 20 path (vision query with preceding
retrieval injection) correctly attaches the image to the user message, not the
trailing tool_result.

**Why the combined path is safe for the Ollama API:**
The Ollama `/api/chat` multimodal API places no ordering constraints on which messages
follow a message with `images`. The `images` field is an attachment on the user message
— the model uses it for its turn regardless of subsequent system/tool messages. The
`"retrieval"` role is a non-standard role that Ollama passes through as plain text context.
The vision model processes all messages as a flat context window.

---

## 4. Files Changed

| File | Change |
|------|--------|
| `constants.rs` | Rename `SCREEN_CAPTURE_PATH` → `SCREEN_CAPTURE_PATH_PREFIX`; add `MEMORY_SOURCE_CONVERSATION`, `MEMORY_SOURCE_OPERATOR`, `MEMORY_RECALL_THRESHOLD`, `MEMORY_RECALL_TOP_N` |
| `retrieval/store.rs` | Add `upsert()`, `delete()`, `search_source()` to `VectorStore` |
| `retrieval/pipeline.rs` | Add `recall_relevant()`, `embed_and_store_turn()`, `store_fact()`, `delete_fact()`, `list_facts()` to `RetrievalPipeline`; update `RetrievalPipelineTrait` with default stubs |
| `memory/mod.rs` | New file — module declaration |
| `memory/commands.rs` | New file — `MemoryCommand`, `detect_memory_command()`, `slug_id()` |
| `lib.rs` | Add `pub mod memory;` |
| `orchestrator.rs` | Step 0 (memory command fast-path); step 3c (recall); step 8b (turn embedding); update `prepare_messages_for_inference()` signature; Phase 20 race fix; update existing test for renamed constant |

---

## 5. New Tests (14 total → 238 passing)

### `retrieval/store.rs` — 4 tests

```
vector_store_upsert_replaces_existing_content_by_id
  Uses VectorStore::in_memory(). Insert id="k1". Upsert id="k1" with different content.
  search() returns one entry with updated content, not two entries.

vector_store_delete_returns_true_for_existing_id
  Insert id="k1". delete("k1") returns true. search() returns empty.

vector_store_delete_returns_false_for_missing_id
  Empty store. delete("nope") returns false. No panic.

vector_store_search_source_filters_by_source_field
  Insert two entries: source='memory' and source='retrieval', both with identical embeddings.
  search_source(&embedding, 10, "memory") returns the memory entry, not the retrieval entry.
  search_source(&embedding, 10, "retrieval") returns the retrieval entry, not the memory entry.
```

### `memory/commands.rs` — 5 tests

```
detect_remember_command_with_that_prefix
  "remember that my setup uses Apple Silicon" → Remember("my setup uses Apple Silicon")

detect_remember_command_without_that
  "remember I prefer dark mode" → Remember("I prefer dark mode")

detect_forget_command_strips_that_prefix
  "forget that I'm using Python 3.14" → Forget("I'm using Python 3.14")

detect_list_command_what_do_you_know
  "what do you know about me?" → List
  "what do you remember about me" → List

detect_no_memory_command_for_normal_query
  "remember the time we deployed that server?" → None (not a storage command — question form)
  "how do you forget things?" → None
```

### `orchestrator.rs` — 5 tests

```
recall_injection_formats_entries_as_memory_system_message
  Build a Vec<MemoryEntry> with two entries. Call
  prepare_messages_for_inference(&recall_entries). Assert one system message contains
  "[Memory:" and both entry contents separated by " | ".

recall_injection_absent_when_recall_is_empty
  prepare_messages_for_inference(&[]) produces messages with no "[Memory:" system message.

vision_image_attaches_to_user_message_skipping_trailing_tool_result
  Constructs [system, user("look at this"), retrieval("Safari 18.3")].
  Runs the Phase 20 step 4d image-attachment logic.
  Asserts user message gets images = Some([fake_b64]).
  Asserts retrieval message has images = None.
  Asserts system message has images = None.

screen_capture_path_prefix_generates_unique_per_call_paths
  Two calls to format!("{SCREEN_CAPTURE_PATH_PREFIX}_{}.png", uuid::Uuid::new_v4().as_simple())
  produce different strings. Each starts with "/tmp/dexter_screen_". Each ends with ".png".
  (Tests the constant + path generation pattern without spawning a subprocess.)

slug_id_is_deterministic_and_lowercase
  slug_id("I'm building Dexter") == "i_m_building_dexter".
  slug_id("I'm building Dexter") == slug_id("I'm building Dexter") (deterministic).
  slug_id("") == "".
  slug_id produces no uppercase characters.
```

---

## 6. Implementation Guide

### Step 1 — Phase 20 race fix (constants.rs)

Rename `SCREEN_CAPTURE_PATH` to `SCREEN_CAPTURE_PATH_PREFIX` and change its value and
comment:

```rust
/// Path prefix for per-invocation screen capture temp files.
///
/// Each `capture_screen()` call appends `_{uuid}.png` to produce a unique path:
/// `format!("{SCREEN_CAPTURE_PATH_PREFIX}_{}.png", uuid::Uuid::new_v4().as_simple())`
///
/// A prefix rather than a fixed path eliminates the race condition where two concurrent
/// vision queries would clobber each other's capture files. The prefix is under `/tmp`
/// so each invocation file is still ephemeral and not in the operator's home directory.
pub const SCREEN_CAPTURE_PATH_PREFIX: &str = "/tmp/dexter_screen";
```

Add the four new memory constants:

```rust
// ── Memory (Phase 21) ────────────────────────────────────────────────────────

/// VectorStore `source` value for automatically-embedded conversation turns.
pub const MEMORY_SOURCE_CONVERSATION: &str = "memory";

/// VectorStore `source` value for operator-specified explicit facts ("remember X").
pub const MEMORY_SOURCE_OPERATOR: &str = "operator";

/// Minimum cosine similarity score for a recalled memory entry to be injected
/// into inference context. Below this threshold: the entry is ignored.
///
/// 0.65 is chosen to pass obviously relevant entries ("I'm building Dexter" recalled
/// for a Dexter-related question) while excluding loosely related entries that
/// would add noise without useful grounding. Tune in Phase 22 if needed.
pub const MEMORY_RECALL_THRESHOLD: f32 = 0.65;

/// Maximum number of memory entries injected into a single inference request.
///
/// Three entries fits comfortably in a "[Memory: a | b | c]" single system message
/// without inflating context length. Increasing this risks prompt pollution;
/// decreasing it may miss relevant context. Tune in Phase 22 if needed.
pub const MEMORY_RECALL_TOP_N: usize = 3;
```

### Step 2 — VectorStore additions (retrieval/store.rs)

Add three methods to `impl VectorStore`. Add them after the existing `search()` method.

**`upsert()`** — identical to `insert()` except uses `INSERT OR REPLACE`:

```rust
/// Upsert a memory entry by `id`. If `id` already exists, the row is replaced.
///
/// Used for operator facts ("remember X") where repeated "remember X" should
/// update the stored value rather than silently ignore the new content.
/// Contrast with `insert()` which uses `INSERT OR IGNORE` (silently skips duplicates).
pub fn upsert(
    &self,
    id:         &str,
    content:    &str,
    source:     &str,
    entry_type: &str,
    session_id: Option<&str>,
    embedding:  &[f32],
) -> Result<(), rusqlite::Error> {
    let blob = embedding_to_blob(embedding);
    let created_at = Utc::now().to_rfc3339();
    let conn = self.conn.lock().expect("VectorStore mutex poisoned");
    conn.execute(
        "INSERT OR REPLACE INTO memory
         (id, content, source, entry_type, session_id, created_at, embedding)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, content, source, entry_type, session_id, created_at, blob],
    )?;
    Ok(())
}
```

**`delete()`** — delete a single row by primary key:

```rust
/// Delete the entry with the given `id`. Returns `true` if a row was deleted,
/// `false` if no row with that `id` existed.
pub fn delete(&self, id: &str) -> Result<bool, rusqlite::Error> {
    let conn = self.conn.lock().expect("VectorStore mutex poisoned");
    let rows_changed = conn.execute(
        "DELETE FROM memory WHERE id = ?1",
        params![id],
    )?;
    Ok(rows_changed > 0)
}
```

**`search_source()`** — cosine search filtered to a single `source` value:

```rust
/// Like `search()` but restricts the candidate pool to entries with
/// `source = filter_source`.
///
/// Used by the memory recall path to search only conversation turns
/// (`source='memory'`) or only operator facts (`source='operator'`),
/// without contaminating results with web retrieval documents.
pub fn search_source(
    &self,
    query_embedding: &[f32],
    limit:           usize,
    filter_source:   &str,
) -> Result<Vec<MemoryEntry>, rusqlite::Error> {
    let conn = self.conn.lock().expect("VectorStore mutex poisoned");
    let mut stmt = conn.prepare(
        "SELECT id, content, source, entry_type, session_id, created_at, embedding
         FROM memory WHERE source = ?1",
    )?;

    let mut scored: Vec<(f32, MemoryEntry)> = stmt
        .query_map(params![filter_source], |row| {
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
            let sim = cosine_similarity(query_embedding, &row_emb);
            (sim, MemoryEntry { id, content, source, entry_type, session_id, created_at, similarity: sim })
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    Ok(scored.into_iter().map(|(_, entry)| entry).collect())
}
```

### Step 3 — New `memory/` module

**`src/rust-core/src/memory/mod.rs`:**

```rust
pub mod commands;
pub use commands::{MemoryCommand, detect_memory_command, slug_id};
```

**`src/rust-core/src/memory/commands.rs`:**

```rust
/// A memory management command detected in the operator's raw input.
///
/// Detected by `detect_memory_command()` before routing. On a match, the orchestrator
/// handles the operation directly and returns without going through inference.
#[derive(Debug, PartialEq)]
pub enum MemoryCommand {
    /// "remember [that] X" — upsert X as an operator fact.
    Remember(String),
    /// "forget [that] X" — delete the fact identified by slug_id(X).
    Forget(String),
    /// "what do you know about me?" and variants — list all operator facts.
    List,
}

/// Detect whether `text` is a memory management command.
///
/// Pattern matching only — no inference. Returns `None` for regular queries.
/// Matching is case-insensitive; the payload is returned with original casing
/// (the operator's phrasing is stored verbatim in the VectorStore content field).
///
/// Patterns:
/// - Remember: starts with "remember that " or "remember " (then captures the rest)
/// - Forget:   starts with "forget that " or "forget " (then captures the rest)
/// - List:     exact or near-exact match against a small set of known phrasings
pub fn detect_memory_command(text: &str) -> Option<MemoryCommand> {
    let lower = text.trim().to_lowercase();

    // Remember — "remember [that] X"
    // Ordered: try "remember that" first to strip "that" from the payload.
    if let Some(rest) = lower.strip_prefix("remember that ") {
        let original = &text.trim()[("remember that ".len())..];
        let _ = rest; // lower version used only for detection; store original casing
        return Some(MemoryCommand::Remember(original.trim().to_string()));
    }
    if let Some(_) = lower.strip_prefix("remember ") {
        let original = &text.trim()[("remember ".len())..];
        // Guard: "remember when..." / "remember the time..." are questions, not commands.
        // Heuristic: if the payload starts with a WH-word or "the", treat as question.
        let payload_lower = original.trim().to_lowercase();
        if payload_lower.starts_with("when ")
            || payload_lower.starts_with("the ")
            || payload_lower.starts_with("how ")
            || payload_lower.starts_with("what ")
        {
            return None;
        }
        return Some(MemoryCommand::Remember(original.trim().to_string()));
    }

    // Forget — "forget [that] X"
    if let Some(_) = lower.strip_prefix("forget that ") {
        let original = &text.trim()[("forget that ".len())..];
        return Some(MemoryCommand::Forget(original.trim().to_string()));
    }
    if let Some(_) = lower.strip_prefix("forget ") {
        let original = &text.trim()[("forget ".len())..];
        return Some(MemoryCommand::Forget(original.trim().to_string()));
    }

    // List — exact known phrasings only. Not a prefix match — prevents false positives.
    const LIST_TRIGGERS: &[&str] = &[
        "what do you know about me",
        "what do you know about me?",
        "what do you remember about me",
        "what do you remember about me?",
        "list what you know",
        "list what you know about me",
        "show me what you know",
        "show me what you know about me",
        "what do you remember",
    ];
    if LIST_TRIGGERS.iter().any(|t| lower == *t) {
        return Some(MemoryCommand::List);
    }

    None
}

/// Generate a stable identifier for a memory fact from its content.
///
/// Used as the VectorStore `id` for operator facts, providing natural deduplication:
/// "remember I'm building Dexter" and "remember that I'm building Dexter today" produce
/// the same slug prefix and upsert rather than creating duplicate entries.
///
/// Algorithm: lowercase → keep alphanumeric + space → split on whitespace → join with '_'
/// → truncate to 48 characters.
///
/// 48 chars: long enough to be unique for most real phrases, short enough to be readable
/// in logs and query output. Collision risk at truncation boundary is low in practice —
/// operator facts tend to be short and distinctive.
pub fn slug_id(content: &str) -> String {
    content
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
        .chars()
        .take(48)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_remember_command_with_that_prefix() {
        let cmd = detect_memory_command("remember that my setup uses Apple Silicon");
        assert_eq!(cmd, Some(MemoryCommand::Remember("my setup uses Apple Silicon".to_string())));
    }

    #[test]
    fn detect_remember_command_without_that() {
        let cmd = detect_memory_command("remember I prefer dark mode");
        assert_eq!(cmd, Some(MemoryCommand::Remember("I prefer dark mode".to_string())));
    }

    #[test]
    fn detect_forget_command_strips_that_prefix() {
        let cmd = detect_memory_command("forget that I'm using Python 3.14");
        assert_eq!(cmd, Some(MemoryCommand::Forget("I'm using Python 3.14".to_string())));
    }

    #[test]
    fn detect_list_command_what_do_you_know() {
        assert_eq!(detect_memory_command("what do you know about me?"), Some(MemoryCommand::List));
        assert_eq!(detect_memory_command("what do you remember about me"), Some(MemoryCommand::List));
    }

    #[test]
    fn detect_no_memory_command_for_question_form() {
        // "remember the time..." and "remember when..." are questions, not storage commands.
        assert_eq!(detect_memory_command("remember the time we deployed that server?"), None);
        assert_eq!(detect_memory_command("remember when this broke?"), None);
        // Unrelated queries must not match.
        assert_eq!(detect_memory_command("how do you forget things?"), None);
        assert_eq!(detect_memory_command("what is your favorite programming language?"), None);
    }

    #[test]
    fn slug_id_is_deterministic_and_lowercase() {
        assert_eq!(slug_id("I'm building Dexter"), "i_m_building_dexter");
        assert_eq!(slug_id("I'm building Dexter"), slug_id("I'm building Dexter"));
        assert_eq!(slug_id(""), "");
        assert!(slug_id("ANY CAPS INPUT").chars().all(|c| !c.is_uppercase()),
            "slug_id output must be all-lowercase");
    }
}
```

### Step 4 — RetrievalPipeline additions (retrieval/pipeline.rs)

Add to `impl RetrievalPipeline`. Import `crate::memory::slug_id` at the top of the file.
Add the constants import: `MEMORY_SOURCE_CONVERSATION`, `MEMORY_SOURCE_OPERATOR`,
`MEMORY_RECALL_THRESHOLD`, `MEMORY_RECALL_TOP_N` from `crate::constants`.

**`recall_relevant()`** — embed the user query and return relevant memory entries:

```rust
/// Query the VectorStore for memory entries (turns + operator facts) relevant to
/// the current user message. Returns entries with similarity >= MEMORY_RECALL_THRESHOLD,
/// ordered by descending similarity, capped at MEMORY_RECALL_TOP_N.
///
/// Queries both `source='memory'` (conversation turns) and `source='operator'` (explicit
/// facts) and merges results, re-sorting by similarity before truncating. This gives
/// explicit operator facts a fair chance to surface even if their similarity to the
/// current query is slightly lower than a relevant conversation turn.
///
/// Returns an empty vec if:
/// - The VectorStore has no memory entries (common on first session).
/// - The embedding call fails (logs a warning, degrades gracefully).
/// - No entries exceed MEMORY_RECALL_THRESHOLD.
pub async fn recall_relevant(
    &self,
    engine:      &InferenceEngine,
    embed_model: &str,
    query:       &str,
) -> Vec<MemoryEntry> {
    let embedding = match engine.embed(EmbeddingRequest {
        model_name: embed_model.to_string(),
        input:      query.to_string(),
    }).await {
        Ok(e)  => e,
        Err(e) => {
            warn!(error = %e, "Memory recall: embed failed — skipping recall injection");
            return vec![];
        }
    };

    // Query both memory sources and merge.
    let mut results: Vec<MemoryEntry> = Vec::new();
    if let Ok(turns) = self.store.search_source(&embedding, MEMORY_RECALL_TOP_N, MEMORY_SOURCE_CONVERSATION) {
        results.extend(turns);
    }
    if let Ok(facts) = self.store.search_source(&embedding, MEMORY_RECALL_TOP_N, MEMORY_SOURCE_OPERATOR) {
        results.extend(facts);
    }

    // Filter by threshold, re-sort combined results, truncate.
    results.retain(|e| e.similarity >= MEMORY_RECALL_THRESHOLD);
    results.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity)
        .unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(MEMORY_RECALL_TOP_N);
    results
}
```

**`embed_and_store_turn()`** — embed a completed exchange and store it:

```rust
/// Embed the completed (user + assistant) exchange and store it in the VectorStore.
///
/// Called after step 8 (recording assistant reply). The `content` parameter is
/// the pre-formatted turn string: `"User: {user}\nAssistant: {assistant}"`.
/// The `id` is the `trace_id` of the turn — guarantees uniqueness per turn.
///
/// Uses `INSERT OR IGNORE` (via `VectorStore::insert`): if a turn with this trace_id
/// already exists (e.g., retry after partial failure), the existing entry is kept.
///
/// Silently skips on embed failure — a missing memory turn is not a fatal error.
pub async fn embed_and_store_turn(
    &self,
    engine:      &InferenceEngine,
    embed_model: &str,
    session_id:  &str,
    trace_id:    &str,
    content:     &str,
) {
    let embedding = match engine.embed(EmbeddingRequest {
        model_name: embed_model.to_string(),
        input:      content.to_string(),
    }).await {
        Ok(e)  => e,
        Err(e) => {
            warn!(error = %e, "Memory: embed failed — turn not stored");
            return;
        }
    };

    if let Err(e) = self.store.insert(
        trace_id,
        content,
        MEMORY_SOURCE_CONVERSATION,
        "turn",
        Some(session_id),
        &embedding,
    ) {
        warn!(error = %e, "Memory: VectorStore insert failed — turn not stored");
    }
}
```

**`store_fact()`**, **`delete_fact()`**, **`list_facts()`**:

```rust
/// Upsert an operator fact. `slug` is generated by `slug_id(content)` at the call site.
pub async fn store_fact(
    &self,
    engine:      &InferenceEngine,
    embed_model: &str,
    slug:        &str,
    content:     &str,
) {
    let embedding = match engine.embed(EmbeddingRequest {
        model_name: embed_model.to_string(),
        input:      content.to_string(),
    }).await {
        Ok(e)  => e,
        Err(e) => {
            warn!(error = %e, "Memory: embed failed — fact not stored");
            return;
        }
    };

    if let Err(e) = self.store.upsert(
        slug,
        content,
        MEMORY_SOURCE_OPERATOR,
        "fact",
        None,
        &embedding,
    ) {
        warn!(error = %e, "Memory: VectorStore upsert failed — fact not stored");
    }
}

/// Delete an operator fact by its slug. Returns true if deleted, false if not found.
pub fn delete_fact(&self, slug: &str) -> bool {
    self.store.delete(slug).unwrap_or(false)
}

/// Return all operator facts (source='operator') unordered.
///
/// Uses the zero-vector trick: passes a zero-magnitude query embedding to `search_source()`.
/// `cosine_similarity` returns 0.0 for any vector paired with a zero-norm vector (early-return
/// guard prevents NaN). All entries therefore score 0.0 and none are excluded by a threshold
/// filter (there is no threshold in `search_source` — it returns all candidates up to `limit`).
/// A limit of 1000 is effectively unbounded for any realistic operator fact count.
///
/// Does NOT access `self.store.conn` directly — `conn` is a private field on `VectorStore`.
/// All access goes through `search_source()`. This is intentional: no new query variant is
/// needed for a single call site that retrieves all facts without similarity ordering.
pub fn list_facts(&self) -> Vec<MemoryEntry> {
    let zero = vec![0.0f32; crate::constants::RETRIEVAL_EMBED_DIM];
    self.store.search_source(&zero, 1000, MEMORY_SOURCE_OPERATOR).unwrap_or_default()
}
```

**IMPORTANT:** `list_facts()` uses the zero-vector trick to retrieve all operator facts
without a meaningful similarity score. This avoids adding a separate "scan all by source"
method to VectorStore for a single call site. The 1000 limit is effectively unbounded for
any realistic operator fact count (operators don't have thousands of stored facts).

**Trait additions (RetrievalPipelineTrait):**

Add the new methods with default no-op implementations so `MockRetrievalPipeline` doesn't
need to change unless a test specifically needs memory behavior:

```rust
async fn recall_relevant(&self, _: &InferenceEngine, _: &str, _: &str) -> Vec<MemoryEntry> {
    vec![]
}
async fn embed_and_store_turn(&self, _: &InferenceEngine, _: &str, _: &str, _: &str, _: &str) {}
async fn store_fact(&self, _: &InferenceEngine, _: &str, _: &str, _: &str) {}
fn delete_fact(&self, _: &str) -> bool { false }
fn list_facts(&self) -> Vec<MemoryEntry> { vec![] }
```

Note: Rust traits do not permit default implementations for `async fn` on stable Rust.
Use the `#[async_trait]` pattern if the project already imports it, OR implement as:

```rust
fn recall_relevant<'a>(&'a self, ...) -> Pin<Box<dyn Future<Output = Vec<MemoryEntry>> + Send + 'a>> {
    Box::pin(async { vec![] })
}
```

Check the existing `RetrievalPipelineTrait` definition for the pattern already in use —
use the same approach for consistency. Do not introduce a new dependency.

### Step 5 — Add `pub mod memory;` to lib.rs

```rust
pub mod memory;
```

Add it after the existing `pub mod retrieval;` line.

### Step 6 — Orchestrator changes (orchestrator.rs)

**Imports:** Add to the `use crate::` block:
```rust
crate::memory::{MemoryCommand, detect_memory_command, slug_id},
```

Add to the constants import:
```rust
MEMORY_RECALL_THRESHOLD, MEMORY_RECALL_TOP_N,
MEMORY_SOURCE_OPERATOR, MEMORY_SOURCE_CONVERSATION,  // used in step 8b + memory commands
SCREEN_CAPTURE_PATH_PREFIX,  // renamed from SCREEN_CAPTURE_PATH
```

Remove `SCREEN_CAPTURE_PATH` from the import (renamed).

**Phase 20 race fix in `capture_screen()`:**
Replace `SCREEN_CAPTURE_PATH` with the per-invocation UUID path:

```rust
let path = format!("{SCREEN_CAPTURE_PATH_PREFIX}_{}.png", uuid::Uuid::new_v4().as_simple());
// Use `path` (local String, not the constant) throughout the rest of capture_screen().
```

Replace all four occurrences of `SCREEN_CAPTURE_PATH` in `capture_screen()` with `&path`
(or `path.as_str()`).

**`prepare_messages_for_inference()` signature change:**

Old: `fn prepare_messages_for_inference(&self) -> Vec<Message>`
New: `fn prepare_messages_for_inference(&self, recall: &[MemoryEntry]) -> Vec<Message>`

Where to inject recall: immediately after the context snapshot injection (after index
`insert_pos`), before returning `messages`.

```rust
// After existing context snapshot injection block:

// Phase 21 — Recall injection.
// Format non-empty recall entries as a single "[Memory: ...]" system message.
// Placed after context (index 2) so the model reads: personality → context → memory → history.
if !recall.is_empty() {
    let memory_text: String = recall.iter()
        .map(|e| e.content.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    let insert_at = messages.iter().take_while(|m| m.role == "system").count();
    messages.insert(insert_at, Message::system(format!("[Memory: {memory_text}]")));
}
```

**Update all call sites of `prepare_messages_for_inference()`:**
There are three call sites:
1. `handle_text_input` step 4 — pass `&recall_entries` (computed in new step 3c)
2. Phase 19 re-prompt path (inside the intercepted-q handling block) — pass `&[]` (re-prompt
   already has retrieved context; adding memory would be redundant noise)
3. `handle_proactive_trigger` — pass `&[]` (proactive observations are not user-initiated
   queries; memory injection doesn't apply)

**New step 0 in `handle_text_input` (memory command fast-path):**

Add before step 1 (before `self.context.push_user(&content)`):

```rust
// 0. [Phase 21] Memory command fast-path.
//
// Check if the operator is issuing an explicit memory management command.
// These are handled without routing, inference, or TTS — a deterministic
// confirmation is sent directly. The function returns after the command is
// handled, skipping all inference machinery.
if let Some(cmd) = detect_memory_command(&content) {
    self.send_state(EntityState::Thinking, &trace_id).await?;
    match cmd {
        MemoryCommand::Remember(fact) => {
            let slug  = slug_id(&fact);
            let model = self.model_config.embed.clone();
            self.retrieval.store_fact(&self.engine, &model, &slug, &fact).await;
            self.send_text("Got it. I'll remember that.", true, &trace_id).await?;
        }
        MemoryCommand::Forget(target) => {
            let slug = slug_id(&target);
            let found = self.retrieval.delete_fact(&slug);
            let reply = if found { "Forgotten." } else { "I don't have that stored." };
            self.send_text(reply, true, &trace_id).await?;
        }
        MemoryCommand::List => {
            let facts = self.retrieval.list_facts();
            let reply = if facts.is_empty() {
                "I don't have anything stored about you yet.".to_string()
            } else {
                facts.iter()
                    .enumerate()
                    .map(|(i, e)| format!("{}. {}", i + 1, e.content))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            self.send_text(&reply, true, &trace_id).await?;
        }
    }
    self.send_state(EntityState::Idle, &trace_id).await?;
    return Ok(());
}
```

**New step 3c (recall query):**

Add after step 3b (Phase 9 pre-retrieval) and before step 4 (prepare_messages_for_inference):

```rust
// 3c. [Phase 21] Memory recall — embed current user message and query VectorStore
//     for relevant prior turns and operator facts.
//
//     Returns empty vec if the store has no memory entries (first session, or all
//     entries below threshold). Empty recall → no "[Memory: ...]" injection.
//
//     Skipped for retrieval-first queries (retrieval_first_done = true) — those
//     queries are already grounded in fresh web content; memory injection would
//     add noise without benefit. Skipped for vision queries — the image provides
//     the primary context.
let recall_entries: Vec<crate::retrieval::store::MemoryEntry> =
    if retrieval_first_done || decision.model == ModelId::Vision {
        vec![]
    } else {
        let embed_model = self.model_config.embed.clone();
        self.retrieval.recall_relevant(&self.engine, &embed_model, &content).await
    };
```

**Step 4c retrieval index — no change required:**

The Phase 9 retrieval injection in step 4c (which runs after `prepare_messages_for_inference()`)
already uses:

```rust
let retrieval_idx = messages.iter()
    .take_while(|m| m.role == "system")
    .count();
messages.insert(retrieval_idx, Message::system(injection));
```

This `take_while` approach was introduced in Phase 19 precisely to replace the Phase 16
`context_injected` boolean, which would have broken here. Because `prepare_messages_for_inference()`
runs first and inserts the `[Memory: ...]` system message *inside* it, the `messages` vec
that step 4c operates on already contains personality + context (maybe) + memory (maybe).
`take_while(...).count()` counts however many leading system messages are present — 1, 2,
or 3 — and inserts the retrieval context after all of them. The final ordering is:

```
[0] personality
[1] context snapshot (if focused app exists)
[2] [Memory: ...] (if recall non-empty)          ← injected by prepare_messages_for_inference()
[3] [Retrieved: ...] (if Phase 9 retrieval fired) ← inserted by step 4c take_while
[4..] conversation history
```

**Do not change the step 4c index calculation.** The `take_while` expression is self-correcting
for any number of injected system messages. Any attempt to use a hardcoded integer or an
additional `recall_injected` boolean would be a regression to the Phase 16 fragility that
Phase 19 fixed.

**New step 8b (turn embedding after recording):**

Add after step 8 (`self.context.push_assistant(&full_response)`):

```rust
// 8b. [Phase 21] Embed and store the completed turn for cross-session recall.
//
//     The full_response is already displayed. The ~10-20ms embed + insert happens
//     here (still under THINKING display, but THINKING→IDLE transition is step 9).
//
//     Gate condition: `response_already_recorded` is a local `bool` declared at the
//     top of handle_text_input's intercepted-q handling block:
//       `let mut response_already_recorded = false;`  (line ~651 in orchestrator.rs)
//     It is set to `true` at line ~723, inside the `if let Some(ref query) = intercepted_q`
//     block, immediately after the Phase 19 re-prompt response is recorded to context.
//
//     When `response_already_recorded == true`:
//       - The Phase 19 sentinel path fired.
//       - `full_response` contains only the PRE-sentinel partial text (not the full
//         exchange). The re-prompt response was recorded inline inside the sentinel block.
//       - Embedding `full_response` here would store an incomplete turn. Skip it.
//
//     When `response_already_recorded == false`:
//       - Normal (non-intercepted) path. `full_response` is the complete assistant response.
//       - Store the full (user, assistant) exchange.
//
//     Memory command fast-paths return early (step 0) before reaching this point,
//     so they never trigger step 8b regardless of this flag.
if !full_response.is_empty() && !response_already_recorded {
    let turn_content = format!("User: {content}\nAssistant: {full_response}");
    let embed_model = self.model_config.embed.clone();
    self.retrieval.embed_and_store_turn(
        &self.engine,
        &embed_model,
        &self.session_id,
        &trace_id,
        &turn_content,
    ).await;
}
```

---

## 7. Acceptance Checklist

- [ ] AC-1   `SCREEN_CAPTURE_PATH` renamed to `SCREEN_CAPTURE_PATH_PREFIX`; value `"/tmp/dexter_screen"` (no `.png` suffix)
- [ ] AC-2   `capture_screen()` generates per-invocation path: `format!("{prefix}_{uuid}.png")`; no longer references fixed constant for the full path
- [ ] AC-3   `VectorStore::upsert()` exists; second call with same `id` replaces content, does not duplicate
- [ ] AC-4   `VectorStore::delete()` returns `true` for existing id, `false` for missing id
- [ ] AC-5   `VectorStore::search_source()` returns only entries matching `filter_source`
- [ ] AC-6   `memory/mod.rs` and `memory/commands.rs` exist; `pub mod memory;` in lib.rs
- [ ] AC-7   `detect_memory_command("remember X")` → `Remember(X)` (with and without "that")
- [ ] AC-8   `detect_memory_command("remember the time...")` → `None` (question guard)
- [ ] AC-9   `detect_memory_command("forget X")` → `Forget(X)` (with and without "that")
- [ ] AC-10  `detect_memory_command("what do you know about me?")` → `List`
- [ ] AC-11  `slug_id("I'm building Dexter")` == `"i_m_building_dexter"` (deterministic, lowercase)
- [ ] AC-12  Memory command fast-path in `handle_text_input`: Remember/Forget/List handled before routing; function returns `Ok(())` after reply
- [ ] AC-13  `recall_relevant()` on `RetrievalPipeline` queries both `source='memory'` and `source='operator'`; returns entries above `MEMORY_RECALL_THRESHOLD`; returns `vec![]` on embed failure
- [ ] AC-14  `embed_and_store_turn()` inserts a `"User: ...\nAssistant: ..."` entry with `source='memory'`, `entry_type='turn'` into VectorStore after step 8
- [ ] AC-15  `store_fact()` upserts with `source='operator'`, `entry_type='fact'`; repeated call with same slug replaces, does not duplicate
- [ ] AC-16  `delete_fact(slug)` returns `true` when found, `false` when not
- [ ] AC-17  `list_facts()` returns only `source='operator'` entries
- [ ] AC-18  `prepare_messages_for_inference(recall)` injects `[Memory: a | b]` system message when recall is non-empty
- [ ] AC-19  `prepare_messages_for_inference(&[])` produces no `[Memory:` system message
- [ ] AC-20  Recall injection position: after context snapshot system message, before conversation history
- [ ] AC-21  Re-prompt path passes `&[]` to `prepare_messages_for_inference` — no memory injection on re-prompts
- [ ] AC-22  Proactive trigger path passes `&[]` — no memory injection for proactive observations
- [ ] AC-23  Vision + retrieval-first combined path test: `vision_image_attaches_to_user_message_skipping_trailing_tool_result` passes
- [ ] AC-24  Phase 20 race fix: test `screen_capture_path_prefix_generates_unique_per_call_paths` passes
- [ ] AC-25  `cargo test` ≥ 238 passing, 0 failed
- [ ] AC-26  `cargo build` 0 warnings
- [ ] AC-27  `swift build` clean (no Swift changes this phase)
- [ ] AC-28  `uv run pytest` 19/19

---

## 8. Known Pitfalls

**Pitfall: `list_facts()` zero-vector trick and `debug_assert!` in `cosine_similarity`**

`cosine_similarity` contains:
```rust
debug_assert_eq!(a.len(), RETRIEVAL_EMBED_DIM, "query embedding must be RETRIEVAL_EMBED_DIM");
```
The zero-vector passed by `list_facts()` must be exactly `RETRIEVAL_EMBED_DIM` elements
(`vec![0.0f32; RETRIEVAL_EMBED_DIM]`). A shorter or longer vec trips the assert in debug
builds. The zero-norm early return (`if na == 0.0 { return 0.0 }`) fires for the zero
query vector — no NaN.

**Pitfall: `detect_memory_command` question guard is heuristic, not exhaustive**

The WH-word guard (`starts_with("when ")`, `"what "`, `"the "`, `"how "`) covers common
question forms but not all. "remember my project requires Python 3.14" correctly routes
to `Remember("my project requires Python 3.14")`. "remember that everyone uses Python"
also correctly routes to `Remember`. The guard is conservative — false negatives (questions
that slip through as Remember commands) are worse than false positives (valid facts rejected
as questions). Operators will correct by re-phrasing.

**Pitfall: slug collision at 48-char truncation**

Two facts that are identical in the first 48 slug characters will upsert rather than
create two entries. This is intentional (deduplication) but means "I'm working on Project
Alpha for client X" and "I'm working on Project Alpha for client Y" produce the same
slug if the difference is at position > 48. In practice, facts this similar are the same
fact with a different detail — upsert is correct behavior. Log at `debug` level when an
upsert replaces existing content (compare old vs new content before replacing to emit
the log message).

**Pitfall: `RetrievalPipelineTrait` default async methods**

Rust stable does not support `async fn` in trait definitions with default bodies directly.
If the project uses `#[async_trait]` macro (check existing trait definition for the
`async_trait::async_trait` attribute), apply it to the trait and new methods. If the
project uses `Pin<Box<dyn Future>>` return types manually, use that pattern for consistency.
Do not introduce `async-trait` as a new dependency if it's not already present — use the
existing pattern.

**Pitfall: recall skips in vision + retrieval-first paths**

Step 3c explicitly skips recall for `retrieval_first_done = true` and `decision.model == ModelId::Vision`. This is intentional — web retrieval already provides current-fact grounding; memory injection would be additive noise. A future phase could revisit combining recall + retrieval, but Phase 21's policy is conservative.

**Pitfall: TTS not activated for memory command responses**

Memory command responses ("Got it. I'll remember that.") are sent as `is_final=true`
text responses and return directly to IDLE. No TTS. This is intentional — they are
transactional confirmations, not conversational. If this feels wrong during integration
testing, the fix is to set `EntityState::Thinking` → run through the standard TTS path
— but that requires restructuring the fast-path. Leave it as text-only for Phase 21.

**Pitfall: step 8b `response_already_recorded` guard — exact variable location**

`response_already_recorded` is a local `bool` in `handle_text_input`, not a field on
`CoreOrchestrator`. It is declared as `let mut response_already_recorded = false;`
immediately before the `if let Some(ref query) = intercepted_q` block (~line 651), and
set to `true` at the end of that block (~line 723) after the re-prompt response has been
pushed to context and session.

When `true`, `full_response` contains only the partial pre-sentinel text — the re-prompt
response was accumulated separately inside the sentinel block and is already recorded.
Embedding `full_response` in this state would store an incomplete, mid-sentence turn in
the VectorStore. The `!response_already_recorded` guard prevents this.

When `false` (the common path), `full_response` is the complete assistant response for
this turn and is safe to embed.

---

## 9. Phase 22 Preview (not in scope here)

Phase 22 will integrate the local VectorStore into Phase 9's retrieval pipeline.
Currently `detect_pre_trigger` only fires web retrieval. Phase 22 adds a local-first
search: for retrieval-first queries, check `source='memory'` and `source='operator'`
entries first — if local similarity > 0.8, skip web retrieval entirely and use the local
result. This completes the vision in the project proposal: "he retrieves what he doesn't
know" — including things he already knows from prior context.

Phase 22 also evaluates fact-extraction from conversation: after sufficient data has
accumulated in `source='memory'`, patterns can be identified and promoted to
`source='operator'` facts automatically. This is the bridge to "He learns his operator."
