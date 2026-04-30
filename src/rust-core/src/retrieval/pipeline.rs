/// RetrievalPipeline — Phase 9 orchestrator for semantic memory and web grounding.
///
/// Plugs into `handle_text_input()` at two points:
///
/// **Pre-generation** (`detect_pre_trigger`): when the router classifies the query as
/// `Category::RetrievalFirst`, retrieve memory + web content before generating so the
/// response is grounded in retrieved facts, not training data.
///
/// **Post-generation** (`detect_post_trigger`): if the model expresses uncertainty via
/// `UNCERTAINTY_MARKER`, retrieve context and generate a grounded follow-up response.
///
/// ## DuckDuckGo instant-answer API
///
/// `https://api.duckduckgo.com/?q={query}&format=json&no_html=1&skip_disambig=1`
///
/// No API key required. Returns instant answers for common factual queries. Failures
/// are non-fatal — the pipeline falls back to memory-only context.
use std::path::Path;

use tracing::{info, warn};
use uuid::Uuid;

use crate::constants::{
    LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD,
    MEMORY_DB_FILENAME, MEMORY_EMBED_MAX_CHARS, MEMORY_RECALL_THRESHOLD, MEMORY_RECALL_TOP_N,
    MEMORY_SOURCE_CONVERSATION, MEMORY_SOURCE_OPERATOR,
    RETRIEVAL_EMBED_DIM, RETRIEVAL_MAX_MEMORY_HITS, RETRIEVAL_WEB_TIMEOUT_SECS,
    RETRIEVAL_WTTR_TIMEOUT_SECS, UNCERTAINTY_MARKER,
};
use crate::inference::engine::{EmbeddingRequest, InferenceEngine};

use super::store::{MemoryEntry, VectorStore};
use super::web::{FetchResult, WebRetriever};

// ── Public types ──────────────────────────────────────────────────────────────

/// Simplified retrieval result returned by `RetrievalPipeline::retrieve_web_only`.
///
/// Designed for the Phase 19 sentinel-interception and retrieval-first paths where
/// the orchestrator needs the retrieved text without embedding it into VectorStore.
/// The full `RetrievalContext` (with `VectorStore` hits and `FetchResult`) is used
/// by the Phase 9 embed+store path; `RetrievalResult` is used by the Phase 19 path.
#[derive(Debug, Clone)]
pub struct RetrievalResult {
    /// The query string that was retrieved.
    pub query:      String,
    /// The primary text extracted from the retrieval source.
    pub text:       String,
    /// URL or source identifier for provenance logging.
    pub source:     String,
    /// Confidence score 0.0–1.0. 0.9 = AbstractText hit; 0.5 = raw body fallback.
    pub confidence: f32,
}

/// What caused retrieval to trigger — governs which sources to query.
#[derive(Debug, Clone)]
pub enum RetrievalTrigger {
    /// Router classified user query as `Category::RetrievalFirst`.
    /// Search memory for context before generating the primary response.
    MemorySearch { query: String },
    /// Post-generation: model expressed uncertainty via `UNCERTAINTY_MARKER`.
    /// Retrieve context then generate a grounded follow-up response.
    UncertaintyMarker { topic: String },
}

impl RetrievalTrigger {
    /// Return the query string used to embed and search.
    pub fn query(&self) -> &str {
        match self {
            Self::MemorySearch { query }      => query,
            Self::UncertaintyMarker { topic } => topic,
        }
    }
}

/// Retrieved context ready for injection into the generation request.
#[allow(dead_code)] // Phase 10+ callers read query and trigger for audit/logging
pub struct RetrievalContext {
    pub query:       String,
    pub memory_hits: Vec<MemoryEntry>,
    /// `Some` if a web fetch was attempted and succeeded. `None` on failure or
    /// when the query was satisfied by memory hits alone.
    pub web_result:  Option<FetchResult>,
    pub trigger:     RetrievalTrigger,
}

// ── RetrievalPipeline ─────────────────────────────────────────────────────────

pub struct RetrievalPipeline {
    store: VectorStore,
    web:   WebRetriever,
}

impl RetrievalPipeline {
    /// Open VectorStore at `{state_dir}/memory.db`.
    ///
    /// Returns `Err` if SQLite cannot open the file (e.g., permissions). Callers
    /// should fall back to `new_degraded()` on error so the daemon still starts.
    pub fn new(state_dir: &Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let db_path = state_dir.join(MEMORY_DB_FILENAME);
        let store = VectorStore::new(&db_path)?;
        info!(path = %db_path.display(), "VectorStore opened");
        Ok(Self { store, web: WebRetriever::default_timeout() })
    }

    /// In-memory fallback — data not persisted across restarts.
    ///
    /// Used by `make_orchestrator()` in tests and when `new()` fails at startup.
    /// Opens an in-memory SQLite DB that is always available and always succeeds.
    pub fn new_degraded() -> Self {
        let store = VectorStore::in_memory()
            .expect("in-memory SQLite must always succeed");
        Self { store, web: WebRetriever::new(RETRIEVAL_WEB_TIMEOUT_SECS) }
    }

    // ── Trigger detection (pure — no Ollama, no network) ─────────────────────

    /// Pre-generation trigger check.
    ///
    /// Returns `Some(MemorySearch)` when `is_retrieval_first` is true (the caller
    /// passes `matches!(decision.category, Category::RetrievalFirst)`). Returns
    /// `None` for all other categories.
    ///
    /// Pure function — no IO.
    pub fn detect_pre_trigger(
        &self,
        user_message:       &str,
        is_retrieval_first: bool,
    ) -> Option<RetrievalTrigger> {
        if is_retrieval_first {
            Some(RetrievalTrigger::MemorySearch { query: user_message.to_string() })
        } else {
            None
        }
    }

    /// Post-generation trigger check.
    ///
    /// Scans `response_text` for `UNCERTAINTY_MARKER`. If found, extracts the topic
    /// as the text between the marker and the next `.` or end-of-line, trimmed.
    ///
    /// Returns `Some(UncertaintyMarker { topic })` or `None`.
    /// Pure function — no IO.
    pub fn detect_post_trigger(&self, response_text: &str) -> Option<RetrievalTrigger> {
        let marker_pos = response_text.find(UNCERTAINTY_MARKER)?;
        let after = &response_text[marker_pos + UNCERTAINTY_MARKER.len()..];
        // Extract topic: text up to the next '.' or end-of-line, trimmed.
        let topic = after
            .split_once('.')
            .map(|(before, _)| before)
            .unwrap_or_else(|| after.lines().next().unwrap_or(""))
            .trim()
            .to_string();
        Some(RetrievalTrigger::UncertaintyMarker { topic })
    }

    // ── Retrieval ─────────────────────────────────────────────────────────────

    /// Execute retrieval for `trigger`:
    ///
    /// 1. Embed `trigger.query()` via `engine.embed()`
    /// 2. Search knowledge base (facts + cached web pages — conversation turns excluded)
    /// 3. Local-first check: if any hit ≥ LOCAL_RETRIEVAL_SKIP_WEB_THRESHOLD, skip web
    /// 4. Else fetch DuckDuckGo if UncertaintyMarker or no local knowledge
    /// 5. Return `RetrievalContext`
    ///
    /// `embed_model_name` = `model_config.embed` (e.g. `"mxbai-embed-large"`).
    pub async fn retrieve(
        &mut self,
        engine:           &InferenceEngine,
        embed_model_name: &str,
        trigger:          &RetrievalTrigger,
    ) -> Result<RetrievalContext, Box<dyn std::error::Error + Send + Sync>> {
        let query = trigger.query().to_string();

        // ── Step 1: embed the query ───────────────────────────────────────────
        // mxbai-embed-large performs better with this prefix on search queries.
        let prefixed = format!(
            "Represent this sentence for searching relevant passages: {}",
            query
        );
        let embedding = engine
            .embed(EmbeddingRequest { model_name: embed_model_name.to_string(), input: prefixed })
            .await?;

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

        // Phase 37.8 — weather fast-path with cache bypass.
        //
        // DDG's instant-answer API has no live-weather coverage, so weather queries
        // routed to RetrievalFirst used to dead-end with "I don't have live weather
        // data" (Test 3). wttr.in returns a one-line plain-text current-conditions
        // payload (~300 ms) which is enough to ground the model's answer.
        //
        // Phase 37.8.1 — weather BYPASSES the local-first cache gate.
        //
        // First iteration of this fix nested the wttr call inside `if should_fetch_web`,
        // but `should_fetch_web` is false whenever knowledge_hits is non-empty. A single
        // stale weather row from a prior test (or any cached row that vector-matched
        // "weather in Tokyo") suppressed the fast-path entirely, leaving the model with
        // a stale [Retrieved: ...] block and zero current conditions. Weather is
        // inherently time-sensitive — current conditions from 6 hours ago are functionally
        // wrong — so weather queries must bypass the cache decision and ALSO discard
        // any local hits that would otherwise be injected as authoritative context.
        //
        // For non-weather queries the prior gate is preserved (cache-first is the right
        // policy when facts are stable).
        let is_weather = is_weather_query(&query);

        let (web_result, knowledge_hits) = if is_weather {
            // Branch on how many locations the operator named:
            //   0 → wttr IP-geolocation default ("what's the weather")
            //   1 → existing single-city path (retry-on-5xx inside fetch_wttr)
            //   2+ → multi-city parallel fetch — fixes "weather in Tokyo and
            //        Sacramento" which previously answered only Tokyo.
            let locations = extract_weather_locations(&query);
            let wttr = match locations.len() {
                0 => self.fetch_wttr(None).await,
                1 => self.fetch_wttr(Some(&locations[0])).await,
                _ => self.fetch_wttr_multi(&locations).await,
            };
            // Always discard cached knowledge for weather — even on wttr failure, a
            // stale row is worse than no row because it'll be presented as authoritative
            // [Retrieved: ...] context.
            let weather_result = match wttr {
                Some(r) => Some(r),
                None    => self.fetch_ddg(&query, embed_model_name, engine).await,
            };
            (weather_result, Vec::new())
        } else {
            let web = if should_fetch_web {
                self.fetch_ddg(&query, embed_model_name, engine).await
            } else {
                None
            };
            (web, knowledge_hits)
        };

        // field name `memory_hits` preserved for API compatibility; now contains only
        // authoritative knowledge entries (facts + cached web pages)
        Ok(RetrievalContext { query, memory_hits: knowledge_hits, web_result, trigger: trigger.clone() })
    }

    /// Web-only retrieval for the Phase 19 sentinel-interception path.
    ///
    /// Unlike `retrieve()`, this method:
    /// - Takes only a query string (no `InferenceEngine` or `RetrievalTrigger`)
    /// - Does NOT embed the result or cache it in VectorStore
    /// - Takes `&self` (not `&mut self`) — safe to call without exclusive access
    ///
    /// The tradeoff: no local semantic memory hits and no caching. This is acceptable
    /// for Phase 19 because the sentinel path is triggered by genuine model uncertainty,
    /// which typically means the answer is NOT in training data or local memory.
    ///
    /// Results are not cached in this call. A future phase may add async background
    /// caching without blocking the response.
    pub async fn retrieve_web_only(
        &self,
        query: &str,
    ) -> Result<RetrievalResult, Box<dyn std::error::Error + Send + Sync>> {
        let encoded  = urlencoding_encode(query);
        let api_url  = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
            encoded
        );

        let fetch_result = self.web.fetch(&api_url).await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        // Parse DDG JSON to extract AbstractText (the structured instant answer).
        // Prefer AbstractText over raw HTML — it's the concise factual answer.
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&fetch_result.text) {
            let abstract_text = json.get("AbstractText")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());

            if let Some(text) = abstract_text {
                let source = json.get("AbstractURL")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("https://duckduckgo.com")
                    .to_string();
                info!(query = %query, source = %source, "retrieve_web_only: AbstractText hit");
                return Ok(RetrievalResult {
                    query:      query.to_string(),
                    text:       text.to_string(),
                    source,
                    confidence: 0.9,
                });
            }
        }

        // Fall back to raw extracted body text from the DuckDuckGo response.
        // Confidence 0.5 — raw DDG response body is less reliable than AbstractText.
        info!(query = %query, source = %fetch_result.url, "retrieve_web_only: raw body fallback");
        Ok(RetrievalResult {
            query:      query.to_string(),
            text:       fetch_result.text,
            source:     fetch_result.url,
            confidence: 0.5,
        })
    }

    /// Format a `RetrievalContext` as a string for injection as a system-role message.
    ///
    /// Format per result: `[Retrieved: {source}]\n{content}\n\n`
    ///
    /// Returns `""` if there are no memory hits and no web result. The caller skips
    /// injection when the empty string is returned.
    pub fn format_for_injection(&self, ctx: &RetrievalContext) -> String {
        let mut out = String::new();
        for entry in &ctx.memory_hits {
            out.push_str(&format!("[Retrieved: {}]\n{}\n\n", entry.source, entry.content));
        }
        if let Some(web) = &ctx.web_result {
            let source = web.title.as_deref().unwrap_or(web.url.as_str());
            out.push_str(&format!("[Retrieved: {}]\n{}\n\n", source, web.text));
        }
        out
    }

    /// Embed `content` and store it in VectorStore as a `conversation_turn` entry.
    ///
    /// `source = "session:{session_id}"`, `entry_type = "conversation_turn"`.
    /// Non-fatal: the caller logs `Err` and continues — a storage failure must not
    /// prevent response delivery.
    pub async fn store_conversation_turn(
        &mut self,
        engine:           &InferenceEngine,
        embed_model_name: &str,
        content:          &str,
        session_id:       &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let embedding = engine
            .embed(EmbeddingRequest {
                model_name: embed_model_name.to_string(),
                input:      content.to_string(),
            })
            .await?;
        let id     = Uuid::new_v4().to_string();
        let source = format!("session:{}", session_id);
        self.store.insert(&id, content, &source, "conversation_turn", Some(session_id), &embedding)?;
        Ok(())
    }

    // ── Phase 21: Memory recall and fact management ───────────────────────────

    /// Embed `query` and retrieve the most relevant memory entries from both
    /// the conversation-turn store and the operator-fact store.
    ///
    /// Entries below `MEMORY_RECALL_THRESHOLD` are discarded. The combined
    /// result is re-sorted by similarity and truncated to `MEMORY_RECALL_TOP_N`.
    /// Returns an empty Vec on embedding failure (non-fatal, logged at warn).
    pub async fn recall_relevant(
        &self,
        engine:      &InferenceEngine,
        embed_model: &str,
        query:       &str,
    ) -> Vec<crate::retrieval::store::MemoryEntry> {
        let embedding = match engine.embed(crate::inference::engine::EmbeddingRequest {
            model_name: embed_model.to_string(),
            input: truncate_for_embed(query),
        }).await {
            Ok(e)  => e,
            Err(e) => {
                warn!(error = %e, "Memory recall: embed failed — skipping recall injection");
                return vec![];
            }
        };

        let mut results: Vec<crate::retrieval::store::MemoryEntry> = Vec::new();
        if let Ok(turns) = self.store.search_source(&embedding, MEMORY_RECALL_TOP_N, MEMORY_SOURCE_CONVERSATION) {
            results.extend(turns);
        }
        if let Ok(facts) = self.store.search_source(&embedding, MEMORY_RECALL_TOP_N, MEMORY_SOURCE_OPERATOR) {
            results.extend(facts);
        }

        results.retain(|e| e.similarity >= MEMORY_RECALL_THRESHOLD);
        results.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(MEMORY_RECALL_TOP_N);
        results
    }

    /// Embed `content` and store it as a conversation turn in the memory source.
    ///
    /// `id` is `trace_id` — deterministic per turn so re-runs of the same turn
    /// don't create duplicates (uses `INSERT OR IGNORE` via `store.insert()`).
    /// Non-fatal: embed or insert failures are logged at warn and ignored.
    pub async fn embed_and_store_turn(
        &self,
        engine:      &InferenceEngine,
        embed_model: &str,
        session_id:  &str,
        trace_id:    &str,
        content:     &str,
    ) {
        // NOTE: embed the truncated prefix but persist the full `content`.
        // mxbai-embed-large has a 512-token trained context; submitting more
        // triggers an Ollama 400 and silently drops the turn from memory. The
        // embedding only needs to capture "what is this turn about" for cosine
        // retrieval — head-truncation preserves the user question (highest
        // retrieval signal) while dropping the tail of long assistant answers.
        let embedding = match engine.embed(crate::inference::engine::EmbeddingRequest {
            model_name: embed_model.to_string(),
            input: truncate_for_embed(content),
        }).await {
            Ok(e)  => e,
            Err(e) => {
                warn!(error = %e, "Memory: embed failed — turn not stored");
                return;
            }
        };

        if let Err(e) = self.store.insert(
            trace_id, content, MEMORY_SOURCE_CONVERSATION, "turn", Some(session_id), &embedding,
        ) {
            warn!(error = %e, "Memory: VectorStore insert failed — turn not stored");
        }
    }

    /// Embed `content` and upsert it as an operator fact (keyed by `slug`).
    ///
    /// Uses `INSERT OR REPLACE` so that re-stating a fact with the same `slug`
    /// updates the stored content and embedding in place.
    /// Non-fatal: embed or upsert failures are logged at warn and ignored.
    pub async fn store_fact(
        &self,
        engine:      &InferenceEngine,
        embed_model: &str,
        slug:        &str,
        content:     &str,
    ) {
        // Same embed-context protection as embed_and_store_turn. Facts are
        // typically short, but an operator pasting a long "remember this"
        // blob would otherwise hit the same 400-rejection silent-drop path.
        let embedding = match engine.embed(crate::inference::engine::EmbeddingRequest {
            model_name: embed_model.to_string(),
            input: truncate_for_embed(content),
        }).await {
            Ok(e)  => e,
            Err(e) => {
                warn!(error = %e, "Memory: embed failed — fact not stored");
                return;
            }
        };

        if let Err(e) = self.store.upsert(
            slug, content, MEMORY_SOURCE_OPERATOR, "fact", None, &embedding,
        ) {
            warn!(error = %e, "Memory: VectorStore upsert failed — fact not stored");
        }
    }

    /// Delete an operator fact by its slug identifier.
    ///
    /// Returns `true` if a fact with `slug` existed and was deleted, `false` if not found.
    pub fn delete_fact(&self, slug: &str) -> bool {
        self.store.delete(slug).unwrap_or(false)
    }

    /// Return all operator-fact entries from the VectorStore.
    ///
    /// Uses a zero-vector query with a large limit — the zero vector produces cosine
    /// similarity 0.0 for all entries, which is equivalent to an unordered scan.
    /// Results are not sorted by relevance (caller formats them as a numbered list).
    pub fn list_facts(&self) -> Vec<crate::retrieval::store::MemoryEntry> {
        let zero = vec![0.0f32; RETRIEVAL_EMBED_DIM];
        self.store.search_source(&zero, 1000, MEMORY_SOURCE_OPERATOR).unwrap_or_default()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Fetch a DuckDuckGo instant-answer API result for `query`.
    ///
    /// Strategy:
    /// 1. Call `https://api.duckduckgo.com/?q={query}&format=json&no_html=1&skip_disambig=1`
    /// 2. Use `AbstractText` if non-empty
    /// 3. Else try `RelatedTopics[0].FirstURL` and fetch that page
    /// 4. On any failure: log at `warn!` level and return `None`
    ///
    /// Successful results are also stored in VectorStore to avoid duplicate fetches
    /// in future sessions.
    /// Fetch current conditions from wttr.in.
    ///
    /// Phase 37.8 weather fast-path. URL form:
    ///
    ///   - With location: `https://wttr.in/{location}?format=3`
    ///   - Without:       `https://wttr.in/?format=3`
    ///
    /// `?format=3` returns a single line: `"{location}: {emoji} +{temp}°{unit}"`.
    /// wttr selects the unit by geographic IP, so the operator gets °F in the US
    /// and °C elsewhere without us threading a units toggle through the pipeline.
    /// When no location is supplied wttr uses the request IP, which on a home
    /// network resolves to the operator's city — exactly what they meant by
    /// "what's the weather?" with no location named.
    ///
    /// **Retry policy** (added post-37.8 after wttr.in returned 500 on a live
    /// query and the pipeline dead-ended with "I don't have live weather"):
    /// one retry with 200 ms backoff on server-side (5xx) or connection
    /// errors. Client errors (4xx) are never retried — they mean the location
    /// is wrong, not that wttr is struggling, and retry won't help.
    ///
    /// Returns `None` on any failure (network, non-2xx, malformed body). The
    /// caller falls back to `fetch_ddg`.
    async fn fetch_wttr(&self, location: Option<&str>) -> Option<FetchResult> {
        let url = match location {
            Some(loc) => format!("https://wttr.in/{}?format=3", urlencoding_encode(loc)),
            None      => "https://wttr.in/?format=3".to_string(),
        };
        self.fetch_wttr_url(&url).await
    }

    /// Inner helper: one-URL wttr fetch with retry-on-5xx + dead-response filter.
    ///
    /// Broken out so `fetch_wttr_multi` can parallelize fetches via
    /// `futures_util::future::join_all` without rebuilding the URL.
    async fn fetch_wttr_url(&self, url: &str) -> Option<FetchResult> {
        const MAX_ATTEMPTS: u32 = 2;

        let mut last_err: Option<String> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.web.fetch_plain(url, RETRIEVAL_WTTR_TIMEOUT_SECS).await {
                Ok(r) => {
                    // wttr.in returns plain-text error pages for unknown locations:
                    // `"Unknown location; please try [...]"`. Detect and fall back —
                    // retry won't fix a wrong name.
                    let body = r.text.trim();
                    if body.is_empty()
                        || body.to_lowercase().starts_with("unknown location")
                        || body.contains("Sorry, we are running out of queries")
                    {
                        warn!(
                            body_preview = %body.chars().take(80).collect::<String>(),
                            url = %url,
                            "wttr.in returned non-weather body — falling back to DDG"
                        );
                        return None;
                    }
                    if attempt > 1 {
                        info!(url = %url, attempt = attempt, "wttr.in retry succeeded");
                    } else {
                        info!(url = %url, body = %body, "wttr.in fast-path hit");
                    }
                    return Some(FetchResult {
                        url:        r.url,
                        // Title shows up in `format_for_injection` as `[Retrieved: <title>]`,
                        // so name it something the model can quote naturally.
                        title:      Some("wttr.in current conditions".to_string()),
                        text:       body.to_string(),
                        fetched_at: r.fetched_at,
                    });
                }
                Err(e) => {
                    // Retry only on server errors / connection issues. A 404 on
                    // `/Totallyfakeplace` is terminal — burning another ~300 ms on
                    // it is just added latency in the failure path.
                    let is_client_err = e.status().map(|s| s.is_client_error()).unwrap_or(false);
                    last_err = Some(e.to_string());
                    if is_client_err || attempt == MAX_ATTEMPTS {
                        break;
                    }
                    // 200 ms is a deliberate middle ground: long enough for a
                    // transient wttr blip to clear, short enough not to stretch
                    // the retrieval budget past RETRIEVAL_WTTR_TIMEOUT_SECS.
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }

        warn!(
            error = %last_err.unwrap_or_else(|| "unknown".to_string()),
            url = %url,
            "wttr.in fetch failed after retries — falling back to DDG"
        );
        None
    }

    /// Fetch current conditions for multiple named locations in parallel.
    ///
    /// Compound queries ("weather in Tokyo and Sacramento") get one wttr call
    /// per named city, dispatched concurrently via `join_all`. Each city's
    /// result uses the same retry-on-5xx policy as the single-city path.
    ///
    /// Aggregation:
    ///   - At least one success → synthesized multi-line body, newline-joined
    ///   - All fail → `None` (caller falls back to DDG exactly as before)
    ///
    /// Wall-clock latency stays near the slowest single fetch plus one retry,
    /// not `N ×` the single-city cost. Two cities on a healthy network land
    /// inside the existing 4 s `RETRIEVAL_WTTR_TIMEOUT_SECS` envelope.
    async fn fetch_wttr_multi(&self, locations: &[String]) -> Option<FetchResult> {
        use futures_util::future::join_all;

        if locations.is_empty() {
            return None;
        }

        let urls: Vec<String> = locations.iter()
            .map(|loc| format!("https://wttr.in/{}?format=3", urlencoding_encode(loc)))
            .collect();

        let futs = urls.iter().map(|u| self.fetch_wttr_url(u));
        let results = join_all(futs).await;

        // Preserve requested order and pair each body with its requested city
        // name, so the model can quote "Tokyo: ..." / "Sacramento: ..." even
        // when wttr's own line doesn't echo the input exactly.
        let mut bodies: Vec<String> = Vec::new();
        let mut successes = 0usize;
        let mut source_urls: Vec<String> = Vec::new();
        let mut latest_fetched_at: Option<String> = None;

        for (loc, res) in locations.iter().zip(results.into_iter()) {
            match res {
                Some(r) => {
                    bodies.push(r.text.clone());
                    source_urls.push(r.url.clone());
                    latest_fetched_at = Some(r.fetched_at.clone());
                    successes += 1;
                }
                None => {
                    // Per-city miss: keep the answer informative rather than
                    // silently dropping the city. The model needs to know which
                    // piece of the compound question wttr couldn't answer.
                    bodies.push(format!("{}: weather unavailable", loc));
                }
            }
        }

        if successes == 0 {
            warn!(
                locations = ?locations,
                "wttr.in multi-city fetch: all cities failed — falling back to DDG"
            );
            return None;
        }

        info!(
            locations = ?locations,
            successes = successes,
            total = locations.len(),
            "wttr.in multi-city fast-path hit"
        );
        Some(FetchResult {
            // Use the first successful URL as the provenance marker; the body
            // below contains all lines, so this is just for logging.
            url:        source_urls.into_iter().next()
                          .unwrap_or_else(|| "https://wttr.in/".to_string()),
            title:      Some("wttr.in current conditions".to_string()),
            text:       bodies.join("\n"),
            fetched_at: latest_fetched_at
                          .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        })
    }

    async fn fetch_ddg(
        &mut self,
        query:            &str,
        embed_model_name: &str,
        engine:           &InferenceEngine,
    ) -> Option<FetchResult> {
        let encoded = urlencoding_encode(query);
        let api_url = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
            encoded
        );

        let fetch_result = match self.web.fetch(&api_url).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, query = query, "DuckDuckGo API fetch failed");
                return None;
            }
        };

        // Parse the JSON response body for instant-answer fields.
        // The `text` field from WebRetriever contains the extracted text; for JSON
        // APIs we need the raw body. Re-fetch or use the raw text as-is.
        // Since WebRetriever calls extract_text() which handles JSON as plain text,
        // the `text` field contains whatever text was in the body. For the DDG JSON
        // API response, this works acceptably — the JSON is compact and readable.
        // Try to parse the raw body as JSON to extract AbstractText specifically.
        let result = self.try_parse_ddg_json(&fetch_result.text, query, embed_model_name, engine).await;
        result.or(Some(fetch_result))
    }

    /// Try to extract the `AbstractText` or `RelatedTopics[0].FirstURL` from a
    /// DuckDuckGo JSON response body. Returns the parsed FetchResult if successful,
    /// or `None` if the response doesn't contain usable instant-answer content.
    async fn try_parse_ddg_json(
        &mut self,
        body:             &str,
        query:            &str,
        embed_model_name: &str,
        engine:           &InferenceEngine,
    ) -> Option<FetchResult> {
        let json: serde_json::Value = serde_json::from_str(body).ok()?;

        // Prefer AbstractText — it's the concise answer for most factual queries.
        let abstract_text = json.get("AbstractText")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        if let Some(text) = abstract_text {
            let result = FetchResult {
                url:        json.get("AbstractURL")
                    .and_then(|v| v.as_str())
                    .unwrap_or("https://duckduckgo.com")
                    .to_string(),
                title:      Some(format!("DuckDuckGo: {}", query)),
                text:       text.to_string(),
                fetched_at: chrono::Utc::now().to_rfc3339(),
            };
            self.cache_web_result(&result, embed_model_name, engine).await;
            return Some(result);
        }

        // Fallback: fetch the first RelatedTopics URL if AbstractText was empty.
        let first_url = json
            .get("RelatedTopics")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("FirstURL"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())?;

        match self.web.fetch(first_url).await {
            Ok(result) => {
                self.cache_web_result(&result, embed_model_name, engine).await;
                Some(result)
            }
            Err(e) => {
                warn!(error = %e, url = first_url, "DuckDuckGo RelatedTopics fetch failed");
                None
            }
        }
    }

    /// Store a web fetch result in VectorStore to avoid re-fetching in future sessions.
    /// Non-fatal — logs and ignores errors.
    async fn cache_web_result(
        &mut self,
        result:           &FetchResult,
        embed_model_name: &str,
        engine:           &InferenceEngine,
    ) {
        let embed_res = engine
            .embed(EmbeddingRequest {
                model_name: embed_model_name.to_string(),
                input:      result.text.clone(),
            })
            .await;
        match embed_res {
            Ok(embedding) => {
                let id     = Uuid::new_v4().to_string();
                let source = format!("web:{}", result.url);
                if let Err(e) = self.store.insert(&id, &result.text, &source, "web_page", None, &embedding) {
                    warn!(error = %e, "Failed to cache web result in VectorStore — non-fatal");
                }
            }
            Err(e) => warn!(error = %e, "Failed to embed web result for caching — non-fatal"),
        }
    }
}

// ── URL encoding helper ───────────────────────────────────────────────────────

/// Percent-encode a query string for use in a URL parameter.
///
/// Encodes all characters except unreserved ones (A-Z, a-z, 0-9, `-`, `_`, `.`, `~`).
/// Does not pull in the `percent-encoding` crate — the DuckDuckGo query is the only
/// use case and a hand-rolled encoder keeps the dependency tree small.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            b' ' => out.push('+'),
            b    => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ── Weather query detection (Phase 37.8) ──────────────────────────────────────

/// Detect a weather/conditions query that should bypass DDG and hit wttr.in.
///
/// Pattern coverage chosen empirically from real operator phrasings:
///   - Direct nouns: "weather", "forecast", "temperature", "humidity"
///   - Sky/precipitation: "raining", "snowing", "cloudy", "sunny"
///   - Sensory: "how hot", "how cold", "how warm", "feel like outside"
///   - Time-cued: "weather today", "weather tomorrow"
///
/// All matching is lowercase substring; the input is lowercased once. False
/// positives are tolerable (a non-weather query that wttr can't satisfy will
/// fall back to DDG with one wasted ~300ms request); false negatives leave the
/// operator with the prior dead-end behavior, so prefer over-matching.
pub(crate) fn is_weather_query(query: &str) -> bool {
    let q = query.to_lowercase();
    const PATTERNS: &[&str] = &[
        "weather", "forecast", "temperature", "humidity",
        "raining", "snowing", "cloudy", "sunny",
        "how hot", "how cold", "how warm", "feels like outside",
        "is it raining", "is it snowing",
    ];
    PATTERNS.iter().any(|p| q.contains(p))
}

/// Extract a location phrase from a weather query, or `None` for IP-based default.
///
/// Thin back-compat wrapper around `extract_weather_locations`. Returns the
/// first location found, or `None` if the query names no explicit locations.
/// New callers should prefer the plural variant — multi-city queries
/// ("weather in Tokyo and Sacramento") silently lose cities through this one.
///
/// Retained across the Phase 37.9 refactor so the existing single-location
/// extractor tests keep their semantics pinned; also documents the "take
/// first" contract explicitly rather than leaving it implicit in callers.
#[allow(dead_code)]
pub(crate) fn extract_weather_location(query: &str) -> Option<String> {
    extract_weather_locations(query).into_iter().next()
}

/// Extract every location phrase named in a weather query, in query order.
///
/// Multi-city handling is load-bearing: operators ask compound questions
/// ("what's the weather in Tokyo and Sacramento") that the single-location
/// extractor silently truncated to the first match, leaving the second city
/// unanswered. The wttr fast-path used to answer half the question while
/// claiming the rest was unknown.
///
/// Recognition rules (all lowercase, on the already-lowered input):
///   - Every occurrence of `" in "`, `" at "`, `" for "` anchors a location phrase
///   - A phrase ends at the next clause boundary (`?.,!\n`) or end-of-string
///   - Within a phrase, conjunctions (`" and "`, `" or "`) split multiple cities:
///     "in Tokyo and Sacramento" → ["tokyo", "sacramento"]
///   - Trailing time qualifiers ("today", "tomorrow", "right now"…) are stripped
///     from each extracted phrase
///   - Duplicates are dropped preserving first-occurrence order, so
///     "weather in Tokyo. What about weather in Tokyo tomorrow?" → ["tokyo"]
///
/// Returns an empty `Vec` when no preposition appears — wttr's IP-geolocation
/// default handles the no-location case and the caller branches on `is_empty()`.
pub(crate) fn extract_weather_locations(query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    const PREPS: &[&str] = &[" in ", " at ", " for "];
    const TRAIL_NOISE: &[&str] = &[
        " today", " tomorrow", " tonight", " right now", " now", " this week",
        " this morning", " this afternoon", " this evening",
    ];
    const SPLITTERS: &[&str] = &[" and ", " or "];

    // Collect every preposition hit (start-of-location-phrase offset) and scan
    // the whole query — not just the first match — so compound queries like
    // "weather in Tokyo and what about weather in Paris" both register.
    let mut starts: Vec<usize> = Vec::new();
    for prep in PREPS {
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find(prep) {
            let abs = search_from + rel + prep.len();
            starts.push(abs);
            search_from = abs;
        }
    }
    starts.sort_unstable();

    let strip_trailing_noise = |s: &str| -> String {
        let mut cleaned = s.trim().to_string();
        loop {
            let before = cleaned.clone();
            for noise in TRAIL_NOISE {
                if cleaned.ends_with(noise) {
                    cleaned.truncate(cleaned.len() - noise.len());
                    cleaned = cleaned.trim_end().to_string();
                }
            }
            if cleaned == before { break; }
        }
        cleaned
    };

    let mut out:  Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for start in starts {
        if start >= lower.len() { continue; }
        let tail = &lower[start..];

        // Phrase ends at first clause boundary.
        let end = tail.find(|c: char| matches!(c, '?' | '.' | ',' | '!' | '\n'))
            .unwrap_or(tail.len());
        let phrase = &tail[..end];

        // Split on " and " / " or " so "Tokyo and Sacramento" yields both.
        // Manual multi-delimiter split to avoid pulling in `regex` for this.
        let mut parts: Vec<&str> = vec![phrase];
        for splitter in SPLITTERS {
            parts = parts.into_iter()
                .flat_map(|p| p.split(splitter))
                .collect();
        }

        for part in parts {
            let cleaned = strip_trailing_noise(part);
            if cleaned.is_empty() { continue; }
            if seen.insert(cleaned.clone()) {
                out.push(cleaned);
            }
        }
    }

    out
}

// ── Embedding input sanitation ────────────────────────────────────────────────

/// Truncate `input` to at most `MEMORY_EMBED_MAX_CHARS` Unicode scalar values,
/// returning an owned `String` suitable for an `EmbeddingRequest`.
///
/// `mxbai-embed-large` has a 512-token trained context; Ollama rejects longer
/// inputs with `400 "the input length exceeds the context length"`. The
/// pre-fix behavior was a silent drop of any conversation turn whose
/// `User: Q\nAssistant: A` concatenation exceeded the limit — observed in
/// production with ~3 KB assistant responses (e.g. long technical
/// explanations). This helper is the authoritative gate.
///
/// Truncation is at the Unicode-scalar-value level (not byte level), so
/// multi-byte UTF-8 characters (emoji, non-ASCII) never split mid-codepoint.
/// Head-preserving: the question / topic-introducing prefix is retained.
fn truncate_for_embed(input: &str) -> String {
    if input.chars().count() <= MEMORY_EMBED_MAX_CHARS {
        input.to_string()
    } else {
        input.chars().take(MEMORY_EMBED_MAX_CHARS).collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pipeline() -> RetrievalPipeline {
        RetrievalPipeline::new_degraded()
    }

    #[test]
    fn detect_pre_trigger_retrieval_first_returns_memory_search() {
        let p = make_pipeline();
        let trigger = p.detect_pre_trigger("what version of Python is installed?", true);
        assert!(trigger.is_some(), "is_retrieval_first=true must return Some");
        let trigger = trigger.unwrap();
        assert!(
            matches!(trigger, RetrievalTrigger::MemorySearch { .. }),
            "trigger must be MemorySearch variant"
        );
        assert_eq!(trigger.query(), "what version of Python is installed?");
    }

    // ── Embed-input truncation (mxbai-embed-large 512-token ceiling) ──────────

    #[test]
    fn truncate_for_embed_passes_short_input_unchanged() {
        let short = "User: hi\nAssistant: hello";
        let out = truncate_for_embed(short);
        assert_eq!(out, short,
            "inputs ≤ MEMORY_EMBED_MAX_CHARS must round-trip byte-for-byte");
    }

    #[test]
    fn truncate_for_embed_at_boundary_returns_unchanged() {
        // Exact boundary: no truncation fires.
        let s: String = "a".repeat(MEMORY_EMBED_MAX_CHARS);
        let out = truncate_for_embed(&s);
        assert_eq!(out.chars().count(), MEMORY_EMBED_MAX_CHARS);
        assert_eq!(out, s);
    }

    #[test]
    fn truncate_for_embed_caps_long_input_at_max_chars() {
        // Regression: reproduce the production crash where a 2923-char
        // "User: Q\nAssistant: A" turn blew the 512-token embed context and
        // was silently dropped from memory. Post-fix: the embedder always
        // receives ≤ MEMORY_EMBED_MAX_CHARS.
        let long: String = "x".repeat(MEMORY_EMBED_MAX_CHARS + 1_200);
        let out = truncate_for_embed(&long);
        assert_eq!(
            out.chars().count(),
            MEMORY_EMBED_MAX_CHARS,
            "oversized input must be capped at exactly MEMORY_EMBED_MAX_CHARS"
        );
    }

    #[test]
    fn truncate_for_embed_preserves_head_not_tail() {
        // Retrieval semantics depend on the head: "User: {question}" is the
        // strongest handle for cosine recall. The tail of a long assistant
        // answer is the right thing to drop.
        let head = "User: what is a ret2libc attack?\nAssistant: ";
        let tail: String = "z".repeat(MEMORY_EMBED_MAX_CHARS * 2);
        let input = format!("{head}{tail}");
        let out = truncate_for_embed(&input);
        assert!(
            out.starts_with(head),
            "truncation must be head-preserving; got prefix: {:?}",
            &out[..head.len().min(out.len())]
        );
    }

    #[test]
    fn truncate_for_embed_is_unicode_scalar_safe() {
        // Char-count truncation (not byte-count): multi-byte codepoints must
        // never be sliced mid-sequence. Build an input of 2× max-chars where
        // every codepoint is 4 bytes (👋 = U+1F44B, 4 UTF-8 bytes).
        let wave = '👋';
        let input: String = std::iter::repeat_n(wave, MEMORY_EMBED_MAX_CHARS * 2).collect();
        let out = truncate_for_embed(&input);
        assert_eq!(out.chars().count(), MEMORY_EMBED_MAX_CHARS);
        assert!(out.chars().all(|c| c == wave),
            "every surviving char must be the intact emoji — no mid-codepoint split");
    }

    // ── Phase 37.8: weather fast-path detection ───────────────────────────────

    #[test]
    fn is_weather_query_recognizes_common_phrasings() {
        assert!(is_weather_query("what's the weather"));
        assert!(is_weather_query("Weather in Tokyo?"));
        assert!(is_weather_query("forecast for tomorrow"));
        assert!(is_weather_query("how hot is it outside"));
        assert!(is_weather_query("is it raining right now"));
        assert!(is_weather_query("temperature today"));
        assert!(is_weather_query("humidity"));
    }

    #[test]
    fn is_weather_query_rejects_non_weather() {
        assert!(!is_weather_query("what's the time"));
        assert!(!is_weather_query("write a poem about rivers"));
        assert!(!is_weather_query("explain ret2libc"));
        assert!(!is_weather_query("how do I install rust"));
    }

    #[test]
    fn extract_weather_location_handles_in_at_for_prepositions() {
        assert_eq!(extract_weather_location("weather in Tokyo").as_deref(),       Some("tokyo"));
        assert_eq!(extract_weather_location("forecast at Paris").as_deref(),      Some("paris"));
        assert_eq!(extract_weather_location("weather for Buenos Aires").as_deref(), Some("buenos aires"));
    }

    #[test]
    fn extract_weather_location_strips_trailing_time_qualifiers_and_punctuation() {
        // The classifier lowercases the input, so location comes out lowercase.
        // wttr.in is case-insensitive on the location path component.
        assert_eq!(extract_weather_location("weather in Tokyo today?").as_deref(),     Some("tokyo"));
        assert_eq!(extract_weather_location("weather in NYC tomorrow").as_deref(),     Some("nyc"));
        assert_eq!(extract_weather_location("forecast for London right now").as_deref(), Some("london"));
        assert_eq!(extract_weather_location("weather in Paris this evening!").as_deref(), Some("paris"));
    }

    #[test]
    fn extract_weather_location_returns_none_when_no_preposition() {
        // No "in/at/for" → defer to wttr's IP geolocation default.
        assert!(extract_weather_location("what's the weather").is_none());
        assert!(extract_weather_location("how hot is it").is_none());
        assert!(extract_weather_location("is it raining").is_none());
    }

    // ── Phase 37.9: multi-city weather extraction ─────────────────────────────
    //
    // These tests pin the compound-question behavior regressed in the field:
    // "what's the weather in Tokyo? and what's the weather in Sacramento?"
    // previously extracted only "tokyo" and left Sacramento unanswered. The
    // plural variant now returns both locations in query order.

    #[test]
    fn extract_weather_locations_returns_empty_when_no_preposition() {
        assert!(extract_weather_locations("what's the weather").is_empty());
        assert!(extract_weather_locations("how hot is it").is_empty());
        assert!(extract_weather_locations("is it raining").is_empty());
    }

    #[test]
    fn extract_weather_locations_single_city_matches_legacy_extractor() {
        // Contract: with one named city, plural returns exactly the single-
        // extractor's answer in a one-element vec.
        let got = extract_weather_locations("weather in Tokyo today?");
        assert_eq!(got, vec!["tokyo".to_string()]);
    }

    #[test]
    fn extract_weather_locations_two_cities_and_boundary() {
        // Original production failure: "weather in Tokyo and Sacramento" only
        // answered Tokyo. The extractor now splits on " and " inside the
        // preposition phrase.
        let got = extract_weather_locations("what's the weather in Tokyo and Sacramento?");
        assert_eq!(got, vec!["tokyo".to_string(), "sacramento".to_string()]);
    }

    #[test]
    fn extract_weather_locations_two_separate_preposition_clauses() {
        // The production log's actual phrasing — two full clauses, each with
        // its own "in {city}". Each preposition hit is scanned independently.
        let got = extract_weather_locations(
            "what's the weather in Tokyo? and what's the weather in Sacramento?"
        );
        assert_eq!(got, vec!["tokyo".to_string(), "sacramento".to_string()]);
    }

    #[test]
    fn extract_weather_locations_or_boundary() {
        // Operators say "or" as often as "and" when comparing cities.
        let got = extract_weather_locations("weather in Paris or London");
        assert_eq!(got, vec!["paris".to_string(), "london".to_string()]);
    }

    #[test]
    fn extract_weather_locations_deduplicates_preserving_order() {
        // "weather in Tokyo. What about weather in Tokyo tomorrow?" must
        // collapse to one city, not send two identical wttr requests.
        let got = extract_weather_locations(
            "weather in Tokyo. What about weather in Tokyo tomorrow?"
        );
        assert_eq!(got, vec!["tokyo".to_string()]);
    }

    #[test]
    fn extract_weather_locations_three_cities() {
        // Broader compound: "Tokyo, Paris, and Sacramento". Comma is a clause
        // boundary for wttr's single-URL path, so the conjunction-aware split
        // only picks up the first preposition phrase ("Tokyo"); subsequent
        // cities need their own "in" anchor for now. This test pins that
        // limitation so we notice if we ever widen the extractor.
        let got = extract_weather_locations("weather in Tokyo, Paris, and Sacramento");
        assert_eq!(got, vec!["tokyo".to_string()]);
    }

    #[test]
    fn extract_weather_locations_strips_time_qualifiers_per_city() {
        // Trailing time qualifiers must be stripped from each split part, not
        // just the first — else "Tokyo today and Sacramento tomorrow" leaves
        // "sacramento tomorrow" in the output.
        let got = extract_weather_locations("weather in Tokyo today and Sacramento tomorrow");
        assert_eq!(got, vec!["tokyo".to_string(), "sacramento".to_string()]);
    }

    #[test]
    fn extract_weather_location_singular_returns_first_of_plural() {
        // Back-compat wrapper contract: single extractor returns plural[0].
        assert_eq!(
            extract_weather_location("weather in Tokyo and Sacramento").as_deref(),
            Some("tokyo")
        );
    }

    #[test]
    fn detect_pre_trigger_chat_returns_none() {
        let p = make_pipeline();
        let trigger = p.detect_pre_trigger("write me a poem about rivers", false);
        assert!(trigger.is_none(), "is_retrieval_first=false must return None");
    }

    #[test]
    fn detect_post_trigger_extracts_topic_from_marker() {
        let p = make_pipeline();
        let response = "I'm not certain about X. Let me check that for you.";
        let trigger = p.detect_post_trigger(response);
        assert!(trigger.is_some(), "uncertainty marker must trigger post-retrieval");
        match trigger.unwrap() {
            RetrievalTrigger::UncertaintyMarker { topic } => {
                assert_eq!(topic, "X", "topic must be extracted between marker and '.'");
            }
            other => panic!("expected UncertaintyMarker, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn detect_post_trigger_no_marker_returns_none() {
        let p = make_pipeline();
        let response = "Python 3.14 is the latest version as of early 2026.";
        assert!(p.detect_post_trigger(response).is_none(),
            "clean response without UNCERTAINTY_MARKER must return None");
    }

    #[test]
    fn format_for_injection_empty_context_returns_empty_string() {
        let p = make_pipeline();
        let ctx = RetrievalContext {
            query:       "test".to_string(),
            memory_hits: vec![],
            web_result:  None,
            trigger:     RetrievalTrigger::MemorySearch { query: "test".to_string() },
        };
        assert_eq!(p.format_for_injection(&ctx), "",
            "no hits and no web result must produce empty string");
    }

    #[test]
    fn format_for_injection_includes_memory_and_web_results() {
        let p = make_pipeline();
        let ctx = RetrievalContext {
            query: "test".to_string(),
            memory_hits: vec![
                MemoryEntry {
                    id:         "id-1".to_string(),
                    content:    "remembered content".to_string(),
                    source:     "session:s1".to_string(),
                    entry_type: "conversation_turn".to_string(),
                    session_id: Some("s1".to_string()),
                    created_at: "2026-03-08T00:00:00Z".to_string(),
                    similarity: 0.9,
                },
            ],
            web_result: Some(FetchResult {
                url:        "https://example.com".to_string(),
                title:      Some("Example Page".to_string()),
                text:       "web content here".to_string(),
                fetched_at: "2026-03-08T00:00:00Z".to_string(),
            }),
            trigger: RetrievalTrigger::MemorySearch { query: "test".to_string() },
        };
        let formatted = p.format_for_injection(&ctx);
        assert!(formatted.contains("remembered content"),
            "memory hit content must appear in output");
        assert!(formatted.contains("session:s1"),
            "memory hit source must appear in output");
        assert!(formatted.contains("web content here"),
            "web result text must appear in output");
        assert!(formatted.contains("Example Page"),
            "web result title must appear in output");
    }

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
}
