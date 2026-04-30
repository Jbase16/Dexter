/// VectorStore — Phase 9 semantic memory backend.
///
/// Stores embedding vectors as little-endian f32 BLOBs in SQLite and performs
/// cosine similarity search entirely in Rust. No sqlite-vec extension required —
/// the bundled `rusqlite` crate is fully self-contained, making the daemon binary
/// portable without any external `.dylib` at a known runtime path.
///
/// ## Why in-Rust cosine similarity
///
/// sqlite-vec requires a separately compiled native extension loaded at runtime via
/// `Connection::load_extension()`. Self-contained daemon constraint makes this
/// impractical. At Phase 9 scale (hundreds of entries), loading all embeddings into
/// a Vec<f32> and computing similarity in Rust is correct and fast. Migrate to
/// sqlite-vec in Phase 15 hardening once a stable Rust crate emerges.
///
/// ## Schema
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS memory (
///     id         TEXT NOT NULL PRIMARY KEY,
///     content    TEXT NOT NULL,
///     source     TEXT NOT NULL,
///     entry_type TEXT NOT NULL,
///     session_id TEXT,
///     created_at TEXT NOT NULL,
///     embedding  BLOB NOT NULL
/// );
/// CREATE INDEX IF NOT EXISTS memory_created_at ON memory(created_at);
/// ```
use std::{
    path::Path,
    sync::Mutex,
};

use chrono::Utc;
use rusqlite::{params, Connection};

use crate::constants::RETRIEVAL_EMBED_DIM;

// ── MemoryEntry ───────────────────────────────────────────────────────────────

/// One record returned from a VectorStore similarity search.
///
/// `similarity` is populated by `search()` as the cosine similarity score in
/// \[−1, 1\]. For entries not returned from search (e.g., inserted by hand),
/// `similarity` is 0.0.
#[allow(dead_code)] // Phase 10+ callers read entry_type, session_id, created_at
pub struct MemoryEntry {
    pub id:         String,
    pub content:    String,
    pub source:     String,
    pub entry_type: String,
    pub session_id: Option<String>,
    pub created_at: String,
    /// Cosine similarity in [−1, 1]. 0.0 for non-search contexts.
    pub similarity: f32,
}

// ── VectorStore ───────────────────────────────────────────────────────────────

/// SQLite-backed vector store.
///
/// The connection is wrapped in `Mutex` so that `VectorStore` is `Sync`. This is
/// required because `CoreOrchestrator` is used in a `tokio::spawn` task that requires
/// `Send`, and `send_text(&self, ...)` holds `&CoreOrchestrator` across `.await` points,
/// which requires `CoreOrchestrator: Sync` (i.e., `&CoreOrchestrator: Send`).
///
/// `rusqlite::Connection` is `Send` but NOT `Sync` (its internals use `RefCell`).
/// Wrapping in `Mutex<Connection>` makes `VectorStore: Sync` because `Mutex<T>: Sync`
/// when `T: Send`. In practice the orchestrator is single-threaded so the mutex
/// never has contention — the overhead is a single atomic CAS per operation.
pub struct VectorStore {
    conn: Mutex<Connection>,
}

impl VectorStore {
    /// Open or create the memory DB at `db_path`.
    ///
    /// Runs `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS` on every
    /// open — idempotent, so re-opening an existing DB is always safe. Uses
    /// `Connection::open(db_path)` (file-backed), NOT `open_in_memory()`.
    ///
    /// Use `VectorStore::in_memory()` (below) for tests and degraded-mode fallback.
    pub fn new(db_path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        Self::apply_schema(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Open an in-memory SQLite DB. Data is not persisted — used by
    /// `RetrievalPipeline::new_degraded()` and unit tests.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::apply_schema(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Apply the schema idempotently. Factored out to avoid duplication between
    /// `new()` and `in_memory()`.
    fn apply_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory (
                id         TEXT NOT NULL PRIMARY KEY,
                content    TEXT NOT NULL,
                source     TEXT NOT NULL,
                entry_type TEXT NOT NULL,
                session_id TEXT,
                created_at TEXT NOT NULL,
                embedding  BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS memory_created_at ON memory(created_at);",
        )
    }

    /// Insert a memory entry. `embedding` must have exactly `RETRIEVAL_EMBED_DIM`
    /// elements. Duplicate IDs are silently ignored (`INSERT OR IGNORE`).
    ///
    /// `created_at` is stamped with the current UTC time at insert time.
    pub fn insert(
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
            "INSERT OR IGNORE INTO memory
             (id, content, source, entry_type, session_id, created_at, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, content, source, entry_type, session_id, created_at, blob],
        )?;
        Ok(())
    }

    /// Insert or replace a memory entry by primary key.
    ///
    /// Unlike `insert()` (which uses `INSERT OR IGNORE`), `upsert()` uses
    /// `INSERT OR REPLACE`, so an existing row with the same `id` is overwritten.
    /// Used by Phase 21 `store_fact()` to update operator facts in place.
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

    /// Delete a memory entry by primary key.
    ///
    /// Returns `true` if a row was deleted, `false` if no row with that `id` existed.
    pub fn delete(&self, id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock().expect("VectorStore mutex poisoned");
        let rows_changed = conn.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
        Ok(rows_changed > 0)
    }

    /// Cosine search filtered to one `source` value.
    ///
    /// Same algorithm as `search()` but adds `WHERE source = ?1` to restrict results
    /// to a specific source (e.g., `MEMORY_SOURCE_CONVERSATION` or `MEMORY_SOURCE_OPERATOR`).
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

    /// Cosine search restricted to authoritative knowledge entries.
    ///
    /// Filters to `entry_type IN ('fact', 'web_page')` — entries that represent
    /// persistent, intentionally-stored knowledge. Conversation turns
    /// (`entry_type='turn'` or `'conversation_turn'`) are excluded to prevent
    /// Phase 21 turn embeddings from contaminating the retrieval pipeline's
    /// web-fetch decision.
    ///
    /// Entry type semantics:
    /// - `'fact'`              → operator-stated fact stored via `store_fact()` (source='operator')
    /// - `'web_page'`          → fetched and cached by `cache_web_result()` (source='web:{url}')
    /// - `'turn'`              → Phase 21 conversation embed — NOT authoritative for factual queries
    /// - `'conversation_turn'` → Phase 9 `store_conversation_turn()` — also excluded
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

    /// Semantic search: load all embeddings, compute cosine similarity in Rust,
    /// return the top `limit` entries by descending similarity score.
    ///
    /// Empty table → `Ok(vec![])`. Never errors on empty.
    ///
    /// Note: production retrieval now uses `search_knowledge()` (entry_type-filtered).
    /// `search()` is retained for store-level tests and future callers that need
    /// full-table scans (e.g., analytics, migration tooling).
    #[allow(dead_code)] // called from store unit tests; not currently on the hot path
    pub fn search(
        &self,
        query_embedding: &[f32],
        limit:           usize,
    ) -> Result<Vec<MemoryEntry>, rusqlite::Error> {
        let conn = self.conn.lock().expect("VectorStore mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, content, source, entry_type, session_id, created_at, embedding
             FROM memory",
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
                let sim = cosine_similarity(query_embedding, &row_emb);
                (
                    sim,
                    MemoryEntry { id, content, source, entry_type, session_id, created_at, similarity: sim },
                )
            })
            .collect();

        // Sort descending by similarity — highest first.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(_, entry)| entry).collect())
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Cosine similarity in \[−1, 1\]. Returns 0.0 if either vector is zero-norm,
/// preventing NaN from propagating into similarity scores.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), RETRIEVAL_EMBED_DIM, "query embedding must be RETRIEVAL_EMBED_DIM");
    let dot:  f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na:   f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb:   f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// Pack a `&[f32]` slice into a little-endian byte array (4 bytes per element).
///
/// Little-endian matches the native byte order on Apple Silicon and x86_64 — the
/// platforms Dexter runs on. If we ever need cross-platform round-trips, we have
/// explicit endianness here rather than implicit native encoding.
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Unpack a little-endian BLOB into a `Vec<f32>`.
///
/// Panics if `blob.len() % 4 != 0` — an unaligned BLOB indicates data corruption
/// and must not produce a silently wrong embedding.
fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    assert_eq!(blob.len() % 4, 0, "embedding BLOB length must be a multiple of 4 bytes");
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic unit embedding with 1.0 at `idx` and 0.0 elsewhere.
    /// Dimension is RETRIEVAL_EMBED_DIM so the store doesn't reject it.
    fn unit_vec(idx: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; RETRIEVAL_EMBED_DIM];
        v[idx] = 1.0;
        v
    }

    /// Build a vector that is `a` + `b` normalised to unit length.
    fn blend(a: usize, b: usize, wa: f32, wb: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; RETRIEVAL_EMBED_DIM];
        v[a] = wa;
        v[b] = wb;
        let norm = (wa * wa + wb * wb).sqrt();
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }

    #[test]
    fn store_new_creates_schema() {
        // In-memory DB applies the schema on construction.
        let store = VectorStore::in_memory().unwrap();
        // Can re-open (idempotent schema) — verified by calling insert without error.
        let emb = unit_vec(0);
        store.insert("id-1", "hello", "session:s1", "conversation_turn", Some("s1"), &emb)
            .expect("insert should succeed on a freshly created schema");
    }

    #[test]
    fn store_insert_and_search_finds_exact_match() {
        let store = VectorStore::in_memory().unwrap();
        let emb = unit_vec(5);
        store.insert("id-a", "content a", "session:s", "conversation_turn", Some("s"), &emb).unwrap();

        let results = store.search(&emb, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "id-a");
        assert!((results[0].similarity - 1.0).abs() < 1e-5,
            "self-search should return similarity ≈ 1.0, got {}", results[0].similarity);
    }

    #[test]
    fn store_search_empty_db_returns_empty() {
        let store = VectorStore::in_memory().unwrap();
        let results = store.search(&unit_vec(0), 10).unwrap();
        assert!(results.is_empty(), "search on empty DB must return empty Vec");
    }

    #[test]
    fn cosine_similarity_identical_returns_one() {
        let v = unit_vec(3);
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6, "identical vectors → similarity 1.0, got {sim}");
    }

    #[test]
    fn cosine_similarity_orthogonal_returns_zero() {
        let a = unit_vec(0);
        let b = unit_vec(1);
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6, "orthogonal vectors → similarity 0.0, got {sim}");
    }

    #[test]
    fn cosine_similarity_known_vectors() {
        // [1,0,...] vs [0.6,0.8,...] → cosine = 0.6
        // Build these as RETRIEVAL_EMBED_DIM vectors with only idx 0 and 1 nonzero.
        let mut a = vec![0.0f32; RETRIEVAL_EMBED_DIM];
        a[0] = 1.0;
        let mut b = vec![0.0f32; RETRIEVAL_EMBED_DIM];
        b[0] = 0.6;
        b[1] = 0.8;
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 0.6).abs() < 1e-5, "expected 0.6, got {sim}");
    }

    #[test]
    fn vector_store_upsert_replaces_existing_content_by_id() {
        let store = VectorStore::in_memory().unwrap();
        let emb = vec![0.1f32; RETRIEVAL_EMBED_DIM];
        store.insert("k1", "original", "test", "fact", None, &emb).unwrap();
        store.upsert("k1", "updated", "test", "fact", None, &emb).unwrap();
        let results = store.search(&emb, 10).unwrap();
        assert_eq!(results.len(), 1, "upsert must not create a duplicate");
        assert_eq!(results[0].content, "updated", "upsert must replace content");
    }

    #[test]
    fn vector_store_delete_returns_true_for_existing_id() {
        let store = VectorStore::in_memory().unwrap();
        let emb = vec![0.1f32; RETRIEVAL_EMBED_DIM];
        store.insert("k1", "content", "test", "fact", None, &emb).unwrap();
        assert!(store.delete("k1").unwrap());
        assert!(store.search(&emb, 10).unwrap().is_empty());
    }

    #[test]
    fn vector_store_delete_returns_false_for_missing_id() {
        let store = VectorStore::in_memory().unwrap();
        assert!(!store.delete("nonexistent").unwrap());
    }

    #[test]
    fn vector_store_search_source_filters_by_source_field() {
        let store = VectorStore::in_memory().unwrap();
        let emb = vec![0.5f32; RETRIEVAL_EMBED_DIM];
        store.insert("m1", "memory content", "memory", "turn", None, &emb).unwrap();
        store.insert("r1", "retrieval content", "retrieval", "document", None, &emb).unwrap();

        let memory_results = store.search_source(&emb, 10, "memory").unwrap();
        assert_eq!(memory_results.len(), 1);
        assert_eq!(memory_results[0].source, "memory");

        let retrieval_results = store.search_source(&emb, 10, "retrieval").unwrap();
        assert_eq!(retrieval_results.len(), 1);
        assert_eq!(retrieval_results[0].source, "retrieval");
    }

    #[test]
    fn store_search_returns_most_similar_of_three() {
        let store = VectorStore::in_memory().unwrap();

        // A is close to the query (unit_vec(0)).
        // B blends dimensions 0 and 1 — somewhat similar.
        // C is orthogonal to the query — zero similarity.
        let query = unit_vec(0);
        let a_emb = blend(0, 1, 0.99, 0.14);   // mostly dimension 0 → high similarity
        let b_emb = blend(0, 1, 0.5, 0.87);    // ~30° tilt → lower
        let c_emb = unit_vec(2);                 // orthogonal → 0.0

        store.insert("id-a", "A", "session:s", "conversation_turn", Some("s"), &a_emb).unwrap();
        store.insert("id-b", "B", "session:s", "conversation_turn", Some("s"), &b_emb).unwrap();
        store.insert("id-c", "C", "session:s", "conversation_turn", Some("s"), &c_emb).unwrap();

        let results = store.search(&query, 3).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, "id-a", "A must rank first (most similar to query)");
        assert!(results[0].similarity > results[1].similarity,
            "ranked order must be strictly descending");
        assert!(results[1].similarity >= results[2].similarity,
            "ranked order must be descending (B ≥ C)");
    }

    #[test]
    fn search_knowledge_returns_facts_and_web_pages_only() {
        let store = VectorStore::in_memory().unwrap();
        let emb   = vec![0.5f32; RETRIEVAL_EMBED_DIM];

        // Insert one entry of each type.
        store.insert("f1",  "a fact",     "operator",              "fact",             None, &emb).unwrap();
        store.insert("w1",  "a web page", "web:https://example.com","web_page",         None, &emb).unwrap();
        store.insert("t1",  "a turn",     "memory",                "turn",             None, &emb).unwrap();
        store.insert("ct1", "conv turn",  "session:s1",            "conversation_turn",None, &emb).unwrap();

        let results = store.search_knowledge(&emb, 10).unwrap();
        assert_eq!(results.len(), 2,
            "search_knowledge must return exactly the fact + web_page entries; got {:?}",
            results.iter().map(|e| e.entry_type.as_str()).collect::<Vec<_>>());
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
}
