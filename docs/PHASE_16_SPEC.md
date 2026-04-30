# Phase 16 — Context-Aware Inference + Hotkey Activation
## Spec version 1.0 — Session 016, 2026-03-13

> **Status:** Current phase.
> This document is the authoritative implementation guide for Phase 16.
> All architectural decisions are locked. Implement exactly as written.

---

## 1. What Phase 16 Delivers

Two tightly related capabilities that close the most important open loops in the system:

| Deliverable | Why Now |
|-------------|---------|
| **Context snapshot injection into inference** | `context_summary()` has been `#[allow(dead_code)]` since Phase 7. Dexter knows what the operator is looking at but never tells the model. This is the highest-impact single-line change in the codebase. |
| **Global hotkey activation (Ctrl+Shift+Space)** | VoiceCapture is always-on (Phase 13). The only missing piece is an OS-level attention signal: pressing a hotkey transitions the entity to LISTENING, giving the operator a clear visual cue before speaking. |

**What this does NOT include:** Proactive context-triggered responses (Dexter initiating
without being asked) — that is Phase 17. Phase 16 makes Dexter context-aware when
*responding*; Phase 17 will make him context-aware when *initiating*.

**Test count target:** 180 Rust passing (currently 178).

---

## 2. What Already Exists (Do Not Rebuild)

| Component | Phase | Current State |
|-----------|-------|--------------|
| `ContextObserver::context_summary()` | 7 | ✅ Implemented — `#[allow(dead_code)]` because never called. Wires in Step 1. |
| `ContextObserver::update_from_app_focused()` | 7 | ✅ Called from `handle_system_event()` |
| VoiceCapture always-on (VAD-gated) | 13 | ✅ Starts in `runSession()`, runs for session lifetime |
| `EventBridge.sendSystemEvent()` | 7 | ✅ Private helper; hotkey tap calls it with `.hotkeyActivated` |
| `axFocusCallback` (module-level C function pattern) | 7 | ✅ Exact same pattern for `hotkeyTapCallback` |
| `handle_system_event()` exhaustive match | 7 | ✅ Add `HotkeyActivated` case in Step 3 |
| `send_state()` / `EntityState::Listening` | 12 | ✅ Used throughout — hotkey handler calls it directly |

---

## 3. Architectural Decisions

### 3.1 Context injection position: index 1, before retrieval

The message list after personality injection is:
```
[0] personality system message  ("You are Dexter...")
[1..N] conversation history
[N+1] current user turn
```

Phase 16 injects context at index 1 — immediately after personality, before everything
else. Retrieval injection (Phase 9) currently inserts at index 1 and shifts. After
Phase 16, retrieval shifts to index 2 when context was injected:

```
[0] personality system message
[1] context snapshot:  "[Context: Xcode — Source Editor: func parseVmStat]"
[2] retrieval context: "[Retrieved context]\n..." (if triggered)
[3..N] conversation history
[N+1] current user turn
```

**Why this ordering:** The model should understand the operator's workspace context
before receiving retrieved facts. Context establishes the interpretive frame; retrieval
provides supporting data.

**When context is absent:** If `context_summary()` returns `None` (no app focused,
screen locked), no message is inserted. The retrieval index stays at 1. No change in
behavior from current state.

### 3.2 Hotkey uses `SystemEvent`, not `UIAction`

The hotkey is a keyboard event captured by CGEventTap at the OS level — the same
observation layer as screen lock/unlock and app focus. `SystemEvent` is the correct
semantic bucket. `UIAction` is reserved for direct interactions with Dexter's window
(dismiss, drag, resize).

Adding a new `SystemEventType` constant (= 7) requires updating the proto and
regenerating code. This is a small change. The Rust `handle_system_event()` match
already covers all 6 current variants — the compiler will enforce adding a case for
the new variant (non-exhaustive match error), which prevents forgetting to handle it.

### 3.3 Hotkey is an attention signal, not a mic toggle

VoiceCapture runs continuously with VAD throughout the session. It is always capturing.
The hotkey does not start or stop audio capture — it sends `EntityState::Listening`
to the Swift entity, giving the operator a visual cue that their next utterance will
be processed by Dexter. The VAD-detected utterance is then processed through the
normal STT → orchestrator → response pipeline.

**Why not toggle VoiceCapture?** Phase 13 designed VoiceCapture as always-on. It is
already consuming minimal CPU at rest (VAD-gated, only buffers during speech). Adding
a toggle would require a start/stop protocol between DexterClient and VoiceCapture,
plus handling the case where the operator speaks before pressing the hotkey. Always-on
is the correct default for an always-present AI entity.

### 3.4 Hotkey combination: Ctrl+Shift+Space

- Avoids Option+Space (inserts non-breaking space in text editors — would break typing)
- Avoids Cmd+Space (Spotlight) and Cmd+Shift+Space (system reserved)
- Ctrl+Shift+Space is assigned by very few macOS apps
- The event tap **consumes** the keypress (returns nil from callback) — so it never
  reaches the focused app. This prevents accidental character insertion.

**Configuration:** The hotkey modifier is hardcoded for Phase 16. Making it
configurable via `default.yaml` is Phase 17+.

### 3.5 CGEventTap callback: module-level C function, not closure

`CGEventTapCreate` takes a `CGEventTapCallBack` which is a C function pointer.
In Swift 6, closures that capture context cannot be used as C function pointers.
The correct pattern — already established for `axFocusCallback` in Phase 7 — is a
module-level `private func` that receives `self` via the `refcon` parameter using
`Unmanaged<EventBridge>.fromOpaque(refcon)`. Phase 16 adds a second module-level
function `hotkeyTapCallback` using the identical pattern.

---

## 4. Acceptance Criteria

| # | Criterion | How to Verify |
|---|-----------|--------------|
| AC-1 | When app is focused (e.g., Xcode), Dexter's inference prompt contains a `[Context: Xcode]` system message | Integration test or manual: run Dexter, focus Xcode, ask a question, check structured logs for context message |
| AC-2 | When no app is focused, no context system message is added | Unit test `context_summary_returns_none_for_fresh_orchestrator` |
| AC-3 | Context message appears at index 1 (after personality, before retrieval) | Code review of orchestrator insertion order |
| AC-4 | `context_summary()` has no `#[allow(dead_code)]` annotation | `grep -n allow.dead_code src/rust-core/src/context_observer.rs` |
| AC-5 | `dexter.proto` contains `SYSTEM_EVENT_TYPE_HOTKEY_ACTIVATED = 7` | `grep -n HOTKEY src/shared/proto/dexter.proto` |
| AC-6 | EventBridge registers a CGEventTap in `start()` and removes it in `stop()` | Code review; no tap leak on reconnect |
| AC-7 | Pressing Ctrl+Shift+Space sends a `SYSTEM_EVENT_TYPE_HOTKEY_ACTIVATED` SystemEvent | Manual test: press hotkey, observe structured log |
| AC-8 | Ctrl+Shift+Space keypress is consumed (does not reach focused app) | Manual test: type in a text field, press hotkey, no spurious character inserted |
| AC-9 | Rust orchestrator receives `HOTKEY_ACTIVATED` event → sends `EntityStateChange(LISTENING)` to Swift | Unit test `handle_system_event_hotkey_activated_transitions_to_listening` |
| AC-10 | AnimatedEntity visually shows LISTENING state after hotkey press | Manual test: press hotkey, confirm disc animation changes (color + speed) |
| AC-11 | CGEventTapCreate failure is non-fatal (logged as warning, session continues) | Code review: `guard let tap = ... else { logger.warning(...); return }` |
| AC-12 | `cargo test` produces ≥ 180 passing tests, 0 failures | `make test` |
| AC-13 | `cargo test` produces 0 compiler warnings | `make test` output |
| AC-14 | `swift build` produces 0 project-code warnings | `cd src/swift && swift build` |
| AC-15 | `uv run pytest` produces 19/19 passing | `make test-python` |
| AC-16 | `make proto` succeeds (regenerates Swift + Rust from updated proto) | `make proto` |

---

## 5. What Is NOT in Scope

- Proactive context-triggered responses — Dexter initiating conversation based on context change (Phase 17)
- Configurable hotkey combination — hardcoded to Ctrl+Shift+Space; config via `default.yaml` is Phase 17+
- "Dexter, listen" wake phrase — Phase 17+
- Hotkey visual feedback in OS (menu bar icon, etc.) — out of scope (Dexter has no menu bar presence by design)
- Context snapshot persistence across sessions — memory is Phase 17; context is per-session

---

## 6. Implementation Guide

Implement in exactly this order. Run `cargo test` after each Rust step.

---

### Step 1: Context snapshot injection

**File:** `src/rust-core/src/context_observer.rs`

Remove the `#[allow(dead_code)]` annotation from `context_summary()`:

```rust
// BEFORE:
#[allow(dead_code)] // Phase 8+ — inference context injection
pub fn context_summary(&self) -> Option<String> {

// AFTER:
pub fn context_summary(&self) -> Option<String> {
```

**File:** `src/rust-core/src/orchestrator.rs`

In `handle_text_input()`, find the comment block around step 4 (personality injection)
and step 4b (retrieval injection). Insert a new step between them:

```rust
// 4. Apply personality — prepend system prompt to message list.
let mut messages = self.personality.apply_to_messages(self.context.messages());

// Step 4b. [Phase 16] Context snapshot injection.
//
// If the operator is focused on an app, inject a brief system message telling
// the model what they're looking at. This is the primary mechanism by which
// Dexter remains contextually aware of the operator's current task.
//
// Injected at index 1: immediately after the personality system message (index 0),
// before retrieval context (index 2 if retrieval fires, otherwise absent) and
// before conversation history.
//
// When context_summary() returns None (no app focused, screen locked, early
// in session before first APP_FOCUSED event), this step is a no-op.
let context_injected = if let Some(summary) = self.context_observer.context_summary() {
    messages.insert(1, crate::inference::engine::Message {
        role:    "system".to_string(),
        content: format!("[Context: {summary}]"),
    });
    debug!(
        session = %self.session_id,
        context = %summary,
        "Context snapshot injected into inference request"
    );
    true
} else {
    false
};

// Step 4c. [Phase 9, index updated in Phase 16] Inject retrieval context.
//
// Must come AFTER context injection so the ordering is:
//   [0] personality   [1] context   [2] retrieval   [3..N] conversation
// If context was not injected, retrieval still lands at index 1 (original behavior).
if let Some(injection) = pre_retrieval_injection {
    let retrieval_idx = if context_injected { 2 } else { 1 };
    debug_assert!(
        messages.len() > retrieval_idx,
        "message list must have at least personality + context entries before retrieval insertion"
    );
    messages.insert(retrieval_idx, crate::inference::engine::Message {
        role:    "system".to_string(),
        content: injection,
    });
    info!(session = %self.session_id, "Pre-retrieval context injected into generation request");
}
```

**Replace the existing Phase 9 retrieval insertion block** (which currently does
`messages.insert(1, ...)` unconditionally) with the `Step 4c` block above.
Remove the old `debug_assert!` that checked `!messages.is_empty()` — the new
`debug_assert!` supersedes it.

**Update the step-numbered comment block** at the top of `handle_text_input()`:

```rust
//   4.  Apply personality (PersonalityLayer → prepend system prompt)
//   4b. [Phase 16] Inject context snapshot (current app + element, if known)
//   4c. [Phase 9]  Inject retrieval context as second system message (if retrieved)
```

**Add 1 unit test** to the `#[cfg(test)]` block inside `orchestrator.rs` (not a
separate integration test file — private fields like `context_observer` are
accessible from within the same file's test module):

```rust
#[tokio::test]
async fn context_summary_returns_some_after_app_focused_event() {
    // Verifies that the data path from system event → context snapshot → summary
    // is intact, confirming context injection has a non-None value to inject
    // when the operator is actively using an app.
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, _rx) = make_orchestrator(tmp.path());

    // Before any event — no app focused.
    assert!(
        orch.context_observer.context_summary().is_none(),
        "Fresh orchestrator must have no context summary"
    );

    // Simulate APP_FOCUSED for Xcode.
    let payload = r#"{"bundle_id":"com.apple.dt.Xcode","name":"Xcode"}"#;
    let evt = SystemEvent {
        r#type:  crate::ipc::proto::SystemEventType::AppFocused.into(),
        payload: payload.to_string(),
    };
    orch.handle_system_event(evt, new_trace()).await.unwrap();

    // After the event — context summary should contain the app name.
    let summary = orch.context_observer.context_summary();
    assert!(summary.is_some(), "context_summary must return Some after APP_FOCUSED");
    assert!(
        summary.unwrap().contains("Xcode"),
        "context_summary must contain the focused app name"
    );
}
```

**After Step 1: `cargo test` → 179 passing (178 + 1 new), 0 warnings.**

---

### Step 2: Proto extension — add `HOTKEY_ACTIVATED` system event

**File:** `src/shared/proto/dexter.proto`

In the `SystemEventType` enum, add after `SYSTEM_EVENT_TYPE_SCREEN_UNLOCKED`:

```protobuf
enum SystemEventType {
  SYSTEM_EVENT_TYPE_UNSPECIFIED        = 0;
  SYSTEM_EVENT_TYPE_CONNECTED          = 1;
  SYSTEM_EVENT_TYPE_APP_FOCUSED        = 2;
  SYSTEM_EVENT_TYPE_APP_UNFOCUSED      = 3;
  SYSTEM_EVENT_TYPE_SCREEN_LOCKED      = 4;
  SYSTEM_EVENT_TYPE_AX_ELEMENT_CHANGED = 5;
  SYSTEM_EVENT_TYPE_SCREEN_UNLOCKED    = 6;
  SYSTEM_EVENT_TYPE_HOTKEY_ACTIVATED   = 7;  // Global hotkey (Ctrl+Shift+Space) pressed — payload: "{}"
}
```

Regenerate Swift and Rust proto artifacts:

```bash
make proto
```

**After `make proto`:** `swift build` will show a warning/error in `DexterClient.swift`
if `switch change.type` is exhaustive — add the `.hotkeyActivated` case there if Swift
requires it (see Step 4). In Rust, the `handle_system_event` match will produce a
non-exhaustive warning until Step 3.

---

### Step 3: Orchestrator — handle `HOTKEY_ACTIVATED`

**File:** `src/rust-core/src/orchestrator.rs`

In `handle_system_event()`, add the `HotkeyActivated` branch to the exhaustive match.
The current match covers 6 variants (`Connected`, `AppFocused`, `AppUnfocused`,
`ScreenLocked`, `AxElementChanged`, `ScreenUnlocked`). Add the 7th:

```rust
// After the ScreenUnlocked arm:

// Phase 16: Global hotkey pressed — transition entity to LISTENING so the
// operator has a clear visual cue their next utterance will be processed.
// VoiceCapture is always-on (Phase 13); this is an attention signal only.
SystemEventType::HotkeyActivated => {
    info!(session = %self.session_id, trace_id = %trace_id, "Global hotkey activated");
    self.send_state(EntityState::Listening, &trace_id).await?;
}
```

Also update the stale comment on `handle_ui_action()`:

```rust
// BEFORE (stale since Phase 11 shipped):
/// Handle a `UIAction` (dismiss / drag / resize).
///
/// Phase 11 (Swift Shell) will process position and size changes. For now: log
/// the action with structured fields, return Ok.

// AFTER:
/// Handle a `UIAction` (dismiss / drag / resize).
///
/// Currently all UIAction types are logged and acknowledged. Phase 16 uses
/// SystemEvent for hotkey activation. Future phases may add UIAction handlers
/// for DRAG (persist window position to state) and RESIZE.
```

**Add 1 unit test:**

```rust
#[tokio::test]
async fn handle_system_event_hotkey_activated_transitions_to_listening() {
    // Verifies the orchestrator emits EntityState::Listening to the Swift UI
    // when the global hotkey SystemEvent is received.
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, mut rx) = make_orchestrator(tmp.path());

    // Drain the CONNECTED event emitted during make_orchestrator (if any).
    // The test channel is unbounded — drain any already-queued items.
    while rx.try_recv().is_ok() {}

    let evt = SystemEvent {
        r#type:  crate::ipc::proto::SystemEventType::HotkeyActivated.into(),
        payload: "{}".to_string(),
    };
    orch.handle_system_event(evt, new_trace()).await.unwrap();

    // The orchestrator should have sent EntityState::Listening.
    let server_event = rx.try_recv()
        .expect("orchestrator must emit a server event after HOTKEY_ACTIVATED")
        .expect("server event must be Ok");

    match server_event.event {
        Some(crate::ipc::proto::server_event::Event::EntityState(ref change)) => {
            assert_eq!(
                change.state,
                crate::ipc::proto::EntityState::Listening as i32,
                "HOTKEY_ACTIVATED must transition entity to LISTENING"
            );
        }
        other => panic!("Expected EntityStateChange(Listening), got {:?}", other),
    }
}
```

**After Step 3: `cargo test` → 180 passing (179 + 1 new), 0 warnings.**

---

### Step 4: EventBridge — CGEventTap hotkey listener

**File:** `src/swift/Sources/Dexter/Bridge/EventBridge.swift`

#### 4a. Module-level C callback function

Add a new module-level free function immediately after `axFocusCallback`, following
the exact same `@unchecked Sendable` / `Unmanaged<EventBridge>` pattern:

```swift
// ── Hotkey CGEventTap C callback ──────────────────────────────────────────────
//
// CGEventTapCreate takes a CGEventTapCallBack — a C function pointer.
// Swift 6 forbids capturing context in C function pointer closures, so this must
// be a module-level free function (same pattern as axFocusCallback above).
// `self` is passed via `refcon` as an Unmanaged<EventBridge> pointer.
//
// Matches CGEventTapCallBack:
//   (CGEventTapProxy, CGEventType, CGEvent, UnsafeMutableRawPointer?) -> Unmanaged<CGEvent>?
//
// CGEventRef bridges to Swift as a non-optional CGEvent (a class type). The event
// parameter is therefore CGEvent, not CGEvent?. Mirror the axFocusCallback pattern:
// only refcon can be nil (when the tap fires before self is set, which cannot happen
// in practice — but guard defensively regardless).
//
// Returns nil for the hotkey chord (consumes the event — prevents it reaching
// the focused app). Returns the event unchanged for all other keypresses.
private func hotkeyTapCallback(
    proxy:  CGEventTapProxy,
    type:   CGEventType,
    event:  CGEvent,
    refcon: UnsafeMutableRawPointer?
) -> Unmanaged<CGEvent>? {
    guard let refcon else { return Unmanaged.passRetained(event) }
    let bridge = Unmanaged<EventBridge>.fromOpaque(refcon).takeUnretainedValue()
    if bridge.isHotkeyEvent(event) {
        bridge.handleHotkeyActivated()
        return nil   // consume: do not forward to focused app
    }
    return Unmanaged.passRetained(event)
}
```

#### 4b. EventBridge stored properties

Add two stored properties to the EventBridge class (alongside existing observer tokens):

```swift
// ── Hotkey CGEventTap (main thread only — see threading contract) ─────────────
private var hotkeyTap:             CFMachPort?
private var hotkeyRunLoopSource:   CFRunLoopSource?
```

#### 4c. `start()` and `stop()` wiring

In `start()`, after `registerScreenLockObservers()` and AXObserver setup, add:

```swift
startHotkeyTap()
```

In `stop()`, after cleaning up AX and NSWorkspace observers, add:

```swift
stopHotkeyTap()
```

#### 4d. `startHotkeyTap()` and `stopHotkeyTap()`

```swift
/// Register a CGEventTap to listen for the global activation hotkey (Ctrl+Shift+Space).
///
/// Requires Accessibility permission (kTCCServiceAccessibility) — already checked by
/// permissions.sh and asserted at EventBridge startup. If CGEventTapCreate fails
/// (e.g., in a context where Accessibility was revoked), logs a warning and continues
/// in degraded mode (voice still works, hotkey is unavailable).
///
/// The tap is installed at .cgSessionEventTap with .headInsertEventTap so it sees
/// all key events before they reach the focused application. The callback returns nil
/// for the hotkey chord to consume it; all other events are passed through unchanged.
private func startHotkeyTap() {
    let eventMask = CGEventMask(1 << CGEventType.keyDown.rawValue)
    let bridge    = Unmanaged.passUnretained(self).toOpaque()

    guard let tap = CGEventTapCreate(
        .cgSessionEventTap,
        .headInsertEventTap,
        .defaultTap,
        eventMask,
        hotkeyTapCallback,   // module-level C function — see above
        bridge
    ) else {
        logger.warning("CGEventTapCreate failed — Ctrl+Shift+Space hotkey unavailable (check Accessibility permission)")
        return
    }

    let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
    CFRunLoopAddSource(CFRunLoopGetMain(), source, .commonModes)
    CGEventTapEnable(tap, true)

    hotkeyTap           = tap
    hotkeyRunLoopSource = source
}

private func stopHotkeyTap() {
    if let tap = hotkeyTap, let source = hotkeyRunLoopSource {
        CGEventTapEnable(tap, false)
        CFRunLoopRemoveSource(CFRunLoopGetMain(), source, .commonModes)
        CFMachPortInvalidate(tap)
    }
    hotkeyTap           = nil
    hotkeyRunLoopSource = nil
}
```

#### 4e. `isHotkeyEvent(_:)` and `handleHotkeyActivated()`

```swift
/// Returns true if `event` matches the Dexter activation hotkey (Ctrl+Shift+Space).
///
/// Hotkey: Ctrl+Shift+Space
///   keyCode 49 = kVK_Space
///   Required flags: .maskControl + .maskShift
///   Excluded flags: .maskCommand (prevents conflict with Cmd+Shift+Space)
///                   .maskAlternate (prevents conflict with Option+Shift+Space)
///
/// The choice of Ctrl+Shift+Space avoids:
///   - Option+Space (inserts non-breaking space in text editors)
///   - Cmd+Space (Spotlight)
///   - Cmd+Shift+Space (system reserved in some macOS versions)
private func isHotkeyEvent(_ event: CGEvent) -> Bool {
    let keyCode = event.getIntegerValueField(.keyboardEventKeycode)
    let flags   = event.flags
    return keyCode == 49
        && flags.contains(.maskControl)
        && flags.contains(.maskShift)
        && !flags.contains(.maskCommand)
        && !flags.contains(.maskAlternate)
}

/// Send a HOTKEY_ACTIVATED SystemEvent to the Rust orchestrator.
/// Called from `hotkeyTapCallback` on the main run loop.
private func handleHotkeyActivated() {
    logger.debug("Global hotkey activated — sending SYSTEM_EVENT_TYPE_HOTKEY_ACTIVATED")
    sendSystemEvent(.hotkeyActivated)
}
```

#### 4f. DexterClient.swift — exhaustiveness

After `make proto`, if the Swift compiler requires an exhaustive match on
`SystemEventType` anywhere in `DexterClient.swift`, add:

```swift
case .hotkeyActivated:
    break  // handled on Rust side; Swift just observes the entity state change
```

The entity transitions to LISTENING via the `EntityStateChange` server event that
Rust sends in response — `DexterClient` already handles `EntityStateChange` in Phase 12.
No new DexterClient cases needed beyond the exhaustiveness guard above.

**After Step 4: `swift build` → Build complete, 0 project-code warnings.**

---

### Step 5: Full regression

```bash
cargo test            # 180 passing, 0 failed, 0 warnings
cd src/swift && swift build  # Build complete, 0 project-code warnings
uv run pytest -q      # 19 passed
make smoke            # all three layers pass
```

Run `make test-e2e` only if Ollama is available with the required models.

For manual validation (requires `make run`):

1. Start the full system: `make run`
2. Focus Xcode or VS Code
3. Ask a question via voice — check structured logs for `"Context snapshot injected into inference request"` with the app name
4. Press Ctrl+Shift+Space — the animated entity should flash to the LISTENING state (green, faster pulse)
5. Speak — next utterance is processed normally
6. Check that the keypress did not insert a character in the focused text field

---

## 7. Known Pitfalls

**Pitfall: Retrieval insertion index must shift when context was injected**

The existing Phase 9 code does `messages.insert(1, ...)` for retrieval. After Phase 16
adds context injection at index 1, retrieval must move to index 2. Failing to update
this would interleave context and retrieval in reverse order. The spec uses a
`context_injected: bool` flag to conditionally set `retrieval_idx` — see Step 1.

**Pitfall: `debug_assert!` on message list length before retrieval insertion**

The original Phase 9 `debug_assert!(!messages.is_empty())` is superseded. Replace it
with the new `debug_assert!(messages.len() > retrieval_idx, ...)` which guards the
specific insertion index. If `context_injected = true` and `retrieval_idx = 2`, the
list must have at least 3 elements (personality + context + at least one history turn)
for the insertion to be valid. In practice, any conversation has at least one user turn
at this point.

**Pitfall: `hotkeyTapCallback` is a free function — do not make it a method**

Swift 6 requires C function pointers to be module-level free functions with no captured
state. Do not attempt to use `self.hotkeyTapCallback` or a closure — the compiler will
reject it. The `axFocusCallback` function at the top of `EventBridge.swift` is the
established pattern; `hotkeyTapCallback` must follow the same form.

**Pitfall: macOS 26 SDK replaced `CGEventTapCreate`/`CGEventTapEnable` free functions**

On macOS 26 (Swift 6), the CoreGraphics C API free functions are errors:
```
'CGEventTapCreate' has been replaced by 'CGEvent.tapCreate(tap:place:options:eventsOfInterest:callback:userInfo:)'
'CGEventTapEnable' has been replaced by 'CGEvent.tapEnable(tap:enable:)'
```
Use the Swift class method equivalents:
```swift
CGEvent.tapCreate(tap: .cgSessionEventTap, place: .headInsertEventTap,
                  options: .defaultTap, eventsOfInterest: eventMask,
                  callback: hotkeyTapCallback, userInfo: bridge)
CGEvent.tapEnable(tap: tap, enable: true)
```
`CFMachPortCreateRunLoopSource`, `CFRunLoopAddSource/RemoveSource`, and `CFMachPortInvalidate` remain available as C-level CF functions and do not need replacement.

**Pitfall: `event` parameter is `CGEvent`, not `CGEvent?` — match `axFocusCallback` signature exactly**

`CGEventRef` (the C type) bridges to Swift as a non-optional class reference `CGEvent`.
The `CGEventTapCallBack` typedef in Swift is therefore:
```
(CGEventTapProxy, CGEventType, CGEvent, UnsafeMutableRawPointer?) -> Unmanaged<CGEvent>?
```
Using `CGEvent?` for the event parameter causes a type mismatch (compile error or
silent wrong bridging depending on Swift version). The correct guard is:
```swift
guard let refcon else { return Unmanaged.passRetained(event) }
```
Not `guard let event, let refcon` — `event` is non-optional and cannot be nil-checked.
Verify this by looking at the existing `axFocusCallback` signature in `EventBridge.swift`
and applying the same parameter optionality to `hotkeyTapCallback`.

**Pitfall: CGEventTap requires Accessibility permission**

`CGEventTapCreate` returns nil if the process lacks Accessibility permission
(`kTCCServiceAccessibility`). `permissions.sh` already checks this, but the
`startHotkeyTap()` implementation must handle nil gracefully (`guard let tap = ...
else { logger.warning(...); return }`). The session must not fail if the hotkey tap
can't be created — voice still works, hotkey is just unavailable.

**Pitfall: `HotkeyActivated` in Rust match — non-exhaustive compiler error**

After `make proto` regenerates `dexter.v1.rs`, the `handle_system_event()` match in
`orchestrator.rs` will produce a non-exhaustive match error if the new variant is
not handled. This is intentional — the compiler enforces handling. Add the
`HotkeyActivated` arm before running `cargo test`.

**Pitfall: `SYSTEM_EVENT_TYPE_HOTKEY_ACTIVATED` is the proto field name in Rust**

In Rust, proto enum variants generated by prost are accessible as:
`crate::ipc::proto::SystemEventType::HotkeyActivated` (converted to PascalCase).
The comparison in the match uses the integer discriminant via `.into()` or direct
enum comparison. Follow the existing pattern in `handle_system_event()`:
```rust
let evt_type = SystemEventType::try_from(event.r#type).unwrap_or_default();
match evt_type {
    SystemEventType::HotkeyActivated => { ... }
    // ...
}
```

**Pitfall: `context_summary()` format does not include brackets — the format string does**

`context_summary()` returns `"Xcode — Source Editor: func parseVmStat"` (no brackets).
The orchestrator wraps it: `format!("[Context: {summary}]")`. The square brackets
signal to the model that this is injected metadata, not user content. Do not add
brackets inside `context_summary()` itself — keeping the format minimal in the method
allows future callers to use the summary string without bracketing.

---

## 8. Acceptance Criteria Sign-Off Checklist

```
[ ] AC-1  Context snapshot injected into inference when app is focused
[ ] AC-2  No context message when no app focused (context_summary returns None)
[ ] AC-3  Context message at correct index (after personality, before retrieval)
[ ] AC-4  #[allow(dead_code)] removed from context_summary()
[ ] AC-5  proto contains SYSTEM_EVENT_TYPE_HOTKEY_ACTIVATED = 7
[ ] AC-6  EventBridge registers/deregisters CGEventTap in start()/stop()
[ ] AC-7  Ctrl+Shift+Space sends HOTKEY_ACTIVATED SystemEvent
[ ] AC-8  Ctrl+Shift+Space consumed (does not reach focused app)
[ ] AC-9  Orchestrator sends EntityState::Listening on HOTKEY_ACTIVATED
[ ] AC-10 AnimatedEntity shows LISTENING state after hotkey press (manual)
[ ] AC-11 CGEventTapCreate failure is non-fatal (logged warning, session continues)
[ ] AC-12 cargo test ≥ 180 passing, 0 failed
[ ] AC-13 cargo test 0 warnings
[ ] AC-14 swift build 0 project-code warnings
[ ] AC-15 uv run pytest 19/19
[ ] AC-16 make proto succeeds
```
