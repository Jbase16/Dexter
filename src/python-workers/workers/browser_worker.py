"""
Dexter browser automation worker — Playwright Chromium subprocess.

Long-lived: one browser process + one Page per session. Cookies, auth state,
and page history persist across commands within the session. The process exits
when it receives MSG_SHUTDOWN or when stdin closes (Rust core exited).

Threading: single-threaded asyncio event loop. All I/O is async.
Blocking stdin reads are offloaded to the default thread pool executor so
Playwright's internal asyncio timers can run while waiting for commands.
"""
import asyncio
import json
import sys
import re
import time
from pathlib import Path
from typing import Any, Optional

from playwright.async_api import Page, TimeoutError as PlaywrightTimeoutError, async_playwright

from workers.protocol import (
    MSG_BROWSER_CLICK,
    MSG_BROWSER_EXTRACT,
    MSG_BROWSER_NAVIGATE,
    MSG_BROWSER_RESULT,
    MSG_BROWSER_SCREENSHOT,
    MSG_BROWSER_TYPE,
    MSG_HEALTH_PING,
    MSG_HEALTH_PONG,
    MSG_SHUTDOWN,
    read_frame,
    send_frame,
    write_handshake,
    write_startup_error_handshake,
)

# Screenshots are saved here; directory is created on first use.
# Phase 37 / B6: default to the operator's Desktop so screenshots are visible
# without hunting through /tmp. Desktop always exists on macOS; mkdir(exist_ok=True)
# is harmless if the directory pre-exists.
SCREENSHOT_DIR = Path.home() / "Desktop"
SELECTOR_CANDIDATE_MAX_COUNT = 30
SELECTOR_CANDIDATE_LABEL_MAX_CHARS = 80


async def browser_result(
    *,
    success: bool,
    output: str = "",
    error: str = "",
    error_kind: str | None = None,
    page: Page | None = None,
    selector: str | None = None,
) -> dict:
    """Build a browser result with optional page-state diagnostics."""
    result = {"success": success, "output": output, "error": error}
    if error_kind:
        result["error_kind"] = error_kind
    if selector:
        result["selector"] = selector
    if page is not None:
        try:
            result["page_url"] = page.url
        except Exception:
            pass
        try:
            title = await page.title()
            if title:
                result["page_title"] = title
        except Exception:
            pass
    return result


def classify_navigation_error(error: Exception) -> str:
    detail = str(error).lower()
    if isinstance(error, PlaywrightTimeoutError) or "timeout" in detail:
        return "navigation_timeout"
    if (
        "net::err_file_not_found" in detail
        or "net::err_name_not_resolved" in detail
        or "net::err_connection_refused" in detail
        or "net::err_connection_timed_out" in detail
        or "net::err_internet_disconnected" in detail
        or "net::err_aborted" in detail
    ):
        return "navigation_failed"
    if "page" in detail and "closed" in detail:
        return "page_not_ready"
    return "navigation_failed"


def classify_page_action_error(error: Exception, fallback: str) -> str:
    detail = str(error).lower()
    if isinstance(error, PlaywrightTimeoutError) or "timeout" in detail:
        return fallback
    if "page" in detail and "closed" in detail:
        return "page_not_ready"
    if "target closed" in detail or "browser has been closed" in detail:
        return "page_not_ready"
    return fallback


def _compact_selector_label(value: object) -> str:
    text = re.sub(r"\s+", " ", str(value or "")).strip()
    if len(text) > SELECTOR_CANDIDATE_LABEL_MAX_CHARS:
        return text[:SELECTOR_CANDIDATE_LABEL_MAX_CHARS - 1].rstrip() + "…"
    return text


def _format_selector_candidates(rows: list[dict[str, Any]]) -> str:
    lines: list[str] = []
    for row in rows[:SELECTOR_CANDIDATE_MAX_COUNT]:
        selectors = row.get("selectors")
        if not isinstance(selectors, list):
            continue
        selector_text = " / ".join(str(selector) for selector in selectors[:3] if selector)
        if not selector_text:
            continue
        tag = _compact_selector_label(row.get("tag"))
        label = _compact_selector_label(row.get("label"))
        if label:
            lines.append(f"- {selector_text} ({tag}, text={label!r})")
        else:
            lines.append(f"- {selector_text} ({tag})")

    if not lines:
        return ""
    return "Candidate selectors:\n" + "\n".join(lines)


async def extract_selector_candidates(page: Page) -> str:
    """Return a bounded list of visible selectors useful for browser replanning."""
    try:
        rows = await page.evaluate(
            """() => {
                const maxCount = 30;
                const maxLabel = 80;
                const clean = (value) => String(value || "")
                    .replace(/\\s+/g, " ")
                    .trim()
                    .slice(0, maxLabel);
                const attr = (name, value) => {
                    if (!value) return null;
                    return `[${name}="${String(value).replace(/\\\\/g, "\\\\\\\\").replace(/"/g, "\\\\\\"")}"]`;
                };
                const idSelector = (id) => {
                    if (!id) return null;
                    if (window.CSS && CSS.escape) return `#${CSS.escape(id)}`;
                    return attr("id", id);
                };
                const isVisible = (el) => {
                    const style = window.getComputedStyle(el);
                    const rect = el.getBoundingClientRect();
                    return style.visibility !== "hidden"
                        && style.display !== "none"
                        && rect.width > 0
                        && rect.height > 0;
                };
                const selectorFor = (el) => {
                    const tag = el.tagName.toLowerCase();
                    const selectors = [];
                    const push = (selector) => {
                        if (selector && !selectors.includes(selector)) selectors.push(selector);
                    };
                    const id = idSelector(el.id);
                    if (id) {
                        push(id);
                        push(`${tag}${id}`);
                    }
                    const testId = attr("data-testid", el.getAttribute("data-testid"));
                    if (testId) {
                        push(testId);
                        push(`${tag}${testId}`);
                    }
                    const name = attr("name", el.getAttribute("name"));
                    if (name) push(`${tag}${name}`);
                    const aria = attr("aria-label", el.getAttribute("aria-label"));
                    if (aria) push(`${tag}${aria}`);
                    const role = attr("role", el.getAttribute("role"));
                    if (role) push(`${tag}${role}`);
                    const href = attr("href", el.getAttribute("href"));
                    if (href && tag === "a") push(`${tag}${href}`);
                    return selectors.slice(0, 3);
                };
                const elements = Array.from(document.querySelectorAll(
                    "button, a[href], input, textarea, select, [role], [onclick], [id], [data-testid], [aria-label]"
                ));
                const rows = [];
                for (const el of elements) {
                    if (!isVisible(el)) continue;
                    const selectors = selectorFor(el);
                    if (!selectors.length) continue;
                    const tag = el.tagName.toLowerCase();
                    const label = clean(
                        el.innerText
                        || el.value
                        || el.getAttribute("aria-label")
                        || el.getAttribute("title")
                        || el.getAttribute("name")
                        || el.id
                    );
                    rows.push({ tag, label, selectors });
                    if (rows.length >= maxCount) break;
                }
                return rows;
            }"""
        )
        if not isinstance(rows, list):
            return ""
        return _format_selector_candidates(rows)
    except Exception:
        return ""


# ── Command handlers ──────────────────────────────────────────────────────────
# Each handler returns a JSON-serializable dict.
# Exceptions are caught internally — no handler raises.

async def handle_navigate(page: Page, payload: bytes) -> dict:
    """Navigate to a URL. Returns the final URL and page title on success.

    The page title is included so the model immediately knows what actually
    loaded — "Age Verification | Pornhub" vs "Search Results | Pornhub" vs
    "Access Denied | Cloudflare" — without needing a separate extract step.
    """
    cmd = json.loads(payload)
    url = cmd["url"]
    timeout_ms = cmd.get("timeout_ms", 30_000)
    try:
        await page.goto(url, timeout=timeout_ms)
        title = await page.title()
        output = f"{page.url}"
        if title:
            output += f"\nPage title: {title}"
        return await browser_result(success=True, output=output, page=page)
    except Exception as e:
        return await browser_result(
            success=False,
            error=str(e),
            error_kind=classify_navigation_error(e),
            page=page,
        )


async def handle_click(page: Page, payload: bytes) -> dict:
    """Click an element by CSS selector."""
    cmd = json.loads(payload)
    selector = cmd["selector"]
    timeout_ms = cmd.get("timeout_ms", 10_000)
    try:
        # Check existence synchronously BEFORE clicking. page.click() is a polling
        # wait — if the element never exists it burns the full timeout_ms (10s by
        # default) before raising an exception. locator.count() is instant and
        # gives us an informative diagnostic immediately.
        count = await page.locator(selector).count()
        if count == 0:
            return await browser_result(
                success=False,
                error=(
                    f"element not found: {selector!r}. "
                    "The page may be showing an age gate, CAPTCHA, or different structure. "
                    "Run a null-selector extract to inspect the current page."
                ),
                error_kind="selector_not_found",
                page=page,
                selector=selector,
            )
        await page.click(selector, timeout=timeout_ms)
        return await browser_result(success=True, output=f"clicked: {selector}", page=page)
    except Exception as e:
        return await browser_result(
            success=False,
            error=str(e),
            error_kind=classify_page_action_error(e, "click_failed"),
            page=page,
            selector=selector,
        )


async def handle_type(page: Page, payload: bytes) -> dict:
    """Fill an input element with text. Uses fill() which clears existing content first."""
    cmd = json.loads(payload)
    selector = cmd["selector"]
    text = cmd["text"]
    timeout_ms = cmd.get("timeout_ms", 10_000)
    try:
        # Same existence-check-first pattern as handle_click — fill() also
        # polls up to timeout_ms when the element doesn't exist.
        count = await page.locator(selector).count()
        if count == 0:
            return await browser_result(
                success=False,
                error=(
                    f"element not found: {selector!r}. "
                    "Cannot type into a missing element. Run a null-selector extract "
                    "to inspect the current page."
                ),
                error_kind="selector_not_found",
                page=page,
                selector=selector,
            )
        await page.fill(selector, text, timeout=timeout_ms)
        return await browser_result(success=True, output=f"typed into: {selector}", page=page)
    except Exception as e:
        return await browser_result(
            success=False,
            error=str(e),
            error_kind=classify_page_action_error(e, "typing_failed"),
            page=page,
            selector=selector,
        )


async def handle_extract(page: Page, payload: bytes) -> dict:
    """Extract content from the current page.

    When a selector is given:
      - Collects all matching elements (query_selector_all, not just the first).
      - For each element, extracts all <a href="..."> links as "Title → URL" pairs.
      - Falls back to inner_text() if the element contains no links.
    When selector is null:
      - Extracts all <a href="..."> links from the full page body as "Title → URL" pairs.
      - Falls back to full page inner_text() if no links found.

    Always returns links when present so the caller has real URLs to work with.
    10k char cap prevents runaway payloads from large pages.
    """
    cmd = json.loads(payload)
    selector: Optional[str] = cmd.get("selector")
    try:
        if selector:
            elements = await page.query_selector_all(selector)
            if not elements:
                # Return a diagnostic string rather than empty — the Rust layer
                # converts empty output to "Done." which completely hides the
                # failure from the model and causes it to loop (e.g. trying to
                # click elements that never loaded).
                return await browser_result(
                    success=False,
                    error=(
                        f"no elements found for selector: {selector!r}. "
                        "The page may not have loaded correctly, or may be showing an age gate/CAPTCHA. "
                        "Run a null-selector extract to inspect the full page."
                    ),
                    error_kind="selector_not_found",
                    page=page,
                    selector=selector,
                )
            parts: list[str] = []
            for el in elements:
                # Prefer links over bare text — links carry the actionable URL.
                anchors = await el.query_selector_all("a[href]")
                if anchors:
                    for a in anchors:
                        href  = (await a.get_attribute("href")) or ""
                        title = (await a.inner_text()).strip()
                        if href:
                            # Resolve relative URLs against the page origin.
                            if href.startswith("/"):
                                origin = "/".join(page.url.split("/")[:3])
                                href = origin + href
                            parts.append(f"{title} → {href}" if title else href)
                else:
                    text = (await el.inner_text()).strip()
                    if text:
                        parts.append(text)
            result = "\n".join(parts)
        else:
            # Full-page link extraction first; fall back to body text.
            selector_summary = await extract_selector_candidates(page)
            anchors = await page.query_selector_all("a[href]")
            if anchors:
                parts = []
                for a in anchors:
                    href  = (await a.get_attribute("href")) or ""
                    title = (await a.inner_text()).strip()
                    if href and not href.startswith(("#", "javascript:")):
                        if href.startswith("/"):
                            origin = "/".join(page.url.split("/")[:3])
                            href = origin + href
                        parts.append(f"{title} → {href}" if title else href)
                result = "\n".join(parts)
            else:
                result = await page.inner_text("body")
            if selector_summary:
                result = f"{selector_summary}\n\nVisible text:\n{result}"
        # 10k char cap prevents runaway payloads from large pages.
        return await browser_result(success=True, output=result[:10_000], page=page)
    except Exception as e:
        return await browser_result(
            success=False,
            error=str(e),
            error_kind=classify_page_action_error(e, "extraction_failed"),
            page=page,
            selector=selector,
        )


async def handle_screenshot(page: Page, _payload: bytes) -> dict:
    """Save a screenshot to /tmp/dexter-screenshots/ and return the path."""
    try:
        SCREENSHOT_DIR.mkdir(parents=True, exist_ok=True)
        # Timestamp in ms provides uniqueness without requiring UUIDs.
        ts = int(time.time() * 1000)
        path = SCREENSHOT_DIR / f"screenshot_{ts}.png"
        await page.screenshot(path=str(path))
        return await browser_result(success=True, output=str(path), page=page)
    except Exception as e:
        return await browser_result(
            success=False,
            error=str(e),
            error_kind=classify_page_action_error(e, "screenshot_failed"),
            page=page,
        )


# ── Main loop ─────────────────────────────────────────────────────────────────

async def run(stdin, stdout) -> None:
    async with async_playwright() as pw:
        try:
            # Launch with AutomationControlled disabled so sites don't fingerprint us as
            # headless automation via the navigator.webdriver property or the
            # "HeadlessChrome" user-agent token.
            browser = await pw.chromium.launch(
                headless=True,
                args=["--disable-blink-features=AutomationControlled"],
            )
            context = await browser.new_context(
                user_agent=(
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
                    "AppleWebKit/537.36 (KHTML, like Gecko) "
                    "Chrome/122.0.0.0 Safari/537.36"
                ),
                viewport={"width": 1280, "height": 800},
                locale="en-US",
            )
            page = await context.new_page()
            # Belt-and-suspenders: override the webdriver property at the JS layer so
            # even scripts that check navigator.webdriver directly get undefined.
            await page.add_init_script(
                "Object.defineProperty(navigator, 'webdriver', {get: () => undefined})"
            )
        except Exception as e:
            write_startup_error_handshake(
                stdout,
                "browser",
                "browser_launch_failed",
                str(e),
            )
            return

        write_handshake(stdout, "browser")

        dispatch = {
            MSG_BROWSER_NAVIGATE:   handle_navigate,
            MSG_BROWSER_CLICK:      handle_click,
            MSG_BROWSER_TYPE:       handle_type,
            MSG_BROWSER_EXTRACT:    handle_extract,
            MSG_BROWSER_SCREENSHOT: handle_screenshot,
        }

        loop = asyncio.get_event_loop()

        while True:
            # Offload the blocking stdin read to the thread pool executor.
            # This allows Playwright's internal asyncio timers (element visibility
            # waits, network idle, etc.) to run while we wait for the next command.
            msg_type, payload = await loop.run_in_executor(None, read_frame, stdin)

            if msg_type is None:
                # EOF — Rust core exited; clean up and exit.
                break

            if msg_type == MSG_SHUTDOWN:
                break
            elif msg_type == MSG_HEALTH_PING:
                send_frame(stdout, MSG_HEALTH_PONG, b"")
            elif msg_type in dispatch:
                result = json.dumps(await dispatch[msg_type](page, payload or b""))
                send_frame(stdout, MSG_BROWSER_RESULT, result.encode())
            # Unknown message types are silently dropped — forward protocol compat.

        await context.close()
        await browser.close()


if __name__ == "__main__":
    asyncio.run(run(sys.stdin.buffer, sys.stdout.buffer))
