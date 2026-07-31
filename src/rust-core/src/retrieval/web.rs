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
use futures_util::StreamExt;
use reqwest::{header::LOCATION, redirect::Policy as RedirectPolicy, StatusCode, Url};
use scraper::{Html, Selector};
use thiserror::Error;

use crate::constants::{
    CORE_VERSION, RETRIEVAL_MAX_CONTENT_CHARS, RETRIEVAL_MAX_REDIRECTS,
    RETRIEVAL_MAX_RESPONSE_BYTES, RETRIEVAL_WEB_TIMEOUT_SECS,
};

// ── FetchResult ───────────────────────────────────────────────────────────────

/// The readable text extracted from a fetched web page.
#[allow(dead_code)] // Phase 10+ callers read fetched_at for provenance display
#[derive(Debug)]
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
    client: Result<reqwest::Client, String>,
    request_timeout: Duration,
    allow_loopback_http: bool,
}

/// Typed retrieval failures. Error messages deliberately omit query strings and
/// response bodies so operator content cannot be copied into logs accidentally.
#[derive(Debug, Error)]
pub enum WebRetrievalError {
    #[error("retrieval URL is invalid")]
    InvalidUrl,
    #[error("retrieval requires HTTPS")]
    DisallowedScheme,
    #[error("retrieval destination is not allowed")]
    DisallowedDestination,
    #[error("retrieval redirect is missing a valid Location header")]
    InvalidRedirect,
    #[error("retrieval exceeded the redirect limit")]
    TooManyRedirects,
    #[error("retrieval response status was {0}")]
    HttpStatus(StatusCode),
    #[error("retrieval response exceeded {limit} bytes")]
    ResponseTooLarge { limit: usize },
    #[error("retrieval response was not valid UTF-8")]
    InvalidUtf8,
    #[error("retrieval client is unavailable")]
    ClientUnavailable,
    #[error("retrieval request timed out")]
    Timeout,
    #[error("retrieval request failed: {0}")]
    Request(#[from] reqwest::Error),
}

impl WebRetrievalError {
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            Self::HttpStatus(status) => Some(*status),
            Self::Request(error) => error.status(),
            _ => None,
        }
    }
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
            // Redirects are followed manually so each destination is checked
            // before the next request is opened.
            .redirect(RedirectPolicy::none())
            .build()
            .map_err(|error| error.to_string());
        Self {
            client,
            request_timeout: Duration::from_secs(timeout_secs),
            allow_loopback_http: false,
        }
    }

    #[cfg(test)]
    fn new_for_loopback_tests(timeout_secs: u64) -> Self {
        let mut retriever = Self::new(timeout_secs);
        retriever.allow_loopback_http = true;
        retriever
    }

    /// Create a WebRetriever with the default `RETRIEVAL_WEB_TIMEOUT_SECS` timeout.
    pub fn default_timeout() -> Self {
        Self::new(RETRIEVAL_WEB_TIMEOUT_SECS)
    }

    /// GET `url`, decode body as UTF-8, extract text via `extract_text()`.
    ///
    /// Returns `Err` on network failure, non-2xx HTTP status, or non-UTF-8 body.
    pub async fn fetch(&self, url: &str) -> Result<FetchResult, WebRetrievalError> {
        let (response, final_url) = self.send_with_redirects(url, self.request_timeout).await?;
        let html = read_bounded_utf8(response).await?;
        let (title, text) = extract_text(&html);
        Ok(FetchResult {
            url: final_url.to_string(),
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
    ) -> Result<FetchResult, WebRetrievalError> {
        let (response, final_url) = self
            .send_with_redirects(url, Duration::from_secs(timeout_secs))
            .await?;
        let text = read_bounded_utf8(response).await?;
        // Trim trailing whitespace/newlines that text endpoints often append.
        let text = truncate_to_chars(text.trim().to_string(), RETRIEVAL_MAX_CONTENT_CHARS);
        Ok(FetchResult {
            url: final_url.to_string(),
            title: None,
            text,
            fetched_at: Utc::now().to_rfc3339(),
        })
    }

    async fn send_with_redirects(
        &self,
        url: &str,
        timeout: Duration,
    ) -> Result<(reqwest::Response, Url), WebRetrievalError> {
        let mut current = Url::parse(url).map_err(|_| WebRetrievalError::InvalidUrl)?;
        let deadline = tokio::time::Instant::now() + timeout;
        let client = self
            .client
            .as_ref()
            .map_err(|_| WebRetrievalError::ClientUnavailable)?;

        for redirect_count in 0..=RETRIEVAL_MAX_REDIRECTS {
            self.validate_destination(&current)?;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(WebRetrievalError::Timeout);
            }
            let response = client
                .get(current.clone())
                .timeout(remaining)
                .send()
                .await?;

            if response.status().is_redirection() {
                validate_redirect_count(redirect_count)?;
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(WebRetrievalError::InvalidRedirect)?;
                current = resolve_redirect(&current, location)?;
                // The next loop iteration validates the resolved absolute URL
                // before opening the redirected request.
                continue;
            }

            if !response.status().is_success() {
                return Err(WebRetrievalError::HttpStatus(response.status()));
            }
            return Ok((response, current));
        }

        Err(WebRetrievalError::TooManyRedirects)
    }

    fn validate_destination(&self, url: &Url) -> Result<(), WebRetrievalError> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err(WebRetrievalError::DisallowedDestination);
        }

        let host = url
            .host_str()
            .ok_or(WebRetrievalError::DisallowedDestination)?;
        let is_loopback = is_loopback_host(host);
        let is_local_or_special = is_local_or_special_host(host);
        if is_local_or_special && !(self.allow_loopback_http && is_loopback) {
            return Err(WebRetrievalError::DisallowedDestination);
        }
        match url.scheme() {
            "https" => Ok(()),
            "http" if self.allow_loopback_http && is_loopback => Ok(()),
            "http" => Err(WebRetrievalError::DisallowedScheme),
            _ => Err(WebRetrievalError::DisallowedScheme),
        }
    }
}

async fn read_bounded_utf8(response: reqwest::Response) -> Result<String, WebRetrievalError> {
    validate_content_length(response.content_length())?;

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        append_bounded(&mut body, &chunk)?;
    }
    String::from_utf8(body).map_err(|_| WebRetrievalError::InvalidUtf8)
}

fn resolve_redirect(current: &Url, location: &str) -> Result<Url, WebRetrievalError> {
    current
        .join(location)
        .map_err(|_| WebRetrievalError::InvalidRedirect)
}

fn validate_content_length(content_length: Option<u64>) -> Result<(), WebRetrievalError> {
    if content_length.is_some_and(|length| length > RETRIEVAL_MAX_RESPONSE_BYTES as u64) {
        Err(WebRetrievalError::ResponseTooLarge {
            limit: RETRIEVAL_MAX_RESPONSE_BYTES,
        })
    } else {
        Ok(())
    }
}

fn validate_redirect_count(redirect_count: usize) -> Result<(), WebRetrievalError> {
    if redirect_count >= RETRIEVAL_MAX_REDIRECTS {
        Err(WebRetrievalError::TooManyRedirects)
    } else {
        Ok(())
    }
}

fn append_bounded(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), WebRetrievalError> {
    if body.len().saturating_add(chunk.len()) > RETRIEVAL_MAX_RESPONSE_BYTES {
        return Err(WebRetrievalError::ResponseTooLarge {
            limit: RETRIEVAL_MAX_RESPONSE_BYTES,
        });
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    if normalized == "localhost" || normalized.ends_with(".localhost") {
        return true;
    }
    normalized
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn is_local_or_special_host(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    if normalized == "localhost"
        || normalized.ends_with(".localhost")
        || normalized.ends_with(".local")
    {
        return true;
    }

    match normalized.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_multicast()
                || address.octets()[0] == 0
        }
        Ok(std::net::IpAddr::V6(address)) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || address.is_multicast()
        }
        Err(_) => false,
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

    #[test]
    fn destination_validation_requires_https_and_blocks_loopback() {
        let retriever = WebRetriever::default_timeout();
        assert!(retriever
            .validate_destination(&Url::parse("https://example.com/path").unwrap())
            .is_ok());
        assert!(matches!(
            retriever.validate_destination(&Url::parse("http://example.com/path").unwrap()),
            Err(WebRetrievalError::DisallowedScheme)
        ));
        assert!(matches!(
            retriever.validate_destination(&Url::parse("https://127.0.0.1/path").unwrap()),
            Err(WebRetrievalError::DisallowedDestination)
        ));
        assert!(matches!(
            retriever.validate_destination(&Url::parse("https://169.254.169.254/path").unwrap()),
            Err(WebRetrievalError::DisallowedDestination)
        ));
        assert!(matches!(
            retriever.validate_destination(&Url::parse("https://10.0.0.1/path").unwrap()),
            Err(WebRetrievalError::DisallowedDestination)
        ));
        assert!(matches!(
            retriever.validate_destination(&Url::parse("https://service.local/path").unwrap()),
            Err(WebRetrievalError::DisallowedDestination)
        ));
        assert!(matches!(
            retriever.validate_destination(&Url::parse("file:///tmp/secret").unwrap()),
            Err(WebRetrievalError::DisallowedDestination | WebRetrievalError::DisallowedScheme)
        ));
    }

    #[test]
    fn redirect_target_is_resolved_then_revalidated_before_following() {
        let retriever = WebRetriever::new_for_loopback_tests(2);
        let current = Url::parse("http://127.0.0.1/start").unwrap();
        let redirected = resolve_redirect(&current, "http://example.com/leak").unwrap();
        assert!(matches!(
            retriever.validate_destination(&redirected),
            Err(WebRetrievalError::DisallowedScheme)
        ));

        let relative = resolve_redirect(&current, "/next").unwrap();
        assert_eq!(relative.as_str(), "http://127.0.0.1/next");
        assert!(retriever.validate_destination(&relative).is_ok());
    }

    #[test]
    fn response_content_length_is_bounded_before_body_read() {
        assert!(validate_content_length(Some(RETRIEVAL_MAX_RESPONSE_BYTES as u64)).is_ok());
        assert!(validate_content_length(None).is_ok());
        assert!(matches!(
            validate_content_length(Some(RETRIEVAL_MAX_RESPONSE_BYTES as u64 + 1)),
            Err(WebRetrievalError::ResponseTooLarge { .. })
        ));
    }

    #[test]
    fn streamed_body_and_redirect_counts_are_bounded() {
        let mut body = vec![b'x'; RETRIEVAL_MAX_RESPONSE_BYTES - 1];
        assert!(append_bounded(&mut body, b"x").is_ok());
        assert!(matches!(
            append_bounded(&mut body, b"x"),
            Err(WebRetrievalError::ResponseTooLarge { .. })
        ));

        assert!(validate_redirect_count(RETRIEVAL_MAX_REDIRECTS - 1).is_ok());
        assert!(matches!(
            validate_redirect_count(RETRIEVAL_MAX_REDIRECTS),
            Err(WebRetrievalError::TooManyRedirects)
        ));
    }
}
