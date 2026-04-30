# Phase 15 — Integration + Hardening
## Spec version 1.0 — Session 015, 2026-03-10

> **Status:** Current phase.
> This document is the authoritative implementation guide for Phase 15.
> All architectural decisions are locked. Implement exactly as written.

---

## 1. What Phase 15 Delivers

Phase 15 is not a feature phase — it is a hardening phase. The full Dexter pipeline
(context → inference → retrieval → action → voice → browser) is complete as of Phase 14.
Phase 15 firms up what's soft: memory visibility, worker lifecycle edge cases, OS
permissions guidance, and the `make smoke` / `make test-e2e` targets.

**Concrete deliverables:**

| Deliverable | Why Now |
|-------------|---------|
| `src/rust-core/src/system/memory.rs` — vm_stat headroom probe | HEAVY model needs memory visibility before load |
| Memory log in orchestrator HEAVY routing path | Surface memory state in structured logs |
| `VoiceCoordinator::is_permanently_degraded()` + log-spam fix | health_check re-fires warn! every 5s after max restarts — bug |
| `BrowserCoordinator::is_permanently_degraded()` | Mirrors VoiceCoordinator — exposes accessor the orchestrator needs |
| Orchestrator: emit TextResponse when worker permanently fails | UI currently silent on permanent worker death |
| `scripts/permissions.sh` + `make check-permissions` | No TCC permission checker exists; operators need guided setup |
| `make smoke` — fast sanity check (<30s, no Ollama) | No single target that verifies all three layers build+test |
| `make test-e2e` — supersedes `make test-inference` | `test-inference` was scoped to Phase 4; now covers all `#[ignore]` tests |
| `#[allow(dead_code)]` cleanup on now-consumed constants | Stale annotations mislead readers into thinking constants are unused |
| 1 new integration test (`memory_sample_positive`) | Proves `vm_stat` parsing works on live Apple Silicon |

**Test count target:** 178 Rust passing, 7 ignored (currently 168 passing, 6 ignored).

---

## 2. What Already Exists (Do Not Rebuild)

| Component | Phase | Current State |
|-----------|-------|--------------|
| `VoiceCoordinator::health_check_and_restart()` | 10/13 | ✅ restart logic correct; fix log-spam only |
| `VOICE_WORKER_RESTART_MAX_ATTEMPTS` / `_BACKOFF_SECS` | 10 | ✅ consumed — remove `#[allow(dead_code)]` |
| `BrowserCoordinator::health_check_and_restart()` | 14 | ✅ log-once already correct; add `is_permanently_degraded()` only |
| Stale socket cleanup on startup | 1 | ✅ `cleanup_stale_socket()` in main.rs — no changes |
| Worker restart tests | 10/14 | ✅ basic; extend in §6.3 |
| `text_input_produces_streaming_tokens` e2e test | 6 | ✅ already gated `#[ignore]` — rename in `make test-e2e` |

---

## 3. Architectural Decisions

### 3.1 Memory probe is a warning layer — not a hard gate

Ollama controls its own model lifecycle. When HEAVY is requested, Ollama evicts the
current resident model before loading the 32B weights. The eviction happens atomically
inside Ollama — Rust cannot and should not race it.

The memory sentinel therefore **logs, never blocks**. A `warn!` when headroom < threshold
surfaces in structured logs and is visible to the operator. Blocking the request would
be wrong: the operator might have explicitly requested HEAVY, and Ollama might successfully
complete the eviction and load.

Future work (beyond Phase 15): after HEAVY inference completes, trigger Ollama model
unload via `keep_alive: 0` on the `/api/chat` endpoint. This is not in Phase 15 scope —
`InferenceEngine` does not yet expose a model-unload path.

### 3.2 Permanent degradation uses the orchestrator as the notification bus

Both coordinators are owned by the orchestrator (voice directly, browser via ActionEngine).
The gRPC sender (`tx`) lives on the orchestrator. Rather than threading `tx` into the
coordinators (which creates a circular dependency: coordinator → proto types → server),
the orchestrator checks coordinator state after each health-check call and decides
whether to surface a UI notification itself.

The notification suppression flag (`voice_degraded_notified: bool`) lives on the
orchestrator, not the coordinator. This is correct: notification is a UI concern, not a
worker lifecycle concern.

### 3.3 `permissions.sh` uses TCC database directly (SIP disabled)

The TCC database at `~/Library/Application Support/com.apple.TCC/TCC.db` is normally
protected by SIP. Since SIP is disabled on this machine, `sqlite3` can read it directly.
The script queries for Accessibility and Microphone access.

The executable identifier in the TCC database for a `swift run` dev build is the raw
binary path (e.g., `.build/debug/Dexter`) — not a bundle ID. The script checks for
both the dev build path and the system-installed path, falling back to guided
`open x-apple.systempreferences:` instructions when the query yields no result.

### 3.4 `make smoke` uses `cargo check`, not `cargo build`

`cargo build` for the full debug binary takes 30-90s cold. `cargo check` performs full
type-checking and borrow-checking in ~5s without producing artifacts. The purpose of
`smoke` is to verify that none of the three layers (Rust, Swift, Python) have broken
compilation — not to produce runnable binaries. `swift build` is unavoidably a full
build (SwiftPM has no check-only mode), but it caches incrementally and is fast on
warm runs.

---

## 4. Acceptance Criteria

| # | Criterion | How to Verify |
|---|-----------|--------------|
| AC-1 | `system::memory::parse_vm_stat()` returns correct `available_gb` from known fixture input | Unit test `parse_vm_stat_valid_returns_correct_gb` |
| AC-2 | `parse_vm_stat()` returns `Err` when "Pages free:" is absent | Unit test `parse_vm_stat_missing_field_returns_err` |
| AC-3 | Page size is parsed from vm_stat header, not hardcoded | Unit tests using non-16384 page sizes pass correctly |
| AC-4 | `VoiceCoordinator::health_check_and_restart()` logs "permanently unavailable" exactly once, not every tick | Unit test + manual log inspection |
| AC-5 | `VoiceCoordinator::is_permanently_degraded()` returns true iff `restarts >= VOICE_WORKER_RESTART_MAX_ATTEMPTS` | Unit test |
| AC-6 | `BrowserCoordinator::is_permanently_degraded()` returns true iff `restart_count >= VOICE_WORKER_RESTART_MAX_ATTEMPTS` | Unit test |
| AC-7 | `CoreOrchestrator::voice_health_check()` emits a `TextResponse` to Swift when voice permanently fails, exactly once | Unit test `voice_degraded_notification_sent_once` |
| AC-8 | `CoreOrchestrator` has `voice_degraded_notified: bool` and `browser_degraded_notified: bool` initialized to `false` | Unit test `make_orchestrator` passes; new fields present |
| AC-9 | `cargo test` produces ≥ 178 passing tests, 0 failures | `make test` |
| AC-10 | `cargo test` produces 0 compiler warnings (0 new warnings) | `make test` output |
| AC-11 | `swift build` produces 0 project-code warnings | `cd src/swift && swift build` |
| AC-12 | `uv run pytest` produces 19/19 passing | `make test-python` |
| AC-13 | `make smoke` completes in < 60s and exits 0 | `make smoke` |
| AC-14 | `make test-e2e` runs all `#[ignore]` tests including `text_input_produces_streaming_tokens` | `make test-e2e` (requires Ollama) |
| AC-15 | `make check-permissions` prints pass/fail for Accessibility and Microphone | `make check-permissions` |
| AC-16 | Constants `VOICE_WORKER_RESTART_MAX_ATTEMPTS` and `VOICE_WORKER_RESTART_BACKOFF_SECS` have no `#[allow(dead_code)]` annotation | `grep -n allow.dead_code src/rust-core/src/constants.rs` |
| AC-17 | `system::memory::sample()` returns `Ok(snapshot)` with `snapshot.available_gb > 0` on live Apple Silicon | Integration test `memory_sample_positive` (`#[ignore]`) |

---

## 5. What Is NOT in Scope

- Hard-blocking HEAVY model loading based on memory — Ollama manages unloading; we log only
- Explicit Ollama model unload via `keep_alive: 0` — requires new InferenceEngine method, Phase 16+
- Fine-tuning training pipeline — hook exists in PersonalityLayer, training loop is Phase 16+
- sqlite-vec migration for VectorStore — stable Rust crate not yet available (MEMORY.md note)
- Replay or recovery of conversation history through TTS after crash — out of scope
- LoRA adapter path wiring — null in config until training phase

---

## 6. Implementation Guide

Implement in exactly this order. Run `cargo test` after each step.

---

### Step 1: `src/rust-core/src/system/` module

**New files:**
- `src/rust-core/src/system/mod.rs`
- `src/rust-core/src/system/memory.rs`

**Constants to add to `src/rust-core/src/constants.rs`** (Phase 15 section at end of file):

```rust
// ── System Memory (Phase 15) ─────────────────────────────────────────────────

/// Shell command used to sample macOS virtual memory statistics.
/// Output format: "Pages free: N." etc. Page size on the first line.
pub const VM_STAT_CMD: &str = "vm_stat";

/// Minimum available (free + inactive) headroom in GiB before a HEAVY model
/// inference request triggers a warning log entry.
///
/// deepseek-r1:32b requires ~20GB. Ollama evicts the current resident model
/// before loading HEAVY — this threshold guards against situations where
/// free + inactive < the model's footprint, indicating likely swap pressure.
/// This is a warning threshold only — it never blocks inference.
pub const MEMORY_HEAVY_WARN_THRESHOLD_GB: f64 = 20.0;
```

**`src/rust-core/src/system/mod.rs`:**

```rust
pub mod memory;
```

**`src/rust-core/src/system/memory.rs`:**

```rust
/// System memory sampling via vm_stat.
///
/// Provides a non-fatal headroom estimate before heavy model inference.
/// All functions are best-effort: errors are logged but never propagated
/// to callers as fatal — the memory sampler is a diagnostic tool only.
use std::process::Command;
use tracing::warn;
use crate::constants::{MEMORY_HEAVY_WARN_THRESHOLD_GB, VM_STAT_CMD};

// ── Public API ────────────────────────────────────────────────────────────────

/// Memory snapshot from a single vm_stat invocation.
#[derive(Debug, Clone)]
pub struct MemorySnapshot {
    /// Available memory in GiB: (free + inactive) × page_size.
    /// Conservative: excludes speculative and purgeable pages.
    pub available_gb: f64,
    /// Page size in bytes as reported by vm_stat header.
    pub page_size: u64,
}

/// Error type for memory sampling failures.
#[derive(Debug)]
pub enum MemorySampleError {
    Io(std::io::Error),
    Utf8(std::str::Utf8Error),
    MissingField(&'static str),
    ParseError(String),
}

impl std::fmt::Display for MemorySampleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e)            => write!(f, "vm_stat I/O error: {e}"),
            Self::Utf8(e)          => write!(f, "vm_stat output is not UTF-8: {e}"),
            Self::MissingField(s)  => write!(f, "vm_stat output missing field: {s}"),
            Self::ParseError(s)    => write!(f, "vm_stat parse error: {s}"),
        }
    }
}

impl From<std::io::Error>    for MemorySampleError { fn from(e: std::io::Error)    -> Self { Self::Io(e) } }
impl From<std::str::Utf8Error> for MemorySampleError { fn from(e: std::str::Utf8Error) -> Self { Self::Utf8(e) } }

/// Run vm_stat and return a MemorySnapshot.
pub fn sample() -> Result<MemorySnapshot, MemorySampleError> {
    let output = Command::new(VM_STAT_CMD).output()?;
    parse_vm_stat(&output.stdout)
}

/// Log a warning if available memory is below MEMORY_HEAVY_WARN_THRESHOLD_GB.
///
/// Called by the orchestrator before routing to the HEAVY model tier.
/// Non-fatal: always returns, even if vm_stat fails.
pub fn warn_if_low_for_heavy() {
    match sample() {
        Ok(snap) => {
            if snap.available_gb < MEMORY_HEAVY_WARN_THRESHOLD_GB {
                warn!(
                    available_gb  = snap.available_gb,
                    threshold_gb  = MEMORY_HEAVY_WARN_THRESHOLD_GB,
                    page_size     = snap.page_size,
                    "Available memory below HEAVY model threshold — system may experience swap pressure"
                );
            } else {
                tracing::info!(
                    available_gb = snap.available_gb,
                    "Memory headroom OK for HEAVY model"
                );
            }
        }
        Err(e) => {
            warn!(error = %e, "Failed to sample memory before HEAVY inference — proceeding anyway");
        }
    }
}

// ── Internal parser ───────────────────────────────────────────────────────────

/// Parse raw vm_stat stdout bytes into a MemorySnapshot.
///
/// Public for unit testing. Not part of the stable API — callers should use `sample()`.
///
/// Expected vm_stat output format:
/// ```text
/// Mach Virtual Memory Statistics: (page size of 16384 bytes)
/// Pages free:                               12345.
/// Pages active:                            234567.
/// Pages inactive:                          345678.
/// ...
/// ```
pub fn parse_vm_stat(data: &[u8]) -> Result<MemorySnapshot, MemorySampleError> {
    let text = std::str::from_utf8(data)?;
    let mut page_size: Option<u64> = None;
    let mut free:      Option<u64> = None;
    let mut inactive:  Option<u64> = None;

    for line in text.lines() {
        let trimmed = line.trim();

        // Parse page size from the header line.
        // Format: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
        if page_size.is_none() {
            if let Some(ps) = extract_page_size(trimmed) {
                page_size = Some(ps);
                continue;
            }
        }

        if let Some(n) = parse_pages_line("Pages free:", trimmed) {
            free = Some(n);
        } else if let Some(n) = parse_pages_line("Pages inactive:", trimmed) {
            inactive = Some(n);
        }
    }

    let page_size = page_size.ok_or(MemorySampleError::MissingField("page size header"))?;
    let free      = free.ok_or(MemorySampleError::MissingField("Pages free"))?;
    let inactive  = inactive.ok_or(MemorySampleError::MissingField("Pages inactive"))?;

    let available_bytes = (free + inactive) * page_size;
    let available_gb    = available_bytes as f64 / 1_073_741_824.0; // 1 GiB = 2^30 bytes

    Ok(MemorySnapshot { available_gb, page_size })
}

/// Extract the page size (in bytes) from the vm_stat header line.
///
/// Returns None if the line is not the header or cannot be parsed.
fn extract_page_size(line: &str) -> Option<u64> {
    // Target: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let start = line.find("page size of")?;
    let rest  = &line[start + "page size of".len()..];
    let rest  = rest.trim();
    let end   = rest.find(' ')?;
    rest[..end].parse().ok()
}

/// Parse a "Pages <label>: <digits>." line, returning the digit value.
///
/// vm_stat uses trailing periods as field terminators. This parser strips them.
/// Returns None if the line does not start with `label` or cannot be parsed.
fn parse_pages_line(label: &str, line: &str) -> Option<u64> {
    if !line.starts_with(label) {
        return None;
    }
    let rest = line[label.len()..].trim().trim_end_matches('.');
    rest.parse().ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_VM_STAT: &str = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                               10000.
Pages active:                            200000.
Pages inactive:                           50000.
Pages speculative:                          500.
Pages throttled:                              0.
Pages wired down:                         40000.
Pages purgeable:                             10.
";

    #[test]
    fn parse_vm_stat_valid_returns_correct_gb() {
        let snap = parse_vm_stat(FIXTURE_VM_STAT.as_bytes()).unwrap();
        // (10000 + 50000) × 16384 = 60000 × 16384 = 983_040_000 bytes ≈ 0.915 GiB
        let expected = (10_000u64 + 50_000) * 16_384;
        let expected_gb = expected as f64 / 1_073_741_824.0;
        assert!((snap.available_gb - expected_gb).abs() < 1e-6,
            "available_gb mismatch: got {}, expected {}", snap.available_gb, expected_gb);
    }

    #[test]
    fn parse_vm_stat_page_size_parsed_from_header() {
        let snap = parse_vm_stat(FIXTURE_VM_STAT.as_bytes()).unwrap();
        assert_eq!(snap.page_size, 16_384, "Page size must be extracted from the vm_stat header");
    }

    #[test]
    fn parse_vm_stat_non_apple_silicon_page_size() {
        // Intel Macs use 4096-byte pages. Parser must handle any page size.
        let data = "\
Mach Virtual Memory Statistics: (page size of 4096 bytes)
Pages free:                               10000.
Pages inactive:                           50000.
";
        let snap = parse_vm_stat(data.as_bytes()).unwrap();
        assert_eq!(snap.page_size, 4_096);
        let expected_gb = (10_000u64 + 50_000) * 4_096;
        let expected_gb = expected_gb as f64 / 1_073_741_824.0;
        assert!((snap.available_gb - expected_gb).abs() < 1e-6);
    }

    #[test]
    fn parse_vm_stat_missing_pages_free_returns_err() {
        let data = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages inactive:                           50000.
";
        let err = parse_vm_stat(data.as_bytes()).unwrap_err();
        assert!(matches!(err, MemorySampleError::MissingField("Pages free")));
    }

    #[test]
    fn parse_vm_stat_missing_header_returns_err() {
        let data = "Pages free: 10000.\nPages inactive: 50000.\n";
        let err = parse_vm_stat(data.as_bytes()).unwrap_err();
        assert!(matches!(err, MemorySampleError::MissingField("page size header")));
    }

    /// Calls the real `vm_stat` binary. Run with: make test-e2e
    #[tokio::test]
    #[ignore = "requires live Apple Silicon — run with: make test-e2e"]
    async fn memory_sample_positive_on_live_machine() {
        let snap = sample().expect("vm_stat must succeed on live machine");
        assert!(snap.available_gb > 0.0, "Available GB must be positive");
        assert!(snap.page_size > 0,      "Page size must be positive");
        // Apple Silicon always uses 16384-byte pages.
        assert_eq!(snap.page_size, 16_384, "Expected 16KB pages on Apple Silicon");
    }
}
```

**Register module in `src/rust-core/src/main.rs`:**

Add `mod system;` directly after `mod session;`.

**After Step 1: cargo test → 173 passing (168 + 5 new unit tests), 7 ignored.**

---

### Step 2: Orchestrator — memory warning on HEAVY routing

**File:** `src/rust-core/src/orchestrator.rs`

Add `use crate::system;` to the crate imports.

In `handle_text_input()`, after the routing decision (look for `let decision = self.router.route(...)` or equivalent):

```rust
// Phase 15: Log available memory before dispatching to HEAVY tier.
// The HEAVY model (deepseek-r1:32b, ~20GB) triggers Ollama to evict
// the current resident model before loading. This log makes swap pressure
// visible in structured logs. Non-fatal — inference proceeds regardless.
if matches!(decision.model, ModelId::Heavy) {
    system::memory::warn_if_low_for_heavy();
}
```

**No new unit tests for this step** — `warn_if_low_for_heavy()` is thoroughly tested in
Step 1. The orchestrator test suite validates routing decisions separately.

**After Step 2: cargo test → 173 passing, 0 warnings.**

---

### Step 3: Coordinator permanent-degradation hardening

#### 3A: `VoiceCoordinator` — fix log-spam + add `is_permanently_degraded()`

**File:** `src/rust-core/src/voice/coordinator.rs`

**Current bug:** The `else if !alive` branch in `health_check_and_restart()` fires on
every 5-second health-check tick once the worker has permanently failed. Fix it:

```rust
// BEFORE (fires every 5s after max restarts):
} else if !alive {
    warn!("TTS worker permanently unavailable after max restart attempts");
    self.tts_ready.store(false, Ordering::Relaxed);
    *self.tts.lock().await = None;
}

// AFTER (fires exactly once, at the transition into permanent degradation):
} else if !alive && self.restarts == VOICE_WORKER_RESTART_MAX_ATTEMPTS {
    // Log exactly once when entering permanent degradation.
    warn!(max_attempts = VOICE_WORKER_RESTART_MAX_ATTEMPTS,
          "TTS worker permanently unavailable — text-only mode active");
    self.tts_ready.store(false, Ordering::Relaxed);
    *self.tts.lock().await = None;
    // restarts is already at MAX; do NOT increment further (would overflow u32 eventually).
}
// If restarts > MAX, the condition above is false (== vs >=). Silent no-op on
// subsequent ticks — the coordinator is already permanently degraded.
```

**Why `==` not `>=`:** After the first permanent-degradation tick sets `restarts` to MAX
and drops the worker client, subsequent ticks see `guard.as_mut()` return `None`, so
`alive = false`. Then: `!alive && self.restarts == MAX` → true, but we already logged.
Fix: increment `restarts` to `MAX + 1` on the permanent-degradation branch so subsequent
ticks hit `self.restarts > MAX` (which is `> VOICE_WORKER_RESTART_MAX_ATTEMPTS`, making
`== MAX` false). Alternatively, add a bool flag. The cleanest approach is the bool flag:

```rust
// Add field to VoiceCoordinator struct:
permanently_degraded: bool,

// Initialize in new_degraded():
permanently_degraded: false,

// In health_check_and_restart(), replace the else if block:
} else if !alive && !self.permanently_degraded {
    self.permanently_degraded = true;
    warn!(max_attempts = VOICE_WORKER_RESTART_MAX_ATTEMPTS,
          "TTS worker permanently unavailable — entering text-only mode");
    self.tts_ready.store(false, Ordering::Relaxed);
    *self.tts.lock().await = None;
}
// subsequent ticks: !alive && !permanently_degraded → false → silent no-op
```

**Add accessor:**

```rust
/// True when the worker has exceeded restart limits and will not be retried.
/// The orchestrator uses this to surface a one-time TextResponse to the UI.
pub fn is_permanently_degraded(&self) -> bool {
    self.permanently_degraded
}
```

**Add 2 unit tests:**

```rust
#[test]
fn is_permanently_degraded_false_initially() {
    let vc = VoiceCoordinator::new_degraded();
    assert!(!vc.is_permanently_degraded());
}

#[test]
fn is_permanently_degraded_field_set_directly() {
    let mut vc = VoiceCoordinator::new_degraded();
    vc.permanently_degraded = true;
    assert!(vc.is_permanently_degraded());
}
```

Note: `permanently_degraded` must be accessible from the test module inside the same
file. Since it's a private field, the tests can access it directly because they live in
the same module (the `#[cfg(test)] mod tests` block is inside `coordinator.rs`).

#### 3B: `BrowserCoordinator` — add `is_permanently_degraded()`

**File:** `src/rust-core/src/browser/coordinator.rs`

`BrowserCoordinator` already handles log-once correctly (the `count == VOICE_WORKER_RESTART_MAX_ATTEMPTS`
exact match on line 138). Only add the accessor:

```rust
/// True when the browser worker has exceeded restart limits.
pub fn is_permanently_degraded(&self) -> bool {
    self.restart_count.load(std::sync::atomic::Ordering::Relaxed) >= VOICE_WORKER_RESTART_MAX_ATTEMPTS
}
```

**Add 2 unit tests:**

```rust
#[test]
fn is_permanently_degraded_false_initially() {
    let c = BrowserCoordinator::new_degraded();
    assert!(!c.is_permanently_degraded());
}

#[test]
fn is_permanently_degraded_true_when_count_at_max() {
    let c = BrowserCoordinator::new_degraded();
    c.restart_count.store(VOICE_WORKER_RESTART_MAX_ATTEMPTS, Ordering::Relaxed);
    assert!(c.is_permanently_degraded());
}
```

**After Step 3: cargo test → 177 passing (173 + 4 new), 0 warnings.**

---

### Step 4: Orchestrator — surface permanent degradation to UI

**File:** `src/rust-core/src/orchestrator.rs`

Add two fields to `CoreOrchestrator`:

```rust
// Phase 15: track whether the one-time UI notification has been sent.
// Prevents sending "voice degraded" on every health-check tick.
voice_degraded_notified:   bool,
browser_degraded_notified: bool,
```

Initialize both to `false` in `CoreOrchestrator::new()` (or wherever the struct is
constructed — find the initialization site and add the two fields).

**`voice_health_check()` — add notification logic:**

```rust
pub async fn voice_health_check(&mut self) {
    self.voice.health_check_and_restart().await;

    // Phase 15: one-time UI notification on permanent degradation.
    if self.voice.is_permanently_degraded() && !self.voice_degraded_notified {
        self.voice_degraded_notified = true;
        warn!("TTS worker permanently degraded — notifying UI");
        self.send_text_response(
            "Voice capability lost after repeated failures. Text-only mode is now active.",
            true,   // is_final: this is a complete notification, not a streaming token
        ).await;
    }
}
```

**`browser_health_check()` — add notification logic:**

```rust
pub async fn browser_health_check(&mut self) {
    self.action_engine.browser_health_check().await;

    // Phase 15: one-time UI notification on permanent browser degradation.
    if self.action_engine.is_browser_permanently_degraded() && !self.browser_degraded_notified {
        self.browser_degraded_notified = true;
        warn!("Browser worker permanently degraded — notifying UI");
        self.send_text_response(
            "Browser automation unavailable after repeated failures.",
            true,
        ).await;
    }
}
```

**Add `is_browser_permanently_degraded()` delegation on `ActionEngine`:**

**File:** `src/rust-core/src/action/engine.rs`

```rust
pub fn is_browser_permanently_degraded(&self) -> bool {
    self.browser.is_permanently_degraded()
}
```

**Find `send_text_response` or equivalent helper in orchestrator.rs:**

The orchestrator already has a `tx: UnboundedSender<Result<ServerEvent, Status>>` field
(used in `handle_text_input` for emitting tokens). Locate the helper that sends a
`TextResponse` server event — it will be something like `send_text_response(text, is_final)`.
If no standalone helper exists, extract one from `handle_text_input`.

**Add 1 unit test** to the existing orchestrator test block:

```rust
#[test]
fn orchestrator_degraded_notification_flags_start_false() {
    let o = make_orchestrator();
    assert!(!o.voice_degraded_notified);
    assert!(!o.browser_degraded_notified);
}
```

**After Step 4: cargo test → 178 passing (177 + 1 new), 0 warnings.**

---

### Step 5: `scripts/permissions.sh` + `make check-permissions`

**New file:** `scripts/permissions.sh`

```bash
#!/usr/bin/env bash
# scripts/permissions.sh — Check macOS TCC permissions required by Dexter.
#
# Queries ~/Library/Application Support/com.apple.TCC/TCC.db directly.
# This works because SIP is disabled on this machine (required for Dexter).
# On a SIP-enabled machine, the TCC database is unreadable without entitlements;
# the script falls back to guidance for opening System Settings manually.
#
# Permissions checked:
#   kTCCServiceAccessibility — required for AXObserver (context observation)
#   kTCCServiceMicrophone    — required for AVCaptureSession (voice input)

set -euo pipefail

PASS="✓"
FAIL="✗"
WARN="⚠"
overall_ok=true

tcc_db="${HOME}/Library/Application Support/com.apple.TCC/TCC.db"

# The TCC client identifier for a SwiftPM debug build or release binary.
# SwiftPM executables are not bundled — TCC uses the executable path as the identifier.
SWIFT_BUILD_DEBUG="$(find "$(pwd)/src/swift/.build" -name "Dexter" -type f 2>/dev/null | head -1 || true)"
SWIFT_INSTALLED="/Applications/Dexter.app/Contents/MacOS/Dexter"

echo ""
echo "==> Checking macOS TCC permissions for Dexter"

check_tcc_permission() {
    local service="$1"
    local label="$2"
    local pref_path="$3"

    if [ ! -f "$tcc_db" ]; then
        printf "  %s  %s: TCC database not readable (SIP may be enabled)\n" "$WARN" "$label"
        echo "       Open: System Settings → Privacy & Security → $label"
        echo "       Grant access to Dexter (or to Terminal during development)"
        overall_ok=false
        return
    fi

    # Check all known executable paths.
    local found=false
    for client in "$SWIFT_BUILD_DEBUG" "$SWIFT_INSTALLED" "com.apple.Terminal" "/usr/bin/python3"; do
        [ -z "$client" ] && continue
        result=$(sqlite3 "$tcc_db" \
            "SELECT allowed FROM access WHERE service='${service}' AND client='${client}' LIMIT 1;" \
            2>/dev/null || true)
        if [ "$result" = "1" ]; then
            printf "  %s  %s: granted (client: %s)\n" "$PASS" "$label" "$(basename "$client")"
            found=true
            break
        fi
    done

    if [ "$found" = false ]; then
        printf "  %s  %s: not found in TCC database\n" "$FAIL" "$label"
        echo "       Open: System Settings → Privacy & Security → $label"
        echo "       Add Dexter (or Terminal for development), toggle ON"
        echo "       Command: open '${pref_path}'"
        overall_ok=false
    fi
}

check_tcc_permission \
    "kTCCServiceAccessibility" \
    "Accessibility" \
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"

check_tcc_permission \
    "kTCCServiceMicrophone" \
    "Microphone" \
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"

echo ""
if [ "$overall_ok" = "true" ]; then
    echo "==> All permissions granted"
    exit 0
else
    echo "==> One or more permissions missing — see above for instructions" >&2
    exit 1
fi
```

Make the script executable: `chmod +x scripts/permissions.sh`.

**Add to `Makefile`:**

```makefile
## check-permissions: check macOS TCC permissions required by Dexter (Accessibility, Microphone)
check-permissions:
	@bash scripts/permissions.sh
```

Add `check-permissions` to the `.PHONY` line.

Also update `make setup` to mention `make check-permissions` in its comment, or chain it:
The `setup` target is a developer-environment tool, not a permissions tool, so keep them
separate. Add a comment to the `setup` target pointing at `check-permissions`:

```makefile
## setup: verify all required toolchains and protoc plugins are available
##        (run `make check-permissions` to verify macOS TCC permissions)
setup:
	@bash scripts/setup.sh
```

---

### Step 6: Integration tests + Makefile targets

#### 6.1 `make smoke`

Add to `Makefile`:

```makefile
## smoke: fast syntax+type check across all three layers (no Ollama required, < 60s)
##
## Uses `cargo check` (not build) — full type/borrow checking without producing artifacts.
## swift build is incremental and fast on warm cache. pytest validates Python workers.
## Run this before pushing any change to verify nothing is broken across all layers.
smoke:
	@echo "==> Rust type check"
	cd $(RUST_CORE_DIR) && cargo check
	@echo "==> Swift build"
	cd $(SWIFT_DIR) && swift build 2>&1 | tail -8
	@echo "==> Python worker tests"
	cd src/python-workers && uv run pytest -q
	@echo "==> Smoke check passed"
```

#### 6.2 `make test-e2e`

Rename `make test-inference` → `make test-e2e`. Keep `test-inference` as a deprecated alias
(it prints a deprecation notice and delegates to `test-e2e`):

```makefile
## test-e2e: run all integration tests (requires live Ollama + models; use: make test-e2e)
##
## Includes Phase 4 InferenceEngine, Phase 6 orchestrator e2e, Phase 15 memory sample,
## and any future integration tests. All are marked #[ignore] in cargo test.
##
## Prerequisites:
##   1. Ollama running:       ollama serve
##   2. phi3:mini available:  ollama pull phi3:mini  (used by e2e session test)
##   3. For memory test:      any Apple Silicon Mac (vm_stat must be present)
##
## To run only unit tests: make test
## To run both:            make test && make test-e2e
test-e2e:
	cd $(RUST_CORE_DIR) && cargo test -- --ignored

## test-inference: deprecated alias for test-e2e
test-inference:
	@echo "⚠  test-inference is deprecated — use 'make test-e2e' instead"
	$(MAKE) test-e2e
```

Update `.PHONY` to include `smoke test-e2e test-inference`.

#### 6.3 Verify integration test coverage

The `memory_sample_positive_on_live_machine` test is already in `system/memory.rs`
(added in Step 1). It is `#[ignore]`-gated and will run under `make test-e2e`.

The existing `text_input_produces_streaming_tokens` test in `ipc/server.rs` covers the
full text pipeline e2e. It runs under `make test-e2e`.

**No additional integration tests needed for Phase 15.** The two `#[ignore]` tests
above plus the existing suite form the complete regression set.

---

### Step 7: Cleanup + full regression

#### 7.1 Remove stale `#[allow(dead_code)]` annotations

**File:** `src/rust-core/src/constants.rs`

Remove `#[allow(dead_code)]` from constants that are now actively consumed:

| Constant | Consumed By | Action |
|----------|-------------|--------|
| `VOICE_WORKER_RESTART_MAX_ATTEMPTS` | coordinator.rs (both) | Remove annotation |
| `VOICE_WORKER_RESTART_BACKOFF_SECS` | coordinator.rs (both) | Remove annotation |
| `VM_STAT_CMD` | system/memory.rs | Was never annotated (new in Phase 15) — no action |
| `MEMORY_HEAVY_WARN_THRESHOLD_GB` | system/memory.rs | Was never annotated — no action |
| `VOICE_WORKER_HEALTH_INTERVAL_SECS` | ipc/server.rs | Check — may still need `#[allow]` if ipc/server.rs imports it; verify |
| `VOICE_WORKER_HEALTH_TIMEOUT_SECS` | worker_client.rs | Check — verify consumption |

Do **not** remove `#[allow(dead_code)]` from constants that are still genuinely unused
(e.g., `CONTEXT_DEBOUNCE_MS` which is authoritative for Swift but not imported in Rust,
`SOCKET_TIMEOUT_SECS` which is a Makefile mirror only, etc.).

The criterion: if `cargo test` produces no warning for a constant, its annotation is
already correct regardless of whether we've explicitly reviewed it.

#### 7.2 Verify `make check-permissions` exits 0 on this machine

Run `make check-permissions`. If Accessibility or Microphone TCC entries are not found
for any known executable path, grant them in System Settings and re-run. Both must pass
before the phase is declared complete.

#### 7.3 Full regression

```bash
cargo test            # ≥ 178 passing, 0 failed, 0 warnings
swift build           # Build complete, 0 project-code warnings
uv run pytest -q      # 19 passed
make check-permissions # ✓ Accessibility, ✓ Microphone
make smoke            # All three layers pass
```

Run `make test-e2e` only if Ollama is available with phi3:mini.

---

## 7. Known Pitfalls

**Pitfall: `send_text_response` vs inline send in orchestrator.rs**

The orchestrator's `handle_text_input()` sends `TextResponse` events via the `tx` channel.
If there is no standalone helper for this, Step 4 requires extracting one before adding
the degradation notification calls. Don't inline the channel send directly in
`voice_health_check()` — that duplicates the proto construction logic. Extract:

```rust
async fn send_text_response_to_ui(&self, text: &str, is_final: bool) {
    use crate::ipc::server::proto::{server_event, ServerEvent, TextResponse};
    let event = ServerEvent {
        trace_id: uuid::Uuid::new_v4().to_string(),
        event: Some(server_event::Event::TextResponse(TextResponse {
            content:  text.to_string(),
            is_final,
        })),
    };
    if let Err(e) = self.tx.send(Ok(event)) {
        tracing::warn!(error = %e, "Failed to send degradation notification to UI");
    }
}
```

Then call `self.send_text_response_to_ui("Voice capability lost...", true).await;`

**Pitfall: `BrowserCoordinator::restart_count` is `Arc<AtomicU32>` — no `&mut self` needed**

`is_permanently_degraded()` on `BrowserCoordinator` takes `&self` (not `&mut self`)
because it reads `self.restart_count` via `load()`. This matches the existing
`is_available()` signature. No ownership changes needed.

**Pitfall: `VoiceCoordinator::permanently_degraded` is a plain `bool` (not `Arc<AtomicBool>`)**

`health_check_and_restart()` on `VoiceCoordinator` takes `&mut self` — confirmed by
`voice/coordinator.rs`: it mutates `self.restarts: u32` (a plain non-atomic field) with
`self.restarts += 1`. Adding `permanently_degraded: bool` to the struct and mutating it
inside the same `&mut self` method compiles directly — no atomics needed.

The `Arc<>` fields (`tts: Arc<Mutex<>>`, `tts_ready: Arc<AtomicBool>`) exist because
TTS synthesis tasks spawned by the orchestrator need concurrent access to the worker
client. That concurrent access pattern applies to those fields specifically, not to
`permanently_degraded`, which is only ever read/written from the orchestrator's
sequential health-check loop. Do **not** use `AtomicBool` — it is unnecessary complexity
for state that is only mutated from a single `&mut self` method.

**Pitfall: `vm_stat` trailing period on numeric fields**

vm_stat terminates numeric values with a period: `Pages free:   12345.`
`parse_pages_line()` calls `.trim_end_matches('.')` before `.parse::<u64>()`.
A fixture that omits the period will still parse correctly. A fixture with multiple
trailing periods will fail (`.trim_end_matches('.')` removes all of them, which is fine).

**Pitfall: `memory_sample_positive_on_live_machine` is `#[tokio::test]` but `sample()` is synchronous**

`sample()` calls `std::process::Command::output()` — a blocking call. In a tokio test,
this is acceptable for integration tests because they run serially and the subprocess
is fast (<50ms for vm_stat). Do not use `tokio::process::Command` — the test function
is already `#[ignore]`-gated and won't run in CI.

**Pitfall: `ServerEvent` has no `session_id` field — `send_text_response_to_ui` is complete as-is**

`dexter.proto` declares:

```protobuf
message ServerEvent {
  string trace_id = 1;
  oneof event {
    TextResponse      text_response  = 2;
    EntityStateChange entity_state   = 3;
    AudioResponse     audio_response = 4;
    ActionRequest     action_request = 5;
  }
}
```

`session_id` is exclusively on `ClientEvent` (field 2 there). `ServerEvent` carries only
`trace_id` for log correlation. DexterClient routes incoming events by *gRPC stream
identity* — one bidi stream = one session — not by any field in the event payload, so
there is nothing to mis-route. The helper's `ServerEvent { trace_id, event: Some(...) }`
construction is the complete and correct proto initialization.

**Pitfall: `check-permissions` may not find TCC entries for `swift run` debug path**

The debug binary path changes with Swift version and build configuration. The script
checks multiple candidate paths. If none match, it prints guided instructions rather
than failing silently. Do not add more candidate paths speculatively — the script will
instruct the operator to grant access manually, which is the correct fallback.

---

## 8. Acceptance Criteria Sign-Off Checklist

```
[ ] AC-1  parse_vm_stat valid fixture → correct GB
[ ] AC-2  parse_vm_stat missing field → Err
[ ] AC-3  page size parsed from header
[ ] AC-4  VoiceCoordinator logs "permanently unavailable" exactly once
[ ] AC-5  is_permanently_degraded() on VoiceCoordinator correct
[ ] AC-6  is_permanently_degraded() on BrowserCoordinator correct
[ ] AC-7  Orchestrator TextResponse sent on permanent voice failure
[ ] AC-8  Both notified flags start as false
[ ] AC-9  cargo test ≥ 178 passing, 0 failed
[ ] AC-10 cargo test 0 warnings
[ ] AC-11 swift build 0 project-code warnings
[ ] AC-12 uv run pytest 19/19
[ ] AC-13 make smoke < 60s, exit 0
[ ] AC-14 make test-e2e runs all #[ignore] tests
[ ] AC-15 make check-permissions prints ✓ for both permissions
[ ] AC-16 VOICE_WORKER_RESTART_MAX_ATTEMPTS has no #[allow(dead_code)]
[ ] AC-17 memory_sample_positive test passes under make test-e2e
```
