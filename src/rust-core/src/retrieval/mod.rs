/// Retrieval Pipeline — Phase 9.
///
/// Semantic memory (VectorStore) and web content grounding (WebRetriever).
/// `RetrievalPipeline` orchestrates trigger detection, retrieval, and injection.
///
/// ## Module structure
///
/// - `store`    — `VectorStore`: SQLite + in-Rust cosine similarity over f32 BLOBs
/// - `web`      — `WebRetriever`: HTTP fetch + HTML text extraction via `scraper`
/// - `pipeline` — `RetrievalPipeline`: trigger detection, retrieval, context injection

pub mod pipeline;
pub mod store;
pub mod web;

#[allow(unused_imports)] // Phase 10+ callers via crate::retrieval::{RetrievalContext, RetrievalTrigger, RetrievalResult}
pub use pipeline::{RetrievalContext, RetrievalPipeline, RetrievalResult, RetrievalTrigger};
#[allow(unused_imports)] // Phase 10+ callers via crate::retrieval::MemoryEntry
pub use store::{MemoryEntry, VectorStore};
#[allow(unused_imports)] // Phase 10+ callers via crate::retrieval::FetchResult
pub use web::{FetchResult, WebRetriever};
