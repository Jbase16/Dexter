# Phase 14 — Browser Automation Worker
## Spec version 1.0 — Session 015, 2026-03-10

> **Status:** Current phase.
> This document is the authoritative implementation guide for Phase 14.
> All architectural decisions are locked. Implement exactly as written.

---

## 1. What Phase 14 Delivers

Browser automation capability wired end-to-end through the existing action gate:

```
Model generates <dexter:action>{"type":"browser","action":"navigate","url":"..."}
    │
    ▼ orchestrator extract_action_block()
ActionEngine::submit(ActionSpec::Browser { ... })
    │
    ├─ Extract / Screenshot  → SAFE   → execute immediately
    ├─ Navigate / Click / Type → CAUTIOUS → execute immediately + audit
    └─ category_override: "destructive" → DESTRUCTIVE → operator approval required
    │
    ▼ executor::execute_browser(&engine.browser, action, timeout)
BrowserCoordinator
    │  long-lived Playwright Chromium process
    ├─ write_frame(MSG_BROWSER_NAVIGATE, JSON payload)
    ▼ read_frame(MSG_BROWSER_RESULT)   (timeout: BROWSER_WORKER_RESULT_TIMEOUT_SECS)
ExecutionResult { success, output, error }
    │
    ▼ audit log entry (CAUTIOUS/DESTRUCTIVE only)
ActionOutcome::Completed { output } → injected back into context
```

New capabilities the model gains after Phase 14:
- **Navigate** to a URL
- **Click** an element by CSS selector
- **Type** text into an element
- **Extract** page content (full page or CSS-scoped)
- **Screenshot** (save to disk, return path)

---

## 2. What Already Exists (Do Not Rebuild)

| Component | Phase | Status |
|-----------|-------|--------|
| `WorkerClient` — spawn/health/frame-I/O/shutdown | 10 | ✅ reuse with new `WorkerType::Browser` |
| `VoiceCoordinator` — long-lived worker lifecycle pattern | 10 | ✅ pattern to follow |
| Binary frame protocol (`protocol.py` + `voice/protocol.rs`) | 10 | ✅ extend with browser msg types |
| `ActionEngine::submit/resolve/execute_and_log` | 8 | ✅ add `ActionSpec::Browser` arm |
| `PolicyEngine::classify` | 8 | ✅ add `Browser` arm |
| `executor.rs` standalone async functions | 8 | ✅ add `execute_browser()` |
| SAFE/CAUTIOUS/DESTRUCTIVE gate + audit log | 8 | ✅ no changes |
| `personality/default.yaml` action block format | 8 | ✅ add browser action examples |
| `uv` venv + `pyproject.toml` + `make setup-python` | 10 | ✅ add `playwright` dep |
| `src/python-workers/workers/browser_worker.py` | — | 🔲 stub exists, implement it |

---

## 3. Architectural Decisions

### 3.1 Why `BrowserCoordinator` lives inside `ActionEngine` (not `CoreOrchestrator`)

Browser is an **action capability**, not a session resource. `VoiceCoordinator` is a
session resource — it lives in `CoreOrchestrator` because TTS binds to the session stream's
lifetime. Browser is orthogonal to the session: it executes atomically within the action gate.

Locating `BrowserCoordinator` inside `ActionEngine` keeps the action subsystem self-contained.
`CoreOrchestrator` does not need a new field — it exposes `start_browser()` and
`browser_health_check()` as thin delegation methods that call `self.action_engine.start_browser()`
and `self.action_engine.browser_health_check()`, matching the `voice_health_check()` pattern.

The alternative (ActionEngine holding an `Arc<BrowserCoordinator>` shared with CoreOrchestrator)
introduces shared ownership with no benefit — browser commands are sequential per-session
and do not require concurrent access from multiple owners.

### 3.2 Why long-lived process (not per-call like STT)

`WorkerClient::spawn` for STT creates a fresh process per utterance because STT is stateless:
each call is a new audio stream with its own model context. `browser_worker.py` is stateful:
Chromium startup takes ~1-2 seconds and session cookies / auth state accumulate across
multiple browser actions in a conversation. A long-lived process amortizes startup cost
and preserves page state within a session — matching the TTS coordinator pattern exactly.

### 3.3 Why JSON frame payloads for browser commands

Browser action parameters are structured but variable: URLs can be 2000 chars, selectors
can be arbitrarily complex, extracted text can be kilobytes. JSON in the payload of an
existing binary frame is the correct encoding — it reuses the proven framing without adding
a new binary encoding. Both sides already have `serde_json` / `json` available. The
frame header provides length-framing; the JSON payload carries the command arguments.

### 3.4 Why Chromium headless (not Firefox or WebKit)

Playwright supports all three. Chromium has the widest real-world site compatibility and
is Playwright's default. The operator's automation use cases (web scraping, form filling,
content extraction) are well-served by Chromium. Firefox and WebKit remain available as
fallbacks via a future config knob if compatibility issues arise.

### 3.5 Policy for browser actions

| Action | Category | Rationale |
|--------|----------|-----------|
| `Extract` | SAFE | Read-only, no state changes |
| `Screenshot` | SAFE | Read-only, saves to `/tmp/` |
| `Navigate` | CAUTIOUS | Navigates but doesn't inherently destroy data |
| `Click` | CAUTIOUS | Most clicks are benign; operator uses `category_override: "destructive"` for consequential clicks |
| `Type` | CAUTIOUS | Entering text is reversible; form submission requires an explicit Click |

The model uses `category_override: "destructive"` when it knows a click or navigation
will have irreversible consequences (e.g., clicking "Delete account" or "Confirm purchase").
The downgrade guard in `apply_override` ensures the model cannot lower classification below CAUTIOUS.

---

## 4. Protocol Extension

### 4.1 New message type constants

Add to **both** `src/python-workers/workers/protocol.py` AND `src/rust-core/src/voice/protocol.rs`.
These extend the existing 10 constants (0x01–0x0A) without renumbering them.

```python
# protocol.py additions (after MSG_ERROR = 0x0A):
MSG_BROWSER_NAVIGATE   = 0x0B  # payload: JSON {"url": "..."}
MSG_BROWSER_CLICK      = 0x0C  # payload: JSON {"selector": "...", "timeout_ms": N}
MSG_BROWSER_TYPE       = 0x0D  # payload: JSON {"selector": "...", "text": "..."}
MSG_BROWSER_EXTRACT    = 0x0E  # payload: JSON {"selector": null}  (null = full page)
MSG_BROWSER_SCREENSHOT = 0x0F  # payload: empty — worker saves to /tmp, returns path
MSG_BROWSER_RESULT     = 0x10  # payload: JSON {"success": bool, "output": "...", "error": "..."}
```

```rust
// voice/protocol.rs msg submodule additions:
pub const BROWSER_NAVIGATE:   u8 = 0x0B;
pub const BROWSER_CLICK:      u8 = 0x0C;
pub const BROWSER_TYPE:       u8 = 0x0D;
pub const BROWSER_EXTRACT:    u8 = 0x0E;
pub const BROWSER_SCREENSHOT: u8 = 0x0F;
pub const BROWSER_RESULT:     u8 = 0x10;
```

### 4.2 `WorkerType::Browser` extension

In `voice/protocol.rs`, extend `WorkerType` and `parse_handshake`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerType { Stt, Tts, Browser }

impl WorkerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkerType::Stt     => "stt",
            WorkerType::Tts     => "tts",
            WorkerType::Browser => "browser",
        }
    }
}
```

In `parse_handshake`, add:
```rust
"browser" => WorkerType::Browser,
```

Add one new test to `voice/protocol.rs`:
```rust
#[test]
fn parse_handshake_browser_valid() {
    let line = r#"{"protocol_version":1,"worker_type":"browser"}"#;
    let hs = parse_handshake(line).unwrap();
    assert_eq!(hs.worker_type, WorkerType::Browser);
}
```

---

## 5. New Files to Create

### 5.1 `src/python-workers/workers/browser_worker.py`

**Purpose:** Long-lived Playwright Chromium process. Receives JSON-framed browser
commands, executes them against a single `Page` instance, returns JSON results.

**Why one `Page` per session (not one per command):**
Preserves cookies, auth state, and navigation history within a conversation session.
The operator's browser actions in a conversation are semantically sequential — they
accumulate state. A new page per command would lose login sessions, discard local
storage, and generally be useless for real workflows.

**Architecture:**

```python
"""
Dexter browser automation worker — Playwright Chromium subprocess.

Long-lived: one browser process + one Page per session. Cookies, auth state,
and page history persist across commands within the session. The process exits
when it receives MSG_SHUTDOWN or when stdin closes (Rust core exited).

Threading: single-threaded asyncio event loop. All I/O is async.
"""
import asyncio
import json
import sys
from pathlib import Path

from playwright.async_api import async_playwright, Page

from workers.protocol import (
    MSG_HEALTH_PING, MSG_HEALTH_PONG, MSG_SHUTDOWN,
    MSG_BROWSER_NAVIGATE, MSG_BROWSER_CLICK, MSG_BROWSER_TYPE,
    MSG_BROWSER_EXTRACT, MSG_BROWSER_SCREENSHOT, MSG_BROWSER_RESULT,
    send_frame, read_frame, write_handshake,
)

# SCREENSHOT_DIR: /tmp/dexter-screenshots/ — created on first screenshot.
SCREENSHOT_DIR = Path("/tmp/dexter-screenshots")
```

**Command dispatch table** (all handlers async, called from the main loop):

```python
async def handle_navigate(page: Page, payload: bytes) -> tuple[bool, str, str]:
    cmd = json.loads(payload)
    url = cmd["url"]
    timeout_ms = cmd.get("timeout_ms", 30_000)
    try:
        await page.goto(url, timeout=timeout_ms)
        return True, page.url, ""
    except Exception as e:
        return False, "", str(e)

async def handle_click(page: Page, payload: bytes) -> tuple[bool, str, str]:
    cmd = json.loads(payload)
    selector = cmd["selector"]
    timeout_ms = cmd.get("timeout_ms", 10_000)
    try:
        await page.click(selector, timeout=timeout_ms)
        return True, f"clicked: {selector}", ""
    except Exception as e:
        return False, "", str(e)

async def handle_type(page: Page, payload: bytes) -> tuple[bool, str, str]:
    cmd = json.loads(payload)
    selector, text = cmd["selector"], cmd["text"]
    timeout_ms = cmd.get("timeout_ms", 10_000)
    try:
        await page.fill(selector, text, timeout=timeout_ms)
        return True, f"typed into: {selector}", ""
    except Exception as e:
        return False, "", str(e)

async def handle_extract(page: Page, payload: bytes) -> tuple[bool, str, str]:
    cmd = json.loads(payload)
    selector = cmd.get("selector")   # None = full page
    try:
        if selector:
            el = await page.query_selector(selector)
            text = (await el.inner_text()) if el else ""
        else:
            text = await page.inner_text("body")
        return True, text[:10_000], ""   # 10k char cap — prevent runaway payload
    except Exception as e:
        return False, "", str(e)

async def handle_screenshot(page: Page, _payload: bytes) -> tuple[bool, str, str]:
    try:
        SCREENSHOT_DIR.mkdir(parents=True, exist_ok=True)
        # Name uniquely by page URL hash + timestamp to avoid clobber.
        import time
        ts = int(time.time() * 1000)
        path = SCREENSHOT_DIR / f"screenshot_{ts}.png"
        await page.screenshot(path=str(path))
        return True, str(path), ""
    except Exception as e:
        return False, "", str(e)
```

**Main event loop:**

```python
async def run(stdin, stdout):
    write_handshake(stdout, "browser")

    async with async_playwright() as pw:
        browser = await pw.chromium.launch(headless=True)
        page = await browser.new_page()

        DISPATCH = {
            MSG_BROWSER_NAVIGATE:   handle_navigate,
            MSG_BROWSER_CLICK:      handle_click,
            MSG_BROWSER_TYPE:       handle_type,
            MSG_BROWSER_EXTRACT:    handle_extract,
            MSG_BROWSER_SCREENSHOT: handle_screenshot,
        }

        while True:
            msg_type, payload = await asyncio.get_event_loop().run_in_executor(
                None, read_frame, stdin
            )
            if msg_type is None:
                break

            if msg_type == MSG_SHUTDOWN:
                break
            elif msg_type == MSG_HEALTH_PING:
                send_frame(stdout, MSG_HEALTH_PONG, b"")
            elif msg_type in DISPATCH:
                success, output, error = await DISPATCH[msg_type](page, payload)
                result = json.dumps({"success": success, "output": output, "error": error})
                send_frame(stdout, MSG_BROWSER_RESULT, result.encode())
            # Unknown message types are silently dropped — protocol forward-compat.

        await browser.close()


if __name__ == "__main__":
    asyncio.run(run(sys.stdin.buffer, sys.stdout.buffer))
```

**Why `run_in_executor` for `read_frame`:**
`read_frame` is a blocking call (`f.read()` on stdin). Playwright's async event loop would
stall if `read_frame` is called directly on the event thread — the `asyncio.sleep` inside
page operations would never yield. `run_in_executor(None, read_frame, stdin)` offloads the
blocking read to the default thread pool executor, allowing the asyncio event loop to service
Playwright's internal async tasks while waiting for the next command. This is the standard
pattern for integrating blocking I/O into asyncio without subprocess communication.

---

### 5.2 `src/python-workers/tests/test_browser_worker.py`

**Purpose:** Unit tests using `unittest.mock.patch` for Playwright — same pattern as
`test_tts_worker.py` mocking `KPipeline`.

**Four tests:**

```python
"""
Tests for browser_worker.py — mocks Playwright to avoid requiring a Chromium binary.
"""
import io
import json
import unittest
from unittest.mock import AsyncMock, MagicMock, patch

import pytest


class TestBrowserWorkerHandshake:
    def test_handshake_json_is_valid(self):
        """write_handshake emits {"protocol_version":1,"worker_type":"browser"}."""
        from workers.protocol import PROTOCOL_VERSION
        out = io.BytesIO()

        class FakeStdout:
            def write(self, data): out.write(data)
            def flush(self): pass

        from workers.protocol import write_handshake
        write_handshake(FakeStdout(), "browser")
        line = out.getvalue().decode().strip()
        parsed = json.loads(line)
        assert parsed["protocol_version"] == PROTOCOL_VERSION
        assert parsed["worker_type"] == "browser"


class TestBrowserWorkerHandlers:
    @pytest.mark.asyncio
    async def test_handle_navigate_success(self):
        """handle_navigate returns (True, final_url, "") on success."""
        from workers.browser_worker import handle_navigate

        page = AsyncMock()
        page.url = "https://example.com/"

        payload = json.dumps({"url": "https://example.com/"}).encode()
        success, output, error = await handle_navigate(page, payload)

        assert success is True
        assert output == "https://example.com/"
        assert error == ""
        page.goto.assert_awaited_once_with("https://example.com/", timeout=30_000)

    @pytest.mark.asyncio
    async def test_handle_extract_full_page(self):
        """handle_extract with selector=null returns page body text (capped at 10k)."""
        from workers.browser_worker import handle_extract

        page = AsyncMock()
        page.inner_text = AsyncMock(return_value="Hello World")

        payload = json.dumps({"selector": None}).encode()
        success, output, error = await handle_extract(page, payload)

        assert success is True
        assert output == "Hello World"
        page.inner_text.assert_awaited_once_with("body")

    @pytest.mark.asyncio
    async def test_handle_navigate_failure(self):
        """handle_navigate returns (False, '', error_message) on Playwright exception."""
        from workers.browser_worker import handle_navigate

        page = AsyncMock()
        page.goto.side_effect = Exception("net::ERR_NAME_NOT_RESOLVED")

        payload = json.dumps({"url": "https://doesnotexist.invalid/"}).encode()
        success, output, error = await handle_navigate(page, payload)

        assert success is False
        assert output == ""
        assert "ERR_NAME_NOT_RESOLVED" in error

    @pytest.mark.asyncio
    async def test_handle_screenshot_creates_file_path(self):
        """handle_screenshot returns the screenshot path on success."""
        from workers.browser_worker import handle_screenshot

        page = AsyncMock()
        page.screenshot = AsyncMock()

        success, output, error = await handle_screenshot(page, b"")

        assert success is True
        assert output.startswith("/tmp/dexter-screenshots/")
        assert output.endswith(".png")
        assert error == ""
```

Note: `pytest-asyncio` is required. Add to `pyproject.toml`:
```toml
[tool.pytest.ini_options]
asyncio_mode = "auto"
```

---

### 5.3 `src/rust-core/src/browser/coordinator.rs`

**Purpose:** Manages the long-lived `browser_worker.py` subprocess. Mirrors
`voice/coordinator.rs` for lifecycle management (start/health/restart/shutdown).

**Architecture:**

```rust
/// BrowserCoordinator — lifecycle manager for the Playwright browser worker.
///
/// Mirrors VoiceCoordinator: long-lived process, Arc<Mutex> client slot,
/// AtomicBool availability flag, restart policy with backoff.
///
/// Threading invariant: all methods take &self (or &mut self for mutating
/// restart_count). The tokio::sync::Mutex<Option<WorkerClient>> provides
/// interior mutability for async I/O across await points — required because
/// WorkerClient's frame I/O borrows stdin/stdout across await.
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use tracing::{error, info, warn};

use crate::{
    constants::{
        BROWSER_WORKER_PATH, BROWSER_WORKER_RESULT_TIMEOUT_SECS,
        VOICE_PYTHON_EXE, VOICE_WORKER_RESTART_BACKOFF_SECS, VOICE_WORKER_RESTART_MAX_ATTEMPTS,
    },
    voice::{
        protocol::{msg, WorkerType},
        worker_client::WorkerClient,
    },
};

pub struct BrowserCoordinator {
    // Same Arc<Mutex> pattern as VoiceCoordinator: allows &self usage from
    // execute_and_log (which borrows &ActionEngine) without &mut constraints.
    client:        Arc<tokio::sync::Mutex<Option<WorkerClient>>>,
    is_available:  Arc<AtomicBool>,
    restart_count: Arc<AtomicU32>,
}
```

**Key methods:**

```rust
impl BrowserCoordinator {
    /// Create in degraded mode — worker slot is empty, is_available=false.
    /// Always succeeds. Caller must call start() to spawn the actual process.
    pub fn new_degraded() -> Self {
        Self {
            client:        Arc::new(tokio::sync::Mutex::new(None)),
            is_available:  Arc::new(AtomicBool::new(false)),
            restart_count: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Spawn browser_worker.py. Sets is_available=true on success.
    /// Called once from ActionEngine::start_browser().
    pub async fn start(&self) {
        match WorkerClient::spawn(WorkerType::Browser, VOICE_PYTHON_EXE, BROWSER_WORKER_PATH).await {
            Ok(client) => {
                *self.client.lock().await = Some(client);
                self.is_available.store(true, Ordering::Relaxed);
                self.restart_count.store(0, Ordering::Relaxed);
                info!("Browser worker started");
            }
            Err(e) => {
                error!(error = %e, "Browser worker failed to start — browser actions degraded");
            }
        }
    }

    pub fn is_available(&self) -> bool {
        self.is_available.load(Ordering::Relaxed)
    }

    /// Send a browser command frame and await MSG_BROWSER_RESULT within timeout.
    ///
    /// Returns the JSON payload of the BROWSER_RESULT frame as a String.
    /// Returns Err if the worker is unavailable, the frame write fails, or timeout fires.
    ///
    /// Holds the tokio::sync::Mutex across all await points for this call —
    /// this is intentional and safe: browser commands are sequential per-session.
    pub async fn execute(
        &self,
        msg_type: u8,
        payload:  &[u8],
    ) -> Result<String, crate::voice::worker_client::WorkerError> {
        if !self.is_available.load(Ordering::Relaxed) {
            return Err(crate::voice::worker_client::WorkerError::Io(
                std::io::Error::new(std::io::ErrorKind::NotConnected, "browser worker unavailable")
            ));
        }

        let mut guard = self.client.lock().await;
        let client = guard.as_mut().ok_or_else(|| {
            crate::voice::worker_client::WorkerError::Io(
                std::io::Error::new(std::io::ErrorKind::NotConnected, "browser worker slot is None")
            )
        })?;

        client.write_frame(msg_type, payload).await
            .map_err(crate::voice::worker_client::WorkerError::Io)?;

        tokio::time::timeout(
            std::time::Duration::from_secs(BROWSER_WORKER_RESULT_TIMEOUT_SECS),
            async {
                loop {
                    match client.read_frame().await {
                        Ok(Some((t, data))) if t == msg::BROWSER_RESULT => {
                            return String::from_utf8(data).map_err(|_| {
                                crate::voice::worker_client::WorkerError::Io(
                                    std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF8 result")
                                )
                            });
                        }
                        Ok(Some(_)) => continue, // discard non-result frames
                        Ok(None) | Err(_) => return Err(
                            crate::voice::worker_client::WorkerError::Io(
                                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "worker closed")
                            )
                        ),
                    }
                }
            }
        ).await.map_err(|_| crate::voice::worker_client::WorkerError::HandshakeTimeout)?
    }

    /// Send HEALTH_PING; restart if no HEALTH_PONG. Respects restart_count limit.
    ///
    /// Called periodically from CoreOrchestrator via ActionEngine::browser_health_check().
    pub async fn health_check_and_restart(&self) {
        let healthy = {
            let mut guard = self.client.lock().await;
            match guard.as_mut() {
                None => false,
                Some(client) => client.health_check().await,
            }
        };
        if healthy { return; }

        let count = self.restart_count.fetch_add(1, Ordering::Relaxed);
        if count >= VOICE_WORKER_RESTART_MAX_ATTEMPTS {
            if count == VOICE_WORKER_RESTART_MAX_ATTEMPTS {
                // Log only once — avoid log spam on every tick after max restarts.
                error!("Browser worker reached max restart attempts — browser actions permanently degraded");
            }
            return;
        }

        warn!(restart_count = count + 1, "Browser worker unhealthy — restarting");
        self.is_available.store(false, Ordering::Relaxed);
        *self.client.lock().await = None;

        let backoff = VOICE_WORKER_RESTART_BACKOFF_SECS << count; // doubles each attempt
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;

        self.start().await;
    }

    /// Send SHUTDOWN frame and wait up to 3s for process exit.
    pub async fn shutdown(&mut self) {
        self.is_available.store(false, Ordering::Relaxed);
        let client = self.client.lock().await.take();
        if let Some(c) = client {
            c.shutdown().await;
            info!("Browser worker shut down");
        }
    }
}
```

**Unit tests** (4 tests, all non-subprocess):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_degraded_is_not_available() {
        let c = BrowserCoordinator::new_degraded();
        assert!(!c.is_available());
    }

    #[test]
    fn new_degraded_restart_count_is_zero() {
        let c = BrowserCoordinator::new_degraded();
        assert_eq!(c.restart_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn browser_arc_clone_points_to_same_allocation() {
        let c = BrowserCoordinator::new_degraded();
        let a = Arc::clone(&c.is_available);
        a.store(true, Ordering::Relaxed);
        assert!(c.is_available());
    }

    #[tokio::test]
    async fn execute_returns_err_when_unavailable() {
        let c = BrowserCoordinator::new_degraded();
        // Worker slot is None and is_available=false — must return Err without panic.
        let result = c.execute(msg::BROWSER_NAVIGATE, b"{}").await;
        assert!(result.is_err());
    }
}
```

---

### 5.4 `src/rust-core/src/browser/mod.rs`

```rust
//! Browser automation worker coordination.
//!
//! Provides `BrowserCoordinator` — a long-lived Playwright Chromium subprocess
//! manager following the same lifecycle pattern as `voice::VoiceCoordinator`.
pub mod coordinator;
pub use coordinator::BrowserCoordinator;
```

---

## 6. Files to Modify

### 6.1 `src/rust-core/src/constants.rs`

Add at the end (new Phase 14 section):

```rust
// ── Browser Worker (Phase 14) ─────────────────────────────────────────────────

/// Path to the browser worker script, relative to daemon working directory (project root).
pub const BROWSER_WORKER_PATH: &str = "src/python-workers/workers/browser_worker.py";

/// Timeout for a single browser command (navigate/click/type/extract/screenshot) in seconds.
///
/// Navigation can be slow on large pages. 30s matches ACTION_DEFAULT_TIMEOUT_SECS.
pub const BROWSER_WORKER_RESULT_TIMEOUT_SECS: u64 = 30;

/// How often Rust health-checks the browser worker (seconds).
///
/// Browser worker is idle most of the time — a 60s check interval is sufficient
/// to detect crashes without generating unnecessary subprocess traffic.
pub const BROWSER_WORKER_HEALTH_INTERVAL_SECS: u64 = 60;
```

---

### 6.2 `src/rust-core/src/action/engine.rs`

**Three changes:**

**A. Add `BrowserActionKind` and extend `ActionSpec`:**

```rust
// Add as a top-level type in engine.rs (before ActionSpec):

/// Browser sub-action, embedded as a nested enum inside ActionSpec::Browser.
///
/// Serialized from model output using internally-tagged serde (action field):
/// {"type":"browser","action":"navigate","url":"https://...","rationale":"..."}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrowserActionKind {
    Navigate   { url: String },
    Click      { selector: String },
    Type       { selector: String, text: String },
    Extract    { selector: Option<String> },   // None = full page body
    Screenshot,
}

// In ActionSpec enum, add:
ActionSpec::Browser {
    // #[serde(flatten)] is required because BrowserActionKind uses
    // #[serde(tag = "action")], so its discriminant ("action") and variant
    // fields ("url", "selector", etc.) must appear at the SAME JSON level as
    // ActionSpec's own "type" tag.
    //
    // Without flatten, serde would expect a nested object:
    //   {"type":"browser","action":{"action":"navigate","url":"..."}}
    //
    // With flatten, serde correctly parses the model's flat output:
    //   {"type":"browser","action":"navigate","url":"..."}
    #[serde(flatten)]
    action:            BrowserActionKind,
    rationale:         Option<String>,
    #[serde(default)]
    category_override: Option<String>,
},
```

**B. Add `browser: BrowserCoordinator` field to `ActionEngine`:**

```rust
use crate::browser::BrowserCoordinator;

pub struct ActionEngine {
    state_dir:       PathBuf,
    audit:           AuditLog,
    pending_actions: HashMap<String, PendingAction>,
    browser:         BrowserCoordinator,  // ADD
}

impl ActionEngine {
    pub fn new(state_dir: &std::path::Path) -> Self {
        Self {
            state_dir:       state_dir.to_path_buf(),
            audit:           AuditLog::new(state_dir),
            pending_actions: HashMap::new(),
            browser:         BrowserCoordinator::new_degraded(),  // ADD
        }
    }

    /// Spawn browser_worker.py. Called once from CoreOrchestrator after construction.
    pub async fn start_browser(&self) {
        self.browser.start().await;
    }

    /// Health-check + conditional restart. Called from CoreOrchestrator health-check timer.
    pub async fn browser_health_check(&self) {
        self.browser.health_check_and_restart().await;
    }

    /// Shutdown browser worker. Called from CoreOrchestrator::shutdown.
    ///
    /// `browser` is a private field — callers outside the `action` module must go
    /// through this method. Direct field access (`self.action_engine.browser`) from
    /// orchestrator.rs would be a visibility error at compile time.
    pub async fn shutdown_browser(&mut self) {
        self.browser.shutdown().await;
    }
}
```

**C. Add `ActionSpec::Browser` arm to `execute_and_log`:**

```rust
ActionSpec::Browser { action, .. } => {
    executor::execute_browser(&self.browser, action, BROWSER_WORKER_RESULT_TIMEOUT_SECS).await
}
```

Also update `describe()`, `type_str()`, and `spec_to_audit_json()`:

```rust
// describe():
ActionSpec::Browser { action, .. } => match action {
    BrowserActionKind::Navigate { url }       => format!("Browser navigate: {url}"),
    BrowserActionKind::Click { selector }     => format!("Browser click: {selector}"),
    BrowserActionKind::Type { selector, .. }  => format!("Browser type into: {selector}"),
    BrowserActionKind::Extract { selector }   => format!("Browser extract: {}", selector.as_deref().unwrap_or("<page>")),
    BrowserActionKind::Screenshot             => "Browser screenshot".to_string(),
},

// type_str():
ActionSpec::Browser { .. } => "browser",

// spec_to_audit_json():
ActionSpec::Browser { action, rationale, .. } => {
    let action_str = match action {
        BrowserActionKind::Navigate { url }    => serde_json::json!({"action":"navigate","url":url}),
        BrowserActionKind::Click { selector }  => serde_json::json!({"action":"click","selector":selector}),
        BrowserActionKind::Type { selector, .. } => serde_json::json!({"action":"type","selector":selector,"text":"<omitted>"}),
        BrowserActionKind::Extract { selector } => serde_json::json!({"action":"extract","selector":selector}),
        BrowserActionKind::Screenshot          => serde_json::json!({"action":"screenshot"}),
    };
    serde_json::json!({"browser": action_str, "rationale": rationale})
},
```

Note on `Type` audit: the typed text is omitted from the audit log — it may be a password
or sensitive credential. The selector is recorded (intent), not the content.

---

### 6.3 `src/rust-core/src/action/policy.rs`

Add `ActionSpec::Browser` arm to `classify()`:

```rust
ActionSpec::Browser { action, category_override, .. } => {
    let base = Self::classify_browser(action);
    Self::apply_override(base, category_override.as_deref())
}
```

Add `classify_browser` helper:

```rust
fn classify_browser(action: &BrowserActionKind) -> ActionCategory {
    match action {
        // Read-only operations — no observable side effects.
        BrowserActionKind::Extract { .. }  => ActionCategory::Safe,
        BrowserActionKind::Screenshot      => ActionCategory::Safe,
        // State-changing but reversible — model uses category_override for
        // consequential clicks (delete, confirm purchase, submit irreversible form).
        BrowserActionKind::Navigate { .. } => ActionCategory::Cautious,
        BrowserActionKind::Click { .. }    => ActionCategory::Cautious,
        BrowserActionKind::Type { .. }     => ActionCategory::Cautious,
    }
}
```

Add 4 new unit tests in `policy.rs` tests block:

```rust
fn browser_navigate() -> ActionSpec {
    ActionSpec::Browser {
        action: BrowserActionKind::Navigate { url: "https://example.com".to_string() },
        rationale: None, category_override: None,
    }
}
fn browser_extract() -> ActionSpec {
    ActionSpec::Browser {
        action: BrowserActionKind::Extract { selector: None },
        rationale: None, category_override: None,
    }
}
fn browser_click() -> ActionSpec {
    ActionSpec::Browser {
        action: BrowserActionKind::Click { selector: "button.submit".to_string() },
        rationale: None, category_override: None,
    }
}
fn browser_click_destructive() -> ActionSpec {
    ActionSpec::Browser {
        action: BrowserActionKind::Click { selector: "#delete-account".to_string() },
        rationale: None,
        category_override: Some("destructive".to_string()),
    }
}

#[test]
fn classify_browser_extract_is_safe() {
    assert_eq!(PolicyEngine::classify(&browser_extract()), ActionCategory::Safe);
}
#[test]
fn classify_browser_navigate_is_cautious() {
    assert_eq!(PolicyEngine::classify(&browser_navigate()), ActionCategory::Cautious);
}
#[test]
fn classify_browser_click_is_cautious() {
    assert_eq!(PolicyEngine::classify(&browser_click()), ActionCategory::Cautious);
}
#[test]
fn classify_browser_click_with_destructive_override() {
    assert_eq!(PolicyEngine::classify(&browser_click_destructive()), ActionCategory::Destructive);
}
```

---

### 6.4 `src/rust-core/src/action/executor.rs`

Add `execute_browser()` (standalone async function, matching the pattern of other executor functions):

```rust
use crate::{
    browser::{coordinator::BrowserCoordinator, BrowserActionKind},
    voice::protocol::msg,
};

/// Execute a browser action via the long-lived BrowserCoordinator.
///
/// Translates `BrowserActionKind` → msg_type + JSON payload, calls
/// `coordinator.execute()`, and parses the JSON result into ExecutionResult.
///
/// Returns a failed ExecutionResult if:
/// - The coordinator is unavailable (worker not started or permanently crashed)
/// - The command times out (BROWSER_WORKER_RESULT_TIMEOUT_SECS)
/// - The worker returns {"success": false, "error": "..."}
pub async fn execute_browser(
    coordinator: &BrowserCoordinator,
    action:      &BrowserActionKind,
    timeout_secs: u64,
) -> ExecutionResult {
    let start = std::time::Instant::now();

    let (msg_type, payload) = build_browser_frame(action);
    let result = coordinator.execute(msg_type, &payload).await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Err(e) => ExecutionResult {
            success:     false,
            output:      String::new(),
            error:       format!("Browser worker error: {e}"),
            exit_code:   None,
            duration_ms,
        },
        Ok(json_str) => {
            match serde_json::from_str::<serde_json::Value>(&json_str) {
                Err(e) => ExecutionResult {
                    success:     false,
                    output:      String::new(),
                    error:       format!("Browser result parse error: {e}"),
                    exit_code:   None,
                    duration_ms,
                },
                Ok(val) => ExecutionResult {
                    success:     val["success"].as_bool().unwrap_or(false),
                    output:      val["output"].as_str().unwrap_or("").to_string(),
                    error:       val["error"].as_str().unwrap_or("").to_string(),
                    exit_code:   None,  // browser actions have no exit code
                    duration_ms,
                },
            }
        }
    }
}

/// Map a BrowserActionKind to (msg_type, JSON payload bytes).
fn build_browser_frame(action: &BrowserActionKind) -> (u8, Vec<u8>) {
    match action {
        BrowserActionKind::Navigate { url } => (
            msg::BROWSER_NAVIGATE,
            serde_json::json!({"url": url}).to_string().into_bytes(),
        ),
        BrowserActionKind::Click { selector } => (
            msg::BROWSER_CLICK,
            serde_json::json!({"selector": selector}).to_string().into_bytes(),
        ),
        BrowserActionKind::Type { selector, text } => (
            msg::BROWSER_TYPE,
            serde_json::json!({"selector": selector, "text": text}).to_string().into_bytes(),
        ),
        BrowserActionKind::Extract { selector } => (
            msg::BROWSER_EXTRACT,
            serde_json::json!({"selector": selector}).to_string().into_bytes(),
        ),
        BrowserActionKind::Screenshot => (
            msg::BROWSER_SCREENSHOT,
            vec![],
        ),
    }
}
```

---

### 6.5 `src/rust-core/src/orchestrator.rs`

**Three changes:**

**A. Remove `#[allow(dead_code)]` from `action_engine` field (it's now started during session init).**

**B. Add delegation methods:**

```rust
/// Start the browser worker subprocess. Called from server.rs after start_voice().
pub async fn start_browser(&self) {
    self.action_engine.start_browser().await;
}

/// Health-check the browser worker; restart if unhealthy.
/// Called from the server.rs health-check timer.
pub async fn browser_health_check(&self) {
    self.action_engine.browser_health_check().await;
}
```

**C. Add browser shutdown in `shutdown(mut self)`:**

```rust
// In CoreOrchestrator::shutdown(mut self):
// ADD before drain_pending_on_shutdown:
self.action_engine.shutdown_browser().await;  // delegate: browser field is private to ActionEngine
```

`browser` is a private field of `ActionEngine`. Accessing `self.action_engine.browser`
from `orchestrator.rs` would be a compile-time visibility error — Rust enforces module
privacy even when the caller has owned/mutable access. `ActionEngine::shutdown_browser()`
(specified in §6.2-B) provides the correct delegation through the module boundary.

---

### 6.6 `src/rust-core/src/ipc/server.rs`

**Two changes:**

**A. Call `start_browser()` after `start_voice()`:**

```rust
orchestrator.start_voice().await;
orchestrator.start_browser().await;   // ADD
```

**B. Add browser health-check to the `tokio::select!` loop:**

```rust
// Add a second interval ticker alongside health_interval:
let mut browser_health_interval = tokio::time::interval(
    std::time::Duration::from_secs(BROWSER_WORKER_HEALTH_INTERVAL_SECS)
);
browser_health_interval.tick().await;  // skip t=0 tick

// In the select! loop:
_ = browser_health_interval.tick() => {
    orchestrator.browser_health_check().await;
}
```

Add `BROWSER_WORKER_HEALTH_INTERVAL_SECS` to the constants import.

---

### 6.7 `src/rust-core/src/main.rs`

Add `mod browser;` after existing module declarations:

```rust
mod browser;
```

---

### 6.8 `src/python-workers/pyproject.toml`

Three additive edits — **do not duplicate any existing TOML section headers**.

**1. Add `playwright` to the existing `dependencies` list:**

```toml
dependencies = [
    "faster-whisper>=1.1.0",
    "kokoro>=0.9.0",
    "numpy>=1.26",
    "scipy>=1.13",
    "playwright>=1.40",          # ADD
]
```

**2. Add `asyncio_mode` to the EXISTING `[tool.pytest.ini_options]` section.**

The section already exists from Phase 10 with `testpaths = ["tests"]`. Add one line to it:

```toml
[tool.pytest.ini_options]     # ← already exists, do NOT add this header again
testpaths = ["tests"]
asyncio_mode = "auto"         # ADD — required for async def test functions
```

Duplicating the `[tool.pytest.ini_options]` header is a TOML parse error; uv/pytest
will reject the file entirely.

**3. Add dev dependency section (new section — does not already exist):**

```toml
[tool.uv.dev-dependencies]
pytest-asyncio = ">=0.23"     # ADD
```

---

### 6.9 `Makefile`

Update `setup-python` to install Playwright and download Chromium:

```makefile
setup-python:  ## Install Python worker dependencies (kokoro, faster-whisper, playwright)
	cd $(PYTHON_DIR) && uv sync
	cd $(PYTHON_DIR) && uv run playwright install chromium
```

`playwright install chromium` downloads the Chromium binary (~150MB) to Playwright's
local cache (`~/.cache/ms-playwright`). It is idempotent — safe to run on every
`make setup-python` invocation.

---

### 6.10 `config/personality/default.yaml`

Add browser action examples to the `system_prompt_prefix` action block documentation
section (after existing applescript example):

```yaml
# Browser actions — navigate, click, type, extract, screenshot.
# Use category_override: "destructive" for consequential clicks.
# EXTRACT is read-only (SAFE). SCREENSHOT saves to /tmp/dexter-screenshots/.
# Example — navigate and extract:
# <dexter:action>
# {"type": "browser", "action": "navigate", "url": "https://example.com",
#  "rationale": "User asked to open example.com"}
# </dexter:action>
# (model then issues extract to read the page content)
# <dexter:action>
# {"type": "browser", "action": "extract", "selector": null,
#  "rationale": "Read the page content after navigation"}
# </dexter:action>
```

---

## 7. Implementation Order

Phase 14 must be implemented strictly in this order. Each step must build/test clean
before the next begins.

### Step 1: Protocol extension (Rust + Python, no new behavior yet)

Extend `voice/protocol.rs`:
- Add 6 `MSG_BROWSER_*` constants to the `msg` submodule
- Add `WorkerType::Browser` variant + `"browser"` arm in `parse_handshake`
- Add `parse_handshake_browser_valid` test

Extend `protocol.py`:
- Add 6 `MSG_BROWSER_*` constants

Verify:
```bash
cargo test   # must still show 159 tests pass, +1 new = 160
```

### Step 2: `browser/` Rust module + constants

Create `src/rust-core/src/browser/mod.rs` and `coordinator.rs` (full implementation as spec'd).
Add to `constants.rs`: `BROWSER_WORKER_PATH`, `BROWSER_WORKER_RESULT_TIMEOUT_SECS`, `BROWSER_WORKER_HEALTH_INTERVAL_SECS`.
Add `mod browser;` to `main.rs`.

Verify:
```bash
cargo test   # 160 → 164 tests (4 new BrowserCoordinator unit tests)
```

### Step 3: `ActionSpec::Browser` + `PolicyEngine` + `executor.rs`

Extend `action/engine.rs`: `BrowserActionKind` enum, `ActionSpec::Browser` variant,
`browser: BrowserCoordinator` field, `start_browser()`, `browser_health_check()`,
`Browser` arm in `execute_and_log` / `describe` / `type_str` / `spec_to_audit_json`.

Extend `action/policy.rs`: `classify_browser()` + `Browser` arm + 4 new tests.

Extend `action/executor.rs`: `execute_browser()` + `build_browser_frame()`.

Verify:
```bash
cargo test   # 164 → 168 tests (4 new policy tests)
cargo build  # 0 warnings
```

### Step 4: Orchestrator + server wiring

Add `start_browser()` + `browser_health_check()` to `orchestrator.rs`.
Add browser shutdown to `CoreOrchestrator::shutdown()`.
Update `ipc/server.rs`: call `start_browser()` after `start_voice()`; add browser health-check arm to `tokio::select!`.

Verify:
```bash
cargo test   # still 168 tests pass, 0 failures
cargo build  # 0 warnings
```

### Step 5: `pyproject.toml` + `Makefile` + `make setup-python`

Add `playwright>=1.40` + `pytest-asyncio>=0.23` to `pyproject.toml`.
Update `Makefile` `setup-python` target.

Run:
```bash
make setup-python
# Must complete without error.
# "playwright install chromium" downloads Chromium (~150MB first time, instant on re-run).
```

Manually test the handshake:
```bash
cd src/python-workers
uv run python workers/browser_worker.py
# Must print: {"protocol_version": 1, "worker_type": "browser"}
# Then wait for input. Ctrl-C to exit. No crash.
```

### Step 6: `browser_worker.py` (full implementation)

Implement `browser_worker.py` with all 5 command handlers.
Add `test_browser_worker.py` (4 tests, using `AsyncMock` for Playwright).

Verify:
```bash
cd src/python-workers && uv run pytest
# Must show 18 tests pass (14 existing + 4 new browser tests)
```

### Step 7: Full regression

```bash
cargo test        # must show ≥ 168 tests pass, 0 failures
swift build       # must show 0 errors, 0 project-code warnings (no Swift changes)
make setup-python # idempotent; verify Chromium installed
make run          # full system up
```

Manually test a browser action through the full pipeline (acceptance criteria below).

---

## 8. Acceptance Criteria

Phase 14 is complete when ALL of the following pass:

| ID | Criterion |
|----|-----------|
| AC-1 | `make setup-python` → `playwright install chromium` succeeds; no error |
| AC-2 | `uv run python workers/browser_worker.py` → prints handshake JSON, waits for input, no crash |
| AC-3 | `cargo test` ≥ 168 tests pass, 0 failures |
| AC-4 | `swift build` 0 errors, 0 project-code warnings |
| AC-5 | `PolicyEngine::classify(Browser::Extract)` = SAFE |
| AC-6 | `PolicyEngine::classify(Browser::Navigate)` = CAUTIOUS |
| AC-7 | `PolicyEngine::classify(Browser::Click with category_override:"destructive")` = DESTRUCTIVE |
| AC-8 | `BrowserCoordinator::new_degraded()` always succeeds; `is_available()` = false |
| AC-9 | Browser action submitted when worker unavailable → `ActionOutcome::Rejected`, no panic |
| AC-10 | Browser health-check timer fires in Rust log (BROWSER_WORKER_HEALTH_INTERVAL_SECS = 60s — check log after 60s) |
| AC-11 | Live: ask Dexter to navigate to a URL → browser worker receives command, returns page URL |
| AC-12 | Live: ask Dexter to extract page content → returns text from the current page |
| AC-13 | Python tests: `uv run pytest` → 18 tests pass (14 existing + 4 browser) |

---

## 9. Known Limitations (Deferred to Phase 15)

These are intentional Phase 14 constraints, not bugs:

1. **No browser context isolation per conversation:** The single `Page` shares cookies and
   auth state across all browser actions within one session (intended), but a new session
   doesn't reset Chromium state (unintended). Phase 15 can reset the page context on each
   `session()` RPC call: `await page.close(); page = await browser.new_page()`.

2. **No screenshot delivery to the model:** `Screenshot` saves to `/tmp/` and returns
   the path, but the path isn't fed through the vision model for description. Phase 15
   integrates this: after a screenshot, the model can issue a `FileRead` of the path and
   the VISION model interprets the image via Ollama multimodal endpoint.

3. **No JS dialog handling:** Playwright auto-dismisses alerts/confirms by default.
   Pages that open dialog boxes on navigation or click will succeed silently. Phase 15
   can add explicit dialog handler registration.

4. **No multi-tab support:** All browser actions operate on a single `Page`. Tab-switching
   or opening new pages requires protocol extension. Current `Click` actions that open a new
   tab will not switch context to the new tab — the worker remains on the original page.

5. **Chromium only:** Firefox and WebKit are not installed by `setup-python`. Add
   `playwright install firefox` if cross-browser compatibility is needed.

6. **`type` audit omits text:** The typed text is redacted in the audit log. This is
   intentional security policy (passwords, API keys). Full audit of typed content requires
   an explicit operator opt-in flag in the action spec, deferred.

---

## 10. Test Count Summary

| Source | Tests | Cumulative |
|--------|-------|------------|
| Phase 13 baseline (Rust unit tests) | 159 | 159 |
| Step 1: `WorkerType::Browser` handshake test | +1 | 160 |
| Step 2: `BrowserCoordinator` unit tests | +4 | 164 |
| Step 3: `PolicyEngine::classify(Browser::*)` | +4 | 168 |
| Python (existing) | 14 | — |
| Python (new browser tests) | +4 | 18 total |

---

## 11. Session State Update

When Phase 14 is complete, update `SESSION_STATE.json`:

```json
"current": "Phase 15",
"completed_phases": [
  "... (all prior phases)",
  "Phase 14 Browser Automation Worker — 168 RUST TESTS PASS, 18 PYTHON TESTS PASS, 0 WARNINGS"
],
```

Update `fresh_session_bootstrap_instructions`:
```
"Phases 1–14 are fully complete — Begin Phase 15 (Integration + Hardening) immediately.
Run 'cargo test' (≥ 168 tests expected), 'swift build' (0 warnings), and
'make setup-python' (playwright installed) before starting Phase 15 work."
```
