/// WebRetriever — Phase 9 web content fetcher.
///
/// Fetches a URL and extracts readable text from the HTML response via the
/// `scraper` crate (html5ever parser). Designed for single-page factual lookups —
/// not a crawler. Content is truncated at `RETRIEVAL_MAX_CONTENT_CHARS` to
/// prevent Wikipedia-scale articles from flooding the context window.
///
/// ## Text extraction priority
///
/// 1. First `<article>` element (long-form content pages)
/// 2. First `<main>` element (app pages)
/// 3. `<body>` element (fallback)
///
/// Before extraction: `<script>`, `<style>`, `<nav>`, `<footer>`, and `<header>`
/// subtrees are stripped. This prevents navigation links, cookie banners, and JS
/// from appearing as "content" in the retrieved context.
use std::time::Duration;

use chrono::Utc;
use scraper::{Html, Selector};

use crate::constants::{CORE_VERSION, RETRIEVAL_MAX_CONTENT_CHARS, RETRIEVAL_WEB_TIMEOUT_SECS};

// ── FetchResult ───────────────────────────────────────────────────────────────

/// The readable text extracted from a fetched web page.
#[allow(dead_code)] // Phase 10+ callers read fetched_at for provenance display
pub struct FetchResult {
    pub url: String,
    pub title: Option<String>,
    /// Extracted body text, ≤ `RETRIEVAL_MAX_CONTENT_CHARS` Unicode scalar values.
    pub text: String,
    /// ISO8601 timestamp of when the page was fetched.
    pub fetched_at: String,
}

// ── WebRetriever ──────────────────────────────────────────────────────────────

pub struct WebRetriever {
    client: reqwest::Client,
}

impl WebRetriever {
    /// Build a `reqwest::Client` with a `RETRIEVAL_WEB_TIMEOUT_SECS` timeout.
    ///
    /// User-Agent is "Dexter/{CORE_VERSION}" — identifies the bot courteously to
    /// servers that inspect the User-Agent for rate-limiting or blocking decisions.
    pub fn new(timeout_secs: u64) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .user_agent(format!("Dexter/{}", CORE_VERSION))
            .build()
            .expect("reqwest::Client build is infallible with valid config");
        Self { client }
    }

    /// Create a WebRetriever with the default `RETRIEVAL_WEB_TIMEOUT_SECS` timeout.
    pub fn default_timeout() -> Self {
        Self::new(RETRIEVAL_WEB_TIMEOUT_SECS)
    }

    /// GET `url`, decode body as UTF-8, extract text via `extract_text()`.
    ///
    /// Returns `Err` on network failure, non-2xx HTTP status, or non-UTF-8 body.
    pub async fn fetch(&self, url: &str) -> Result<FetchResult, reqwest::Error> {
        let response = self.client.get(url).send().await?;
        // Treat non-2xx as an error. reqwest doesn't do this automatically.
        let response = response.error_for_status()?;
        let html = response.text().await?;
        let (title, text) = extract_text(&html);
        Ok(FetchResult {
            url: url.to_string(),
            title,
            text,
            fetched_at: Utc::now().to_rfc3339(),
        })
    }

    /// GET `url` with an explicit per-call timeout, return the response body
    /// verbatim with NO HTML extraction.
    ///
    /// Use this for endpoints that return `text/plain`, JSON, or other
    /// non-HTML payloads. Phase 37.8: introduced for the wttr.in weather
    /// fast-path — wttr's `?format=3` returns a single line of plain text
    /// (e.g. `"San Francisco: ☀️ +64°F"`) which the regular `fetch()` would
    /// route through `Html::parse_document`. html5ever is lenient enough that
    /// it'd usually survive the round-trip, but relying on the parser to
    /// preserve a non-HTML body is fragile — this method bypasses it.
    ///
    /// `timeout_secs` overrides the client default; the wttr path uses
    /// `RETRIEVAL_WTTR_TIMEOUT_SECS` (4s) which is tighter than the global
    /// `RETRIEVAL_WEB_TIMEOUT_SECS` (10s) to keep retrieval bounded even when
    /// wttr is slow.
    pub async fn fetch_plain(
        &self,
        url: &str,
        timeout_secs: u64,
    ) -> Result<FetchResult, reqwest::Error> {
        let response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(timeout_secs))
            .send()
            .await?;
        let response = response.error_for_status()?;
        let text = response.text().await?;
        // Trim trailing whitespace/newlines that text endpoints often append.
        let text = text.trim().to_string();
        Ok(FetchResult {
            url: url.to_string(),
            title: None,
            text,
            fetched_at: Utc::now().to_rfc3339(),
        })
    }
}

// ── extract_text ──────────────────────────────────────────────────────────────

/// Extract (title, body_text) from raw HTML. Pure function — no network.
///
/// See module doc for the extraction priority and stripping rules.
pub(crate) fn extract_text(html: &str) -> (Option<String>, String) {
    let document = Html::parse_document(html);

    // ── Title ─────────────────────────────────────────────────────────────────
    let title_sel = Selector::parse("title").unwrap();
    let title = document
        .select(&title_sel)
        .next()
        .map(|el| normalize_whitespace(&el.text().collect::<String>()));

    // ── Body selection ────────────────────────────────────────────────────────
    // Priority: <article> > <main> > <body>. For each candidate, strip noise
    // subtrees before collecting text.
    let body_text = extract_body(&document);

    (title, body_text)
}

/// Try each body selector in priority order and return extracted text from the
/// first match. Falls back to an empty string if no selector matches (malformed
/// or headless HTML — rare in practice).
fn extract_body(document: &Html) -> String {
    let candidates = ["article", "main", "body"];
    let noise = ["script", "style", "nav", "footer", "header"];

    for tag in &candidates {
        let sel = Selector::parse(tag).unwrap();
        if let Some(root) = document.select(&sel).next() {
            let noise_sels: Vec<Selector> =
                noise.iter().map(|n| Selector::parse(n).unwrap()).collect();

            let text = filter_noise_descendants(root, &noise_sels);
            let text = normalize_whitespace(&text);

            if !text.is_empty() {
                return truncate_to_chars(text, RETRIEVAL_MAX_CONTENT_CHARS);
            }
        }
    }
    String::new()
}

/// Collect text content from `root`, skipping entire subtrees of `noise_sels`.
///
/// We recursively walk children, descending into each child only if it is NOT
/// matched by any noise selector. Text nodes are collected into a single string.
fn filter_noise_descendants(root: scraper::ElementRef<'_>, noise_sels: &[Selector]) -> String {
    use scraper::ElementRef;

    fn walk(el: ElementRef<'_>, noise_sels: &[Selector], out: &mut String) {
        for child in el.children() {
            if let Some(child_el) = ElementRef::wrap(child) {
                // If this child is a noise element, skip the whole subtree.
                if noise_sels.iter().any(|s| s.matches(&child_el)) {
                    continue;
                }
                walk(child_el, noise_sels, out);
            } else if let Some(text) = child.value().as_text() {
                out.push_str(text);
            }
        }
    }

    let mut out = String::new();
    walk(root, noise_sels, &mut out);
    out
}

/// Collapse runs of whitespace (spaces, tabs, newlines) into a single space and trim.
fn normalize_whitespace(s: &str) -> String {
    let mut prev_space = true; // start true so leading whitespace is dropped
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                result.push(' ');
                prev_space = true;
            }
        } else {
            result.push(c);
            prev_space = false;
        }
    }
    result.trim_end().to_string()
}

/// Truncate `s` to at most `max_chars` Unicode scalar values.
fn truncate_to_chars(s: String, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s
    } else {
        s.chars().take(max_chars).collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_returns_article_content() {
        // When both <article> and <main> are present, <article> wins.
        let html = r#"<!DOCTYPE html><html>
            <head><title>Test Page</title></head>
            <body>
                <nav>nav stuff</nav>
                <main>main content</main>
                <article>article content</article>
            </body>
        </html>"#;
        let (title, text) = extract_text(html);
        assert_eq!(title.as_deref(), Some("Test Page"));
        assert!(
            text.contains("article content"),
            "article element must win over main; got: {text:?}"
        );
        assert!(
            !text.contains("main content"),
            "main content must be absent when article is present; got: {text:?}"
        );
    }

    #[test]
    fn extract_text_falls_back_to_body() {
        // No <article> or <main> — falls back to <body>.
        let html = r#"<!DOCTYPE html><html>
            <head><title>Fallback</title></head>
            <body>
                <p>body paragraph</p>
            </body>
        </html>"#;
        let (_title, text) = extract_text(html);
        assert!(
            text.contains("body paragraph"),
            "should fall back to body text; got: {text:?}"
        );
    }

    #[test]
    fn extract_text_strips_script_and_style() {
        let html = r#"<!DOCTYPE html><html>
            <body>
                <article>
                    <script>var x = 1;</script>
                    <style>.foo { color: red; }</style>
                    <nav>nav link</nav>
                    <header>site header</header>
                    <footer>footer text</footer>
                    <p>real content</p>
                </article>
            </body>
        </html>"#;
        let (_title, text) = extract_text(html);
        assert!(
            text.contains("real content"),
            "body text must be present; got: {text:?}"
        );
        assert!(
            !text.contains("var x"),
            "script must be stripped; got: {text:?}"
        );
        assert!(
            !text.contains("color: red"),
            "style must be stripped; got: {text:?}"
        );
        assert!(
            !text.contains("nav link"),
            "nav must be stripped; got: {text:?}"
        );
        assert!(
            !text.contains("site header"),
            "header must be stripped; got: {text:?}"
        );
        assert!(
            !text.contains("footer text"),
            "footer must be stripped; got: {text:?}"
        );
    }

    #[test]
    fn extract_text_truncates_at_max_chars() {
        // Build an <article> body with RETRIEVAL_MAX_CONTENT_CHARS + 100 'x' chars.
        let body_len = RETRIEVAL_MAX_CONTENT_CHARS + 100;
        let long_text: String = "x".repeat(body_len);
        let html = format!(
            r#"<!DOCTYPE html><html><body><article><p>{}</p></article></body></html>"#,
            long_text
        );
        let (_title, text) = extract_text(&html);
        assert_eq!(
            text.chars().count(),
            RETRIEVAL_MAX_CONTENT_CHARS,
            "text must be truncated to exactly RETRIEVAL_MAX_CONTENT_CHARS chars"
        );
    }
}
