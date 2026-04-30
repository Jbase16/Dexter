# Phase 28 Implementation Plan: Clipboard Context Integration

## Context

After Phase 27, Dexter has continuous ambient awareness of what app is focused and
what UI element is active — but no awareness of what the operator has *copied*. This
is a significant gap in the "fix this" / "explain this" workflow: the most natural way
to share content with Dexter is Cmd+C, then speak. Currently that content is invisible
to the inference pipeline.

Phase 28 closes this gap by monitoring `NSPasteboard.general.changeCount` in
`EventBridge`, forwarding text clipboard changes to Rust as a new
`SYSTEM_EVENT_TYPE_CLIPBOARD_CHANGED` system event, accumulating the content in
`ContextObserver`, and injecting it into the inference message stack as a
`[Clipboard: ...]` system message alongside the existing `[Context: ...]` and
`[Memory: ...]` injections.

**No proactive reaction on clipboard change.** Clipboard content is injected as
passive context for the *next* explicitly-triggered interaction (voice or typed). The
operator copies something, then speaks — Dexter sees the clipboard content at inference
time without requiring any special phrasing.

**Scope:** Proto change (one new enum value), Swift change (one file: `EventBridge`),
Rust changes (two files: `constants.rs`, `context_observer.rs`) plus a minimal hook in
`orchestrator.rs`. No Python worker changes. No new dependencies.

---

## Why Clipboard, Not Text Selection

Text selection (the highlighted region on screen) is the other obvious "share content
with Dexter" mechanism. It was considered and deferred for three reasons:

1. **Event noise.** AX selection changes on every cursor movement — arrow key presses,
   click-drag, shift+click. Even with aggressive debouncing, it produces far more events
   than clipboard changes. Clipboard changes only on explicit operator action (Cmd+C /
   `Edit → Copy`), making each change a high-intentionality signal.

2. **Infrastructure.** Reading AX selected text requires `kAXSelectedTextAttribute` on
   the focused element, with per-app quirks (some apps use `kAXSelectedTextRangeAttribute`
   + byte-range extraction). Clipboard is a single `NSPasteboard.general.string(forType:
   .string)` call with no app-specific handling.

3. **Completeness.** The operator always copies before wanting "explain this" — selection
   alone is insufficient because the content vanishes when they speak (moving focus
   away often clears the selection). Clipboard persists.

Text selection integration is Phase 29+ scope if warranted.

---

## Architecture Decisions

### Timer-based polling, not notification

`NSPasteboard` has no change-notification API. `NSWorkspace` and
`DistributedNotificationCenter` do not broadcast clipboard events. The only reliable
mechanism is polling `NSPasteboard.general.changeCount` — a monotonically-increasing
integer that increments on every pasteboard write. This is the standard macOS pattern
used by every clipboard manager (Pastebot, Raycast, etc.).

1-second polling is imperceptible for the copy → speak workflow (the delay between
copying and speaking is naturally > 1s) while keeping the polling overhead trivial (one
integer comparison per second).

### Text-only filtering — no images, files, or rich content

Phase 28 reads `string(forType: .string)` only. RTF, HTML, PDF, TIFF, and file-URL
items are ignored. The "explain this code / text" use case covers 95% of clipboard
interactions where Dexter's context awareness adds value. Binary/image clipboard is
Phase 29+ scope (would need Vision model integration).

### Content length bounds

- **`CLIPBOARD_MAX_CHARS = 4_000`** stored in `ContextObserver`. This matches
  `RETRIEVAL_MAX_CONTENT_CHARS` — the established ceiling for third-party text injected
  into the context window. 4,000 chars ≈ 600 tokens, enough for a moderately large code
  file without displacing conversation history or causing context-window pressure.

- **`CLIPBOARD_MIN_CHARS = 5`** minimum to forward. Filters out accidental single-word
  copies (Cmd+C with no text selected produces an empty pasteboard write; selecting a
  single identifier and copying gives 1–10 chars that rarely carries useful context).

### No privacy filter (by design)

The clipboard change is an explicit operator action. Unlike AX element values — where
a password manager could have a `AXSecureTextField` role that Dexter must never read —
clipboard content was deliberately placed there by the operator. Applying a heuristic
privacy filter (e.g., "looks like a password hash") would produce false positives on
legitimate content (API keys in `.env` files the operator is actively editing). The
operator trusts Dexter with clipboard content by virtue of running Dexter at all.

If a future config option `behavior.clipboard_context = false` is added, it can
disable the monitoring in `EventBridge` before it starts.

### Passive injection — no proactive trigger on clipboard change

Clipboard changes do not trigger a proactive response. The proactive engine fires on
app-focus changes (Phase 17). Triggering on clipboard would produce responses mid-copy
— intrusive and poorly timed. Clipboard content is injected silently as context for
the next explicit interaction.

### Clipboard cleared on screen lock, restored on unlock resume

When `SCREEN_LOCKED` arrives, the EventBridge stops its clipboard timer (same pattern
as AX observation is effectively paused while locked). On `SCREEN_UNLOCKED`, the timer
resumes. The stored clipboard content in `ContextObserver` is **not** cleared on lock
— if the operator locked and unlocked quickly, the last-copied content is still
relevant context.

---

## File Map

| Change     | File                                                          |
|------------|---------------------------------------------------------------|
| Modified   | `src/shared/proto/dexter.proto`                               |
| Regenerate | `src/swift/Sources/Dexter/Bridge/generated/dexter.pb.swift`   |
| Regenerate | `src/swift/Sources/Dexter/Bridge/generated/dexter.grpc.swift` |
| Modified   | `src/swift/Sources/Dexter/Bridge/EventBridge.swift`           |
| Modified   | `src/rust-core/src/constants.rs`                              |
| Modified   | `src/rust-core/src/context_observer.rs`                       |
| Modified   | `src/rust-core/src/orchestrator.rs`                           |

---

## 1. Proto: `SYSTEM_EVENT_TYPE_CLIPBOARD_CHANGED`

**Before assigning the field number, verify it against the current proto:**

```sh
grep -n "= [0-9]" src/shared/proto/dexter.proto | grep SYSTEM_EVENT_TYPE
```

Expected output shows `AUDIO_PLAYBACK_COMPLETE = 8` as the highest existing value.
`= 9` is the next available field number. **Protobuf field number conflicts produce
no compile error** — the second enum value with the same integer silently aliases the
first, causing deserialization to map both event types to the same variant. Always
verify before assigning. Current verified state: 0–8 are assigned; 9 is free.

In `dexter.proto`, append to the `SystemEventType` enum:

```protobuf
SYSTEM_EVENT_TYPE_CLIPBOARD_CHANGED = 9;  // Operator copied text to clipboard
                                           // payload: {"text": "<content>"}
```

No new message type — reuses the existing `SystemEvent.payload` string as a JSON object
with a single `text` field, consistent with the `APP_FOCUSED` / `AX_ELEMENT_CHANGED`
convention.

Run `make proto` after this change to regenerate Swift and Rust bindings.

---

## 2. `constants.rs` — new clipboard constants

```rust
// ── Clipboard Context (Phase 28) ─────────────────────────────────────────────

/// Maximum characters of clipboard text stored in ContextObserver and forwarded
/// over gRPC. Clipboard content is operator-initiated (explicit copy action).
/// 4,000 chars ≈ 600 tokens — meaningful code/text content without overwhelming
/// the context window or displacing conversation history.
///
/// Mirrors RETRIEVAL_MAX_CONTENT_CHARS: both represent third-party text injected
/// into the inference context; the same ceiling applies.
///
/// Defined in Rust as the authoritative source. EventBridge.swift mirrors this
/// value in its own `clipboardMaxChars` constant — keeping the value here prevents
/// the two sides from drifting independently.
pub const CLIPBOARD_MAX_CHARS: usize = 4_000;

/// Minimum clipboard text length to forward to Rust.
///
/// Filters out accidental empty-selection copies (Cmd+C with nothing selected
/// writes an empty or whitespace-only string to the pasteboard). 5 chars ensures
/// at least a recognisable word was copied.
pub const CLIPBOARD_MIN_CHARS: usize = 5;

/// NSPasteboard.changeCount polling interval in milliseconds.
///
/// 1,000ms = 1 second: imperceptible for the copy → speak workflow while keeping
/// polling overhead trivial (one integer comparison per second). The delay between
/// copying content and the operator speaking their question is naturally > 1s.
///
/// Authoritative source here; consumed by EventBridge.swift as `clipboardPollIntervalMs`.
#[allow(dead_code)] // authoritative source; consumed by EventBridge.swift, not Rust
pub const CLIPBOARD_POLL_INTERVAL_MS: u64 = 1_000;
```

---

## 3. `EventBridge.swift` — clipboard polling

### 3a. New constants in `EventBridge`

```swift
// Mirrors Rust's CLIPBOARD_MAX_CHARS. Update both together when tuning.
private static let clipboardMaxChars: Int = 4_000

// Mirrors Rust's CLIPBOARD_MIN_CHARS. Update both together when tuning.
private static let clipboardMinChars: Int = 5

// Mirrors Rust's CLIPBOARD_POLL_INTERVAL_MS.
private static let clipboardPollIntervalMs: Int = 1_000
```

### 3b. New state fields

```swift
// Clipboard polling.
// lastClipboardChangeCount tracks NSPasteboard.general.changeCount.
// Updated on every poll regardless of content change — the actual change
// detection uses the integer comparison, not the content.
private var lastClipboardChangeCount: Int  = -1   // -1 → forces a read on first poll
private var clipboardTimer:           Timer?
```

### 3c. Clipboard polling lifecycle

Add a `startClipboardPolling()` helper and call it from `start()`:

```swift
/// Start NSPasteboard.changeCount polling at clipboardPollIntervalMs intervals.
///
/// NSPasteboard has no change-notification API — polling changeCount is the
/// standard macOS pattern. Timer is added to .common runLoop mode so it fires
/// even during modal operations (tracking menus, resize, etc.).
///
/// Called from start() after the AX observer setup block.
private func startClipboardPolling() {
    clipboardTimer = Timer.scheduledTimer(
        withTimeInterval: Double(Self.clipboardPollIntervalMs) / 1_000.0,
        repeats: true
    ) { [weak self] _ in
        self?.handleClipboardPoll()
    }
    // .common mode: timer fires during tracking and modal run-loop modes too.
    RunLoop.main.add(clipboardTimer!, forMode: .common)
}

private func stopClipboardPolling() {
    clipboardTimer?.invalidate()
    clipboardTimer = nil
}
```

Call `startClipboardPolling()` at the end of `start()`:

```swift
func start() {
    registerWorkspaceObservers()
    registerScreenLockObservers()
    startHotkeyTap()
    startClipboardPolling()   // ← add this line

    if AXIsProcessTrustedWithOptions(nil) {
        if let frontmost = NSWorkspace.shared.frontmostApplication {
            startAXObservation(for: frontmost.processIdentifier)
            emitAppFocused(app: frontmost, queryElement: true)
        }
    } else {
        print("[EventBridge] AX permission not granted — element observation disabled")
    }
}
```

Call `stopClipboardPolling()` in `performStop()`, alongside `stopHotkeyTap()`:

```swift
private func performStop() {
    debounceWorkItem?.cancel()
    debounceWorkItem = nil

    stopClipboardPolling()   // ← add this line
    stopHotkeyTap()
    stopAXObservation()

    // ... existing observer removal unchanged
}
```

### 3d. Clipboard poll handler

```swift
/// Called by clipboardTimer on each tick. Compares changeCount to detect new
/// clipboard content; emits CLIPBOARD_CHANGED when text content meets the
/// length threshold.
///
/// All accesses on main thread: Timer fires on main run loop.
private func handleClipboardPoll() {
    let pb            = NSPasteboard.general
    let currentCount  = pb.changeCount
    guard currentCount != lastClipboardChangeCount else { return }
    lastClipboardChangeCount = currentCount

    // Read text-only content. Nil if the pasteboard holds images, files,
    // rich-text-only data, or is empty.
    guard let text = pb.string(forType: .string),
          text.count >= Self.clipboardMinChars else { return }

    // Truncate at clipboardMaxChars before sending. Rust applies a secondary guard.
    let content = text.count <= Self.clipboardMaxChars
        ? text
        : String(text.prefix(Self.clipboardMaxChars))

    sendSystemEvent(.clipboardChanged, payload: ["text": content])
}
```

The `sendSystemEvent(.clipboardChanged, ...)` call uses the existing `sendSystemEvent`
helper which serializes `payload` to JSON and emits a `Dexter_V1_ClientEvent`.
`Dexter_V1_SystemEventType.clipboardChanged` is the enum case generated by `make proto`.

---

## 4. `context_observer.rs` — clipboard field + methods

### 4a. Extend `ContextSnapshot`

```rust
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    pub app_bundle_id:    Option<String>,
    pub app_name:         Option<String>,
    pub focused_element:  Option<AxElementInfo>,
    pub is_screen_locked: bool,
    /// Operator clipboard text. None until a CLIPBOARD_CHANGED event arrives.
    /// Bounded to CLIPBOARD_MAX_CHARS. Stale (from a prior session) is not stored —
    /// ContextObserver starts fresh every session. None until first CLIPBOARD_CHANGED.
    pub clipboard_text:   Option<String>,
    pub snapshot_hash:    u64,
    pub last_updated:     DateTime<Utc>,
}
```

Initialize `clipboard_text: None` in `ContextObserver::new()`:

```rust
let snapshot = ContextSnapshot {
    app_bundle_id:   None,
    app_name:        None,
    focused_element: None,
    is_screen_locked: false,
    clipboard_text:  None,   // ← add this field
    snapshot_hash:   0,
    last_updated:    Utc::now(),
};
```

### 4b. Add `ClipboardPayload` private type

```rust
/// Deserialized from CLIPBOARD_CHANGED event payload JSON.
#[derive(Deserialize)]
struct ClipboardPayload {
    text: String,
}
```

### 4c. Add `update_from_clipboard_changed()`

```rust
/// Parse a CLIPBOARD_CHANGED payload JSON string and update `clipboard_text`.
///
/// Content is truncated to CLIPBOARD_MAX_CHARS as a secondary guard (EventBridge
/// performs the primary truncation before sending). Returns `true` if the content
/// changed (i.e., the new text differs from the previously stored text).
///
/// On JSON parse failure, logs a warning and returns `false` — a single malformed
/// payload should never crash the orchestrator.
pub fn update_from_clipboard_changed(&mut self, payload_json: &str) -> bool {
    let payload: ClipboardPayload = match serde_json::from_str(payload_json) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                error   = %e,
                payload = payload_json,
                "Failed to parse CLIPBOARD_CHANGED payload — clipboard unchanged"
            );
            return false;
        }
    };

    // Secondary truncation guard (EventBridge truncates first; this guards against
    // future callers that bypass EventBridge's truncation).
    let text = if payload.text.chars().count() > CLIPBOARD_MAX_CHARS {
        payload.text.chars().take(CLIPBOARD_MAX_CHARS).collect()
    } else {
        payload.text
    };

    // Skip no-op updates (same content arrived twice — unlikely but guard defensively).
    if self.snapshot.clipboard_text.as_deref() == Some(text.as_str()) {
        return false;
    }

    let old_hash = self.snapshot.snapshot_hash;

    self.snapshot.clipboard_text = Some(text);
    self.snapshot.last_updated   = Utc::now();
    self.snapshot.snapshot_hash  = compute_hash(&self.snapshot);

    self.snapshot.snapshot_hash != old_hash
}
```

### 4d. Add `clipboard_summary()`

```rust
/// Returns the current clipboard text for injection into the inference message stack.
///
/// Returns `None` when no clipboard content has arrived this session.
/// The full stored text is returned (up to CLIPBOARD_MAX_CHARS). The caller
/// (`prepare_messages_for_inference`) injects it as a system message.
pub fn clipboard_summary(&self) -> Option<&str> {
    self.snapshot.clipboard_text.as_deref()
}
```

### 4e. Update `compute_hash()` to include clipboard

```rust
fn compute_hash(s: &ContextSnapshot) -> u64 {
    let mut h = DefaultHasher::new();
    s.app_bundle_id.as_deref().unwrap_or("").hash(&mut h);
    s.app_name.as_deref().unwrap_or("").hash(&mut h);
    if let Some(el) = &s.focused_element {
        el.role.hash(&mut h);
        el.label.as_deref().unwrap_or("").hash(&mut h);
        el.value_preview.as_deref().unwrap_or("").hash(&mut h);
        el.is_sensitive.hash(&mut h);
    }
    s.is_screen_locked.hash(&mut h);
    s.clipboard_text.as_deref().unwrap_or("").hash(&mut h);   // ← add this line
    h.finish()
}
```

### 4f. Add `use crate::constants::CLIPBOARD_MAX_CHARS;`

```rust
use crate::constants::{AX_VALUE_PREVIEW_MAX_CHARS, CLIPBOARD_MAX_CHARS};
```

---

## 5. `orchestrator.rs` — wire `ClipboardChanged`

### 5a. Add `ClipboardChanged` arm to `handle_system_event`

In the `match SystemEventType::try_from(sys.r#type)` block, after
`Ok(SystemEventType::AxElementChanged) =>` (before or after `ScreenUnlocked`), add:

```rust
Ok(SystemEventType::ClipboardChanged) => {
    let changed = self.context_observer.update_from_clipboard_changed(&sys.payload);
    if changed {
        info!(
            session    = %self.session_id,
            char_count = self.context_observer.snapshot()
                             .clipboard_text.as_deref().map(|t| t.chars().count())
                             .unwrap_or(0),
            "Clipboard context updated"
        );
    }
}
```

No proactive trigger. No KV cache prefill on clipboard change. Pure context update.

### 5b. Inject clipboard in `prepare_messages_for_inference`

After the `[Context: ...]` injection block (Step 2) and before the `[Memory: ...]`
injection block (Phase 21), add a clipboard injection step:

```rust
// Step 2b: inject clipboard content when present.
// Ordering: personality → [Context] → [Clipboard] → [Memory] → history.
// Clipboard sits between machine-state context and recalled memory because it
// represents current, operator-initiated content (copied this session) rather
// than historical facts recalled from the vector store.
if let Some(clipboard) = self.context_observer.clipboard_summary() {
    let insert_at = messages.iter().take_while(|m| m.role == "system").count();
    messages.insert(insert_at, crate::inference::engine::Message::system(
        format!("[Clipboard: {clipboard}]")
    ));
    debug!(
        session    = %self.session_id,
        char_count = clipboard.chars().count(),
        "Clipboard content injected into inference request"
    );
}
```

The `insert_at` computation (count of leading system messages) ensures clipboard is
appended after all previously inserted system messages (`[Context: ...]`) and before
the conversation history. This is the same pattern used for `[Memory: ...]`.

**Note on ordering:** The clipboard injection uses `take_while(|m| m.role == "system").count()`
which places it at the end of the current system-message block. The memory injection
below it uses the same expression and also places at the end of the system-message block
— so memory lands after clipboard. Final order: `[personality] [Context] [Clipboard]
[Memory] [history]`. Both insertions read `messages` after the previous insert, so they
stack correctly.

---

## 6. Tests (`context_observer.rs`)

Add to the `#[cfg(test)] mod tests` block:

```rust
// ── Clipboard updates ─────────────────────────────────────────────────────────

fn clipboard_payload(text: &str) -> String {
    // Use serde_json to build the JSON — avoids escaping issues with
    // special characters in test text content.
    serde_json::json!({"text": text}).to_string()
}

#[test]
fn update_from_clipboard_changed_sets_clipboard_text() {
    let mut obs = ContextObserver::new();
    let changed = obs.update_from_clipboard_changed(&clipboard_payload("fn main() {}"));
    assert!(changed, "First clipboard update must report a change");
    assert_eq!(
        obs.snapshot().clipboard_text.as_deref(),
        Some("fn main() {}")
    );
}

#[test]
fn update_from_clipboard_changed_updates_hash() {
    let mut obs = ContextObserver::new();
    obs.update_from_clipboard_changed(&clipboard_payload("first content"));
    let hash_after_first = obs.snapshot().snapshot_hash;
    obs.update_from_clipboard_changed(&clipboard_payload("second content"));
    assert_ne!(
        obs.snapshot().snapshot_hash, hash_after_first,
        "Hash must change when clipboard content changes"
    );
}

#[test]
fn update_from_clipboard_same_content_returns_false() {
    let mut obs = ContextObserver::new();
    obs.update_from_clipboard_changed(&clipboard_payload("same text"));
    let changed = obs.update_from_clipboard_changed(&clipboard_payload("same text"));
    assert!(!changed, "Identical clipboard content must return false");
}

#[test]
fn update_from_clipboard_changed_truncates_at_max() {
    let mut obs = ContextObserver::new();
    // Build a string that exceeds CLIPBOARD_MAX_CHARS.
    let long_text: String = "x".repeat(CLIPBOARD_MAX_CHARS + 100);
    obs.update_from_clipboard_changed(&clipboard_payload(&long_text));

    let stored_len = obs.snapshot().clipboard_text.as_deref()
        .map(|t| t.chars().count())
        .unwrap_or(0);
    assert_eq!(
        stored_len, CLIPBOARD_MAX_CHARS,
        "Clipboard text must be truncated to CLIPBOARD_MAX_CHARS"
    );
}

#[test]
fn clipboard_summary_none_when_empty() {
    let obs = ContextObserver::new();
    assert!(
        obs.clipboard_summary().is_none(),
        "clipboard_summary must be None until a CLIPBOARD_CHANGED event arrives"
    );
}

#[test]
fn clipboard_summary_returns_stored_text() {
    let mut obs = ContextObserver::new();
    obs.update_from_clipboard_changed(&clipboard_payload("let answer = 42;"));
    assert_eq!(
        obs.clipboard_summary(),
        Some("let answer = 42;"),
        "clipboard_summary must return the stored clipboard text"
    );
}

#[test]
fn clipboard_parse_failure_returns_false_and_leaves_state_unchanged() {
    let mut obs = ContextObserver::new();
    obs.update_from_clipboard_changed(&clipboard_payload("original"));
    let changed = obs.update_from_clipboard_changed("not valid json {{{");
    assert!(!changed, "Parse failure must return false");
    assert_eq!(
        obs.clipboard_summary(),
        Some("original"),
        "State must be unchanged after parse failure"
    );
}
```

### Tests for `orchestrator.rs` context injection

Add to the existing orchestrator test block (near `context_summary_returns_some_after_app_focused_event`):

```rust
#[tokio::test]
async fn clipboard_context_injected_into_inference_messages() {
    // Verify that clipboard content from a CLIPBOARD_CHANGED event is injected
    // as a system message in prepare_messages_for_inference.
    let (tx, _rx) = mpsc::channel(4);
    let (action_tx, _action_rx) = mpsc::channel(4);
    let (generation_tx, _generation_rx) = mpsc::channel(4);
    let cfg = test_config();
    let mut orch = CoreOrchestrator::new(&cfg, new_session(), tx, action_tx, generation_tx)
        .await.expect("orchestrator must build");

    // Simulate CLIPBOARD_CHANGED arriving through handle_system_event.
    let clipboard_event = SystemEvent {
        r#type:   SystemEventType::ClipboardChanged as i32,
        payload:  r#"{"text":"fn fibonacci(n: u64) -> u64 { if n <= 1 { n } else { fibonacci(n-1) + fibonacci(n-2) } }"}"#.to_string(),
    };
    orch.handle_system_event(clipboard_event, new_trace())
        .await
        .expect("handle_system_event must succeed for CLIPBOARD_CHANGED");

    // Verify prepare_messages_for_inference includes the [Clipboard: ...] message.
    let messages = orch.prepare_messages_for_inference(&[]);

    // Verify presence and content.
    let clipboard_idx = messages.iter().position(|m| {
        m.role == "system" && m.content.starts_with("[Clipboard:")
    });
    assert!(
        clipboard_idx.is_some(),
        "prepare_messages_for_inference must inject [Clipboard: ...] when clipboard is set"
    );
    assert!(
        messages[clipboard_idx.unwrap()].content.contains("fibonacci"),
        "Clipboard injection must contain the actual clipboard text"
    );

    // Verify ordering: [Clipboard:] must appear after any [Context:] message
    // and before any [Memory:] message.
    // This test uses an empty recall slice so no [Memory:] message is present;
    // the ordering constraint is: clipboard is before history (non-system) messages.
    //
    // To test the full ordering contract (Clipboard before Memory), add a second
    // sub-test that passes a non-empty recall slice and verifies:
    //   clipboard_idx < memory_idx
    let history_idx = messages.iter().position(|m| m.role != "system");
    if let Some(hist) = history_idx {
        assert!(
            clipboard_idx.unwrap() < hist,
            "[Clipboard:] must appear before history (non-system) messages"
        );
    }
}

#[tokio::test]
async fn clipboard_injection_precedes_memory_injection() {
    // Verifies the ordering contract: [Clipboard:] < [Memory:] in the message list.
    // Both clipboard injection (Step 2b) and memory injection (Phase 21) use
    // take_while(|m| m.role == "system").count() at the moment of their insertion.
    // Memory runs second, so it appends AFTER the already-inserted clipboard message.
    let (tx, _rx) = mpsc::channel(4);
    let (action_tx, _action_rx) = mpsc::channel(4);
    let (generation_tx, _generation_rx) = mpsc::channel(4);
    let cfg = test_config();
    let mut orch = CoreOrchestrator::new(&cfg, new_session(), tx, action_tx, generation_tx)
        .await.expect("orchestrator must build");

    // Set clipboard content.
    let clipboard_event = SystemEvent {
        r#type:  SystemEventType::ClipboardChanged as i32,
        payload: r#"{"text":"clipboard content here"}"#.to_string(),
    };
    orch.handle_system_event(clipboard_event, new_trace())
        .await.expect("CLIPBOARD_CHANGED must succeed");

    // Inject a fake recall entry to force [Memory:] to appear.
    let recall = vec![crate::retrieval::store::MemoryEntry {
        content: "recalled fact".to_string(),
        source:  "memory".to_string(),
        score:   0.9,
    }];
    let messages = orch.prepare_messages_for_inference(&recall);

    let clipboard_idx = messages.iter().position(|m| {
        m.role == "system" && m.content.starts_with("[Clipboard:")
    }).expect("[Clipboard:] must be present");

    let memory_idx = messages.iter().position(|m| {
        m.role == "system" && m.content.starts_with("[Memory:")
    }).expect("[Memory:] must be present when recall is non-empty");

    assert!(
        clipboard_idx < memory_idx,
        "[Clipboard:] (idx {clipboard_idx}) must precede [Memory:] (idx {memory_idx})"
    );
}
```

**Note on test visibility:** `prepare_messages_for_inference` is currently a private
method on `CoreOrchestrator`. For this test to compile it must be made
`pub(crate)` — change the visibility declaration:

```rust
// Before:
fn prepare_messages_for_inference(&self, recall: &[crate::retrieval::store::MemoryEntry]) -> Vec<...>

// After:
pub(crate) fn prepare_messages_for_inference(&self, recall: &[crate::retrieval::store::MemoryEntry]) -> Vec<...>
```

This is the minimal visibility change — `pub(crate)` keeps it internal, which is
sufficient for tests in the same crate.

---

## 7. Execution Order

1. Add `SYSTEM_EVENT_TYPE_CLIPBOARD_CHANGED = 9` to `dexter.proto` — run `make proto`
2. Add three constants to `constants.rs`: `CLIPBOARD_MAX_CHARS`, `CLIPBOARD_MIN_CHARS`,
   `CLIPBOARD_POLL_INTERVAL_MS`
3. Edit `context_observer.rs`:
   a. Add `use crate::constants::CLIPBOARD_MAX_CHARS;`
   b. Add `clipboard_text: Option<String>` to `ContextSnapshot`
   c. Initialise `clipboard_text: None` in `ContextObserver::new()`
   d. Add `ClipboardPayload` private struct
   e. Add `update_from_clipboard_changed()` method
   f. Add `clipboard_summary()` method
   g. Update `compute_hash()` to hash `clipboard_text`
   h. Add 7 unit tests
4. Edit `orchestrator.rs`:
   a. Add `ClipboardChanged` arm to `handle_system_event`
   b. Add clipboard injection (Step 2b) to `prepare_messages_for_inference`
   c. Change `prepare_messages_for_inference` to `pub(crate)` for test visibility
   d. Add 2 orchestrator integration tests (presence + ordering)
5. Edit `EventBridge.swift`:
   a. Add three `private static let` constants
   b. Add `lastClipboardChangeCount` and `clipboardTimer` stored properties
   c. Add `startClipboardPolling()` and `stopClipboardPolling()` helpers
   d. Add `handleClipboardPoll()` handler
   e. Call `startClipboardPolling()` at the end of `start()`
   f. Call `stopClipboardPolling()` in `performStop()` before `stopHotkeyTap()`
6. `cd src/rust-core && cargo test` — all prior tests pass + ≥8 new clipboard tests pass
7. `cd src/swift && swift build` — 0 errors, 0 warnings from project code
8. Update `docs/SESSION_STATE.json` (phase → Phase 29 next, test counts)
9. Update `memory/MEMORY.md` (Phase 28 complete, Phase 29 current)

---

## 8. Acceptance Criteria

### Automated

- `cargo test` in `src/rust-core/`: all prior tests pass + ≥9 new clipboard tests pass
- `swift build` in `src/swift/`: 0 errors, 0 warnings from project code

### Manual

| # | Criterion | Verification |
|---|-----------|-------------|
| 1 | Clipboard detected | Copy any text → `[EventBridge] Clipboard changed: N chars` in Swift console (or equivalent trace log) |
| 2 | Dexter sees clipboard | Copy a function, ask "what does this do?" → Dexter answers about the copied function without being told what it is |
| 3 | Short text filtered | Copy a single word → no CLIPBOARD_CHANGED event sent (< 5 chars) |
| 4 | Truncation | Copy a > 4,000 char file → Rust log shows `char_count=4000` (or capped value) |
| 5 | Clipboard persists across turns | Copy code, ask two questions about it → both answers reference the clipboard content |
| 6 | New clipboard replaces old | Copy code A, then copy code B, ask "explain this" → Dexter explains B, not A |
| 7 | Non-text clipboard ignored | Copy an image (⌘⇧4 screenshot to clipboard) → no CLIPBOARD_CHANGED event |
| 8 | Existing context unaffected | Clipboard context injected alongside `[Context: ...]` — asking about the focused app still works |
| 9 | Empty clipboard state clean | Fresh session with no copies yet → `clipboard_summary()` returns nil, no `[Clipboard:]` in messages |
| 10 | Polling stops on session end | Verify `clipboardTimer` is nil after `EventBridge.stop()` (no timer leak on reconnect) |

---

## 9. Key Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| `NSPasteboard.string(forType: .string)` returns nil for apps that copy structured data (Numbers cells, Finder files) | Guarded by `guard let text = pb.string(forType: .string)` — nil pasteboard contents are silently ignored. Only plain text clipboard changes produce events. |
| Timer leaks on reconnect (session disconnects then reconnects without full app restart) | `performStop()` calls `stopClipboardPolling()` which invalidates the timer. `start()` always calls `startClipboardPolling()` fresh. `Timer.scheduledTimer` + `RunLoop.main.add` is safe to call multiple times as long as the previous timer was invalidated. |
| Clipboard injection bloats context on large copies (code + retrieval both active) | Both are bounded: clipboard at 4,000 chars, retrieval context at 4,000 chars. Combined worst case adds ~1,200 tokens to the system block. Primary model (`mistral-small:24b`) has a 32k token context window — plenty of headroom. FAST model (`qwen3:8b`) at 32k also fine. |
| `prepare_messages_for_inference` ordering bug: [Clipboard] lands after [Memory] instead of before | The clipboard injection uses `take_while(|m| m.role == "system").count()` at the point of its insertion. At that point, Memory hasn't been inserted yet — so `insert_at` correctly points to the end of the pre-Memory system block. Memory's own `take_while` then runs on the already-extended `messages`, appending after clipboard. The `clipboard_injection_precedes_memory_injection` test verifies this by index assertion (`clipboard_idx < memory_idx`) with a non-empty recall slice that forces both messages to appear. |
| Very large clipboard content combined with long conversation history approaches context window | 4,000 + 4,000 + system prompt (~1,500) + conversation history = bounded. At extreme conversation lengths (50+ turns), the ConversationContext truncation (established in Phase 5) kicks in and reduces history. Clipboard + retrieval are not subject to that truncation — they are fresh per-request injections. This is the correct tradeoff: current context takes priority over old turns. |
| `pub(crate)` change on `prepare_messages_for_inference` exposes internal method | `pub(crate)` is narrower than `pub` — only accessible within the `rust-core` crate, never by external callers. The method was already used by tests (in the same file, same crate). This is a visibility declaration that aligns with reality, not an expansion of the public API surface. |
| Clipboard polling fires during SPEAKING / THINKING state and triggers an unnecessary log | The `info!` log in `ClipboardChanged` arm is gated on `changed` — duplicate content (operator copying the same text twice) is suppressed. During rapid typing with no clipboard changes, no log is emitted. The timer fires unconditionally but produces no observable effect unless changeCount changes. |
