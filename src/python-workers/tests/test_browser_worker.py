"""
Tests for browser_worker.py — mocks Playwright to avoid requiring a running
Chromium binary. All Playwright calls are replaced with AsyncMock/MagicMock.
"""
import io
import json
from unittest.mock import AsyncMock, patch

import pytest


# ── Handshake tests ───────────────────────────────────────────────────────────

class TestBrowserWorkerHandshake:
    def test_handshake_json_is_valid(self):
        """write_handshake emits {"protocol_version":1,"worker_type":"browser"}."""
        from workers.protocol import PROTOCOL_VERSION, write_handshake

        out = io.BytesIO()

        class FakeStdout:
            def write(self, data: bytes) -> None:
                out.write(data)

            def flush(self) -> None:
                pass

        write_handshake(FakeStdout(), "browser")
        line = out.getvalue().decode().strip()
        parsed = json.loads(line)
        assert parsed["protocol_version"] == PROTOCOL_VERSION
        assert parsed["worker_type"] == "browser"


# ── Handler unit tests ────────────────────────────────────────────────────────

class TestBrowserWorkerHandlers:
    async def test_handle_navigate_success(self):
        """handle_navigate returns final URL plus page title on success."""
        from workers.browser_worker import handle_navigate

        page = AsyncMock()
        page.url = "https://example.com/"
        page.title = AsyncMock(return_value="Example Domain")

        payload = json.dumps({"url": "https://example.com/"}).encode()
        result = await handle_navigate(page, payload)

        assert result["success"] is True
        assert result["output"] == "https://example.com/\nPage title: Example Domain"
        assert result["error"] == ""
        assert result["page_url"] == "https://example.com/"
        assert result["page_title"] == "Example Domain"
        page.goto.assert_awaited_once_with("https://example.com/", timeout=30_000)
        assert page.title.await_count == 2

    async def test_handle_extract_full_page(self):
        """handle_extract with selector=None returns page body text (capped at 10k)."""
        from workers.browser_worker import handle_extract

        page = AsyncMock()
        page.url = "https://example.com/"
        page.title = AsyncMock(return_value="Example Domain")
        page.query_selector_all = AsyncMock(return_value=[])
        page.inner_text = AsyncMock(return_value="Hello World")

        payload = json.dumps({"selector": None}).encode()
        result = await handle_extract(page, payload)

        assert result["success"] is True
        assert result["output"] == "Hello World"
        assert result["error"] == ""
        assert result["page_url"] == "https://example.com/"
        page.query_selector_all.assert_awaited_once_with("a[href]")
        page.inner_text.assert_awaited_once_with("body")

    async def test_handle_navigate_failure(self):
        """handle_navigate returns (False, '', error_message) on Playwright exception."""
        from workers.browser_worker import handle_navigate

        page = AsyncMock()
        page.url = "about:blank"
        page.title = AsyncMock(return_value="")
        page.goto.side_effect = Exception("net::ERR_NAME_NOT_RESOLVED")

        payload = json.dumps({"url": "https://doesnotexist.invalid/"}).encode()
        result = await handle_navigate(page, payload)

        assert result["success"] is False
        assert result["output"] == ""
        assert "ERR_NAME_NOT_RESOLVED" in result["error"]
        assert result["error_kind"] == "navigation_failed"
        assert result["page_url"] == "about:blank"

    async def test_handle_screenshot_creates_file_path(self):
        """handle_screenshot returns the screenshot path on success."""
        from workers.browser_worker import handle_screenshot

        page = AsyncMock()
        page.url = "https://example.com/"
        page.title = AsyncMock(return_value="Example Domain")
        page.screenshot = AsyncMock()

        result = await handle_screenshot(page, b"")

        assert result["success"] is True
        # Phase 37 / B6: screenshots now save to operator's Desktop.
        from pathlib import Path
        expected_prefix = str(Path.home() / "Desktop" / "screenshot_")
        assert result["output"].startswith(expected_prefix)
        assert result["output"].endswith(".png")
        assert result["error"] == ""
        assert result["page_url"] == "https://example.com/"
        # screenshot() was called with the path we returned.
        page.screenshot.assert_awaited_once()
