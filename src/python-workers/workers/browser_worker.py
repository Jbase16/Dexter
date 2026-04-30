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
import time
from pathlib import Path
from typing import Optional

from playwright.async_api import Page, async_playwright

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
)

# Screenshots are saved here; directory is created on first use.
# Phase 37 / B6: default to the operator's Desktop so screenshots are visible
# without hunting through /tmp. Desktop always exists on macOS; mkdir(exist_ok=True)
# is harmless if the directory pre-exists.
SCREENSHOT_DIR = Path.home() / "Desktop"


# ── Command handlers ──────────────────────────────────────────────────────────
# Each handler returns (success: bool, output: str, error: str).
# Exceptions are caught internally — no handler raises.

async def handle_navigate(page: Page, payload: bytes) -> tuple[bool, str, str]:
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
        return True, output, ""
    except Exception as e:
        return False, "", str(e)


async def handle_click(page: Page, payload: bytes) -> tuple[bool, str, str]:
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
            title = await page.title()
            return (
                True,
                f"[element not found: {selector!r} — page title is '{title}'. "
                f"The page may be showing an age gate, CAPTCHA, or different structure. "
                f"Do a null-selector extract to see what is actually on the page.]",
                "",
            )
        await page.click(selector, timeout=timeout_ms)
        return True, f"clicked: {selector}", ""
    except Exception as e:
        return False, "", str(e)


async def handle_type(page: Page, payload: bytes) -> tuple[bool, str, str]:
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
            title = await page.title()
            return (
                True,
                f"[element not found: {selector!r} — page title is '{title}'. "
                f"Cannot type into a missing element. Do a null-selector extract "
                f"to see the actual page content.]",
                "",
            )
        await page.fill(selector, text, timeout=timeout_ms)
        return True, f"typed into: {selector}", ""
    except Exception as e:
        return False, "", str(e)


async def handle_extract(page: Page, payload: bytes) -> tuple[bool, str, str]:
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
                return True, f"[no elements found for selector: {selector!r} — the page may not have loaded correctly, or may be showing an age gate/CAPTCHA. Try null-selector extract to see the full page.]", ""
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
        # 10k char cap prevents runaway payloads from large pages.
        return True, result[:10_000], ""
    except Exception as e:
        return False, "", str(e)


async def handle_screenshot(page: Page, _payload: bytes) -> tuple[bool, str, str]:
    """Save a screenshot to /tmp/dexter-screenshots/ and return the path."""
    try:
        SCREENSHOT_DIR.mkdir(parents=True, exist_ok=True)
        # Timestamp in ms provides uniqueness without requiring UUIDs.
        ts = int(time.time() * 1000)
        path = SCREENSHOT_DIR / f"screenshot_{ts}.png"
        await page.screenshot(path=str(path))
        return True, str(path), ""
    except Exception as e:
        return False, "", str(e)


# ── Main loop ─────────────────────────────────────────────────────────────────

async def run(stdin, stdout) -> None:
    write_handshake(stdout, "browser")

    async with async_playwright() as pw:
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
                success, output, error = await dispatch[msg_type](page, payload or b"")
                result = json.dumps({"success": success, "output": output, "error": error})
                send_frame(stdout, MSG_BROWSER_RESULT, result.encode())
            # Unknown message types are silently dropped — forward protocol compat.

        await context.close()
        await browser.close()


if __name__ == "__main__":
    asyncio.run(run(sys.stdin.buffer, sys.stdout.buffer))
