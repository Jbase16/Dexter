# Phase 25 Implementation Plan: Conversation HUD

## Context

Phase 25 fills the most significant remaining UX gap in the system: **the operator cannot
see what Dexter is saying.** Text responses from Rust arrive at `DexterClient` and are
currently discarded with a `print()` call. The only output channel is TTS audio, which
means:

- Anything missed mid-utterance is gone
- Long technical responses (commands, filenames, code) can't be reviewed
- When TTS is unavailable, the system produces no visible output at all
- There is no keyboard input path — voice is the only way to talk to Dexter

Phase 25 delivers a floating **Conversation HUD**: a second `NSPanel` that appears to
the left of the entity window, streams response text as tokens arrive, shows the
operator's input (voice transcript or typed text), and auto-dismisses after a
configurable delay. It also adds a text input field as a first-class keyboard
alternative to voice, wired all the way through to inference.

**No Rust changes. No proto changes.** All work is in Swift.

---

## Architecture Decisions

### Separate window (not expanding the entity window)

The entity window hosts a `CAMetalLayer` via `MTKView`. Adding `NSTextView` as a sibling
inside the same `NSPanel` content view creates compositing complications — `NSTextView`
draws through `CoreGraphics` while the Metal layer bypasses it entirely. A second
`NSPanel` side-steps this entirely: each window owns its rendering layer independently.

The HUD is logically owned by `FloatingWindow` (it's a property) but is a distinct
`NSPanel` at the same `.screenSaver` window level. Both windows move together:
`FloatingWindow.windowDidMove` calls `hud.follow(entityFrame:)`.

### AppKit throughout (no SwiftUI)

All existing Swift code is pure AppKit. SwiftUI text views in floating panels at
`.screenSaver` level have known edge-case behavior with keyboard focus and vibrancy
compositing. Staying on AppKit keeps the codebase consistent and avoids the
`NSHostingView` wrapper seam.

### NSVisualEffectView background

`.hudWindow` material + `.behindWindow` blending gives the standard macOS HUD look
(dark, translucent) without hard-coding colors. Adapts correctly to varied desktop
backgrounds. Corner radius 12pt.

### Fixed height, scrollable text

The HUD appears at a fixed size (`HUD_WIDTH × HUD_HEIGHT`, see constants). The text
area is an `NSScrollView`/`NSTextView` pair that scrolls when content overflows. This
avoids dynamic resizing logic and the layout complications of growing a frameless panel
upward while keeping it bottom-aligned to the entity.

### Typed input does not steal focus

`HUDWindow` uses `[.borderless, .nonactivatingPanel]` — the same styleMask as
`FloatingWindow`. The input field is inactive until the operator explicitly clicks it.
`acceptsFirstMouse(for:)` returns `true` so the first click both activates the field
and focuses it without requiring a second click.

---

## Constants

Defined once as an `internal enum C` at the **top of `HUDWindow.swift`**, before the
`HUDWindow` class declaration. `internal` (the Swift default — no access modifier
keyword needed) makes it visible to `HUDTextView.swift` and `HUDInputField.swift`
across the module. Do **not** use `private`, which would make it file-scoped and
invisible to the other two HUD files that also reference `C.responseFont` etc.

```swift
enum C {   // internal — no modifier. Visible to HUDTextView.swift + HUDInputField.swift
    static let width:         CGFloat       = 360
    static let height:        CGFloat       = 300
    static let gap:           CGFloat       = 12     // horizontal gap from entity
    static let cornerRadius:  CGFloat       = 12
    static let fadeDuration:  TimeInterval  = 0.20
    static let dismissDelay:  TimeInterval  = 10.0   // seconds after is_final
    static let inputHeight:   CGFloat       = 34
    static let inputPadding:  CGFloat       = 8
    static let responseFont   = NSFont.systemFont(ofSize: 14)
    static let operatorFont   = NSFont.systemFont(ofSize: 13, weight: .medium)
    static let responseColor  = NSColor.white
    static let operatorColor  = NSColor.white.withAlphaComponent(0.55)
}
```

---

## File Map

| Change   | File                                            |
|----------|-------------------------------------------------|
| New      | `Sources/Dexter/HUD/HUDWindow.swift`            |
| New      | `Sources/Dexter/HUD/HUDTextView.swift`          |
| New      | `Sources/Dexter/HUD/HUDInputField.swift`        |
| Modified | `Sources/Dexter/FloatingWindow.swift`           |
| Modified | `Sources/Dexter/App.swift`                      |
| Modified | `Sources/Dexter/Bridge/DexterClient.swift`      |

---

## 1. `HUDTextView.swift`

Wraps an `NSScrollView` + `NSTextView` pair. Exposes a simple streaming-append API.
Owned by `HUDWindow`.

```swift
import AppKit

/// Scrollable text area for streaming response tokens.
///
/// Caller owns layout — `HUDTextView` is just a frame-locked NSView wrapper.
/// All mutation methods must be called on the main thread.
final class HUDTextView: NSView {

    private let scrollView: NSScrollView
    private let textView:   NSTextView

    override init(frame: NSRect) {
        scrollView = NSScrollView(frame: NSRect(origin: .zero, size: frame.size))
        scrollView.hasVerticalScroller   = true
        scrollView.autohidesScrollers    = true
        scrollView.borderType            = .noBorder
        scrollView.backgroundColor       = .clear
        scrollView.drawsBackground       = false
        scrollView.autoresizingMask      = [.width, .height]

        let tv = NSTextView(frame: scrollView.contentView.bounds)
        tv.isEditable              = false
        tv.isSelectable            = true   // operator can copy text
        tv.backgroundColor         = .clear
        tv.drawsBackground         = false
        tv.textContainerInset      = NSSize(width: 10, height: 10)
        tv.isVerticallyResizable   = true
        tv.autoresizingMask        = [.width]
        tv.textContainer?.widthTracksTextView   = true
        tv.textContainer?.heightTracksTextView  = false
        tv.textContainer?.containerSize = NSSize(
            width:  frame.width - 20,
            height: .greatestFiniteMagnitude
        )
        scrollView.documentView = tv
        textView = tv

        super.init(frame: frame)
        autoresizingMask = [.width, .height]
        addSubview(scrollView)
    }

    required init?(coder: NSCoder) { fatalError("IB not used") }

    // MARK: - Streaming API

    /// Append operator-turn text ("You: …\n\n") with subdued styling.
    func showOperatorTurn(_ text: String) {
        let attrs: [NSAttributedString.Key: Any] = [
            .font:            C.operatorFont,
            .foregroundColor: C.operatorColor,
        ]
        textView.textStorage?.append(
            NSAttributedString(string: "You: \(text)\n\n", attributes: attrs)
        )
    }

    /// Append a streaming response token.
    func appendToken(_ text: String) {
        let attrs: [NSAttributedString.Key: Any] = [
            .font:            C.responseFont,
            .foregroundColor: C.responseColor,
        ]
        textView.textStorage?.append(NSAttributedString(string: text, attributes: attrs))
        textView.scrollToEndOfDocument(nil)
    }

    /// Clear all text (called at the start of each new response).
    func clear() {
        textView.textStorage?.setAttributedString(NSAttributedString())
    }
}
```

---

## 2. `HUDInputField.swift`

Single-line `NSTextField` that calls `onSubmit` on Return and clears itself.
Non-activating (HUD panel handles that) but `acceptsFirstMouse = true`.

```swift
import AppKit

/// Single-line text input. Calls `onSubmit` when the operator presses Return.
///
/// Does not activate the application — `HUDWindow` is `.nonactivatingPanel`.
/// `acceptsFirstMouse(for:)` returns `true` so the operator can click-to-focus
/// without a double-click.
final class HUDInputField: NSTextField {

    /// Called with the submitted text when the operator presses Return.
    /// Fired on the main thread; caller is responsible for Task-hopping to actors.
    var onSubmit: ((String) -> Void)?

    override init(frame: NSRect) {
        super.init(frame: frame)
        placeholderString         = "Type a message…"
        bezelStyle                = .roundedBezel
        isBordered                = false
        isBezeled                 = false
        drawsBackground           = false
        // White placeholder / text against the dark vibrancy background.
        textColor                 = .white
        (cell as? NSTextFieldCell)?.placeholderAttributedString =
            NSAttributedString(
                string:     "Type a message…",
                attributes: [.foregroundColor: NSColor.white.withAlphaComponent(0.4),
                             .font:            C.responseFont]
            )
        font = C.responseFont
        // Delegate set by HUDWindow after init.
    }

    required init?(coder: NSCoder) { fatalError("IB not used") }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

// MARK: - NSTextFieldDelegate

extension HUDInputField: NSTextFieldDelegate {
    func control(_ control: NSControl,
                 textView: NSTextView,
                 doCommandBy selector: Selector) -> Bool {
        guard selector == #selector(NSResponder.insertNewline(_:)) else { return false }
        let text = stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return true }
        onSubmit?(text)
        stringValue = ""
        return true
    }
}
```

---

## 3. `HUDWindow.swift`

The HUD panel. Assembles `HUDTextView` + `HUDInputField` on a vibrancy background.
Provides the API consumed by `DexterClient`.

```swift
import AppKit

/// Floating conversation HUD — displays Dexter's streaming response text
/// and accepts typed input from the operator.
///
/// Lifecycle:
///   • `showOperatorInput(_:)` — operator spoke or typed; HUD appears with the transcript
///   • `beginResponseStreaming()` — THINKING received; HUD stays visible, ready for tokens
///   • `appendToken(_:)` — token from TextResponse; appended to text view
///   • `responseComplete()` — is_final received; dismiss timer armed
///   • `scheduleAutoDismiss()` — also called on IDLE as a safety net
///   • auto-dismiss fires → fade out → `orderOut(nil)`
///
/// `onTextSubmit` is set by App.swift and called when the operator presses Return
/// in the input field. App.swift bridges to DexterClient via `Task { await ... }`.
final class HUDWindow: NSPanel {

    // MARK: - Public API hook

    /// Called with the submitted text when the operator presses Return.
    /// Set by App.swift immediately after the window is created.
    var onTextSubmit: ((String) -> Void)?

    // MARK: - Subviews

    private let textArea:   HUDTextView
    private let inputField: HUDInputField

    // MARK: - Dismiss timer

    private var dismissItem: DispatchWorkItem?

    // MARK: - Init

    init(entityWindow: FloatingWindow) {
        let origin   = HUDWindow.origin(for: entityWindow.frame)
        let initRect = NSRect(x: origin.x, y: origin.y, width: C.width, height: C.height)

        // Text area occupies the space above the input field.
        let inputRect = NSRect(x: 0,
                               y: 0,
                               width:  C.width,
                               height: C.inputHeight + C.inputPadding * 2)
        let textRect  = NSRect(x: 0,
                               y: inputRect.maxY,
                               width:  C.width,
                               height: C.height - inputRect.height)

        textArea   = HUDTextView(frame: textRect)
        inputField = HUDInputField(frame: NSRect(
            x:      C.inputPadding,
            y:      C.inputPadding,
            width:  C.width - C.inputPadding * 2,
            height: C.inputHeight
        ))

        super.init(
            contentRect: initRect,
            styleMask:   [.borderless, .nonactivatingPanel],
            backing:     .buffered,
            defer:       false
        )

        level                = .screenSaver
        isOpaque             = false
        backgroundColor      = .clear
        hasShadow            = true
        collectionBehavior   = [.canJoinAllSpaces, .stationary, .ignoresCycle]
        alphaValue           = 0   // hidden until first response

        buildContent(textRect: textRect, inputRect: inputRect)

        inputField.delegate  = inputField   // HUDInputField is its own NSTextFieldDelegate
        inputField.onSubmit  = { [weak self] text in self?.onTextSubmit?(text) }
    }

    // MARK: - Layout

    private func buildContent(textRect: NSRect, inputRect: NSRect) {
        // NSVisualEffectView as the root — provides the dark translucent HUD material.
        let effect = NSVisualEffectView(frame: contentView!.bounds)
        effect.material      = .hudWindow
        effect.blendingMode  = .behindWindow
        effect.state         = .active
        effect.wantsLayer    = true
        effect.layer?.cornerRadius = C.cornerRadius
        effect.layer?.masksToBounds = true
        effect.autoresizingMask    = [.width, .height]
        contentView?.addSubview(effect)

        effect.addSubview(textArea)
        effect.addSubview(inputField)

        // Thin separator line between text area and input field.
        let sep = NSBox(frame: NSRect(x: C.inputPadding,
                                      y: inputRect.maxY - 1,
                                      width: C.width - C.inputPadding * 2,
                                      height: 1))
        sep.boxType   = .separator
        sep.autoresizingMask = [.width]
        effect.addSubview(sep)
    }

    // MARK: - Positioning

    /// Reposition to the left of the entity window, bottom-aligned.
    func follow(entityFrame: NSRect) {
        let origin = HUDWindow.origin(for: entityFrame)
        setFrameOrigin(origin)
    }

    private static func origin(for entityFrame: NSRect) -> NSPoint {
        NSPoint(
            x: entityFrame.minX - C.gap - C.width,
            y: entityFrame.minY
        )
    }

    // MARK: - DexterClient API

    /// Called when voice transcript or typed text is available.
    /// Shows the HUD (if hidden) and renders the operator's turn.
    func showOperatorInput(_ text: String) {
        textArea.clear()
        textArea.showOperatorTurn(text)
        show()
    }

    /// Called when THINKING state arrives — response is about to stream.
    /// Arms the HUD for incoming tokens; cancels any pending dismiss.
    func beginResponseStreaming() {
        cancelDismiss()
        show()
    }

    /// Append a streaming token from a non-final TextResponse.
    func appendToken(_ text: String) {
        textArea.appendToken(text)
    }

    /// Called when is_final TextResponse arrives. Arms the auto-dismiss timer.
    func responseComplete() {
        scheduleDismiss()
    }

    /// Safety-net dismiss trigger called on IDLE or LISTENING entity states.
    /// Only fires if the dismiss timer is not already armed (responseComplete
    /// arms it earlier; this catches cases where is_final never arrived).
    func scheduleAutoDismiss() {
        if dismissItem == nil { scheduleDismiss() }
    }

    // MARK: - Show / Hide

    private func show() {
        guard alphaValue < 0.5 else { return }   // already visible
        orderFrontRegardless()
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = C.fadeDuration
            animator().alphaValue = 1
        }
    }

    private func hide() {
        NSAnimationContext.runAnimationGroup { [weak self] ctx in
            ctx.duration = C.fadeDuration
            self?.animator().alphaValue = 0
        } completionHandler: { [weak self] in
            self?.orderOut(nil)
        }
    }

    private func scheduleDismiss() {
        cancelDismiss()
        let item = DispatchWorkItem { [weak self] in self?.hide() }
        dismissItem = item
        DispatchQueue.main.asyncAfter(deadline: .now() + C.dismissDelay, execute: item)
    }

    private func cancelDismiss() {
        dismissItem?.cancel()
        dismissItem = nil
    }
}

// MARK: - Shared HUD constants (module-visible)
//
// Declared internal (no modifier = internal in Swift) so HUDTextView.swift
// and HUDInputField.swift can reference C.responseFont etc. without duplication.
// Do NOT add `private` here — that would make C file-scoped and break compilation
// in the other two HUD files that depend on it.

enum C {
    static let width:          CGFloat      = 360
    static let height:         CGFloat      = 300
    static let gap:            CGFloat      = 12
    static let cornerRadius:   CGFloat      = 12
    static let fadeDuration:   TimeInterval = 0.20
    static let dismissDelay:   TimeInterval = 10.0
    static let inputHeight:    CGFloat      = 34
    static let inputPadding:   CGFloat      = 8
    static let responseFont  = NSFont.systemFont(ofSize: 14)
    static let operatorFont  = NSFont.systemFont(ofSize: 13, weight: .medium)
    static let responseColor = NSColor.white
    static let operatorColor = NSColor.white.withAlphaComponent(0.55)
}
```

---

## 4. `FloatingWindow.swift` — changes

### 4a. Add `hud` property

```swift
// After:
private(set) var animatedEntity: AnimatedEntity!

// Add:
private(set) var hud: HUDWindow!
```

### 4b. Create HUD in `init()` after `buildContentView()`

```swift
init() {
    super.init(
        contentRect: FloatingWindow.loadOrDefaultFrame(),
        styleMask:   [.borderless, .nonactivatingPanel],
        backing:     .buffered,
        defer:       false
    )
    configureWindow()
    buildContentView()

    // HUDWindow is a separate NSPanel that mirrors the entity window's position.
    // Created after buildContentView() so self.frame is valid for initial placement.
    hud = HUDWindow(entityWindow: self)

    delegate = self
}
```

### 4c. Update `windowDidMove` to reposition HUD

```swift
func windowDidMove(_ notification: Notification) {
    scheduleSaveFrame()
    // Keep the HUD pinned to the left of the entity window as the operator drags.
    hud.follow(entityFrame: frame)
}
```

---

## 5. `App.swift` — changes

Wire `hud.onTextSubmit` to `DexterClient.sendTypedInput` immediately after the client
is created and before `connect(to:)` is called:

```swift
Task {
    let c = DexterClient()
    self.client = c

    // Phase 25: typed input path — operator types in HUD → DexterClient.
    // Task { await } hops from the main-thread closure to the actor executor.
    window.hud.onTextSubmit = { [weak c] text in
        Task { await c?.sendTypedInput(text) }
    }

    await c.connect(to: window)
}
```

---

## 6. `DexterClient.swift` — changes

### 6a. Add `currentSessionID` actor property

```swift
// After `private var eventContinuation: ...`:
/// The session ID for the currently active session.
/// Set at the start of `runSession`, cleared on exit. Used by `sendTypedInput`
/// to construct `ClientEvent` without requiring the caller to know session routing.
private var currentSessionID: String? = nil
```

### 6b. Set / clear `currentSessionID` in `runSession`

```swift
// At the start of runSession, just before the retry loop:
// (sessionID is already created here as `let sessionID = UUID().uuidString`)
self.currentSessionID = sessionID

// In the defer block at the end of runSession / on every exit path:
self.currentSessionID = nil
```

The exact placement: `sessionID` is created as `let sessionID = UUID().uuidString` inside
`runSession`. Set `self.currentSessionID = sessionID` immediately after. The `defer`
that clears `eventContinuation` is the right place to also clear `currentSessionID`.

### 6c. Add `sendTypedInput`

```swift
/// Send a typed text input to the active session.
///
/// Phase 25: called from `HUDWindow.onTextSubmit` via a Task — bridges the
/// operator's keyboard input into the same inference pipeline as voice input.
/// No-ops silently if no session is active (onTextSubmit fires after session end).
func sendTypedInput(_ text: String) async {
    guard let sessionID = currentSessionID else { return }
    let event = Dexter_V1_ClientEvent.with {
        $0.traceID   = UUID().uuidString
        $0.sessionID = sessionID
        $0.textInput = Dexter_V1_TextInput.with { $0.content = text }
    }
    await send(event)
}
```

### 6d. Wire HUD to `onResponse` entity state handler

Replace the existing entity state `case`:

```swift
case .entityState(let change):
    let state = EntityState(from: change.state)
    await MainActor.run {
        window.animatedEntity.entityState = state

        // Phase 25: drive HUD visibility from entity state.
        // THINKING → response is incoming — arm the HUD for tokens.
        // IDLE / LISTENING → response done — schedule dismiss if not already.
        switch state {
        case .thinking:  window.hud.beginResponseStreaming()
        case .idle,
             .listening: window.hud.scheduleAutoDismiss()
        default: break
        }
    }
    if state == .listening {
        capture.activate()
    }
```

### 6e. Wire HUD to `textResponse`

Replace the existing `print` stub:

```swift
case .textResponse(let resp):
    await MainActor.run {
        window.hud.appendToken(resp.content)
        if resp.isFinal { window.hud.responseComplete() }
    }
```

### 6f. Wire HUD to voice transcript in `streamAudio.onResponse`

After the existing empty-transcript guard (where the early return for empty
transcripts is), before the `guard !fastPath` check:

```swift
// Phase 25: show what the operator said in the HUD before inference starts.
// Called before the fast-path check so it fires on both code paths.
if !transcript.isEmpty {
    await MainActor.run { [weak self] in
        // `self` is the DexterClient actor — access `window` via the
        // captured local, not through self (window is not an actor property).
        // Use the same pattern as the session onResponse closure.
    }
}
```

Wait — there's a capturing issue here. The `streamAudio.onResponse` closure currently
captures `[weak self, sessionID]`. It does NOT capture `window`. The session `onResponse`
closure captures `window` but `streamAudio.onResponse` is a separate closure.

**Fix:** Add `window` to the `streamAudio.onResponse` capture list:

```swift
// Before (line ~216):
onResponse: { [weak self, sessionID] response in

// After:
onResponse: { [weak self, sessionID, window] response in
```

Then after the transcript is accumulated and the non-empty guard passes, before the
`fastPath` check:

```swift
// Phase 25: show transcript in HUD so operator sees what Dexter heard.
await MainActor.run { window.hud.showOperatorInput(transcript) }

guard !fastPath else {
    print("[DexterClient] Fast-path transcript — Rust received directly, echo suppressed")
    return
}
```

`window` is `FloatingWindow` which conforms to `@unchecked Sendable` (established pattern
from Phase 12, used throughout the `onResponse` closure). The same extends to `HUDWindow`
which is owned by `window` — no additional Sendable annotation needed since we access it
through `window`.

---

## 7. Execution Order

1. `mkdir -p src/swift/Sources/Dexter/HUD`
2. Write `HUD/HUDTextView.swift`
3. Write `HUD/HUDInputField.swift`
4. Write `HUD/HUDWindow.swift`
5. Edit `FloatingWindow.swift` — add `hud` property + `HUDWindow(entityWindow: self)` in `init()` + `hud.follow(entityFrame:)` in `windowDidMove`
6. Edit `App.swift` — wire `hud.onTextSubmit`
7. Edit `DexterClient.swift`:
   a. Add `currentSessionID: String?` property
   b. Set/clear it in `runSession`
   c. Add `sendTypedInput`
   d. Update entity state `case` to drive HUD
   e. Update `textResponse` case to stream to HUD
   f. Add `window` to `streamAudio.onResponse` capture list + `hud.showOperatorInput`
8. `cd src/swift && swift build` — target: 0 errors, 0 warnings from project code
9. Update `docs/SESSION_STATE.json` (phase → Phase 26 next, test counts)
10. Update `memory/MEMORY.md`

---

## 8. Acceptance Criteria

### Automated

`swift build` in `src/swift/` succeeds with 0 errors, 0 warnings from project code.

### Manual

| # | Criterion | Verification |
|---|-----------|-------------|
| 1 | HUD appears on response | `make run` → speak a query → dark panel slides in to left of entity |
| 2 | Transcript shown | Voice transcript appears in subdued style at top of HUD |
| 3 | Tokens stream | Response text appears token-by-token as Dexter speaks |
| 4 | Auto-dismiss | HUD fades out ~10s after response completes |
| 5 | New query clears HUD | Second query: HUD clears and shows new transcript |
| 6 | HUD follows entity | Drag entity to new position → HUD moves with it, maintaining left-gap |
| 7 | Typed input | Click HUD input field → type → press Return → Dexter responds |
| 8 | Typed text shown | Typed text appears as operator turn in HUD before response streams |
| 9 | Click-through outside HUD | Clicking transparent area of entity window still passes through to app below |
| 10 | HUD hidden when idle | On startup before first interaction, HUD is not visible |
| 11 | Fast-path suppression | Fast-path voice transcripts still show in HUD (HUD fires before fast-path guard) |

---

## 9. Key Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| `window` not `Sendable` in `streamAudio.onResponse` capture | Same `@unchecked Sendable` extension pattern already used in Phase 11+. If a new warning surfaces: `extension FloatingWindow: @unchecked Sendable` (already in place) covers `hud` access through `window`. |
| `HUDWindow.follow` called before HUD frame is set | `HUDWindow.init(entityWindow:)` computes initial origin from `entityWindow.frame` at construction time, which is valid since `FloatingWindow.buildContentView()` runs before `HUDWindow` is created. |
| Typing in HUD input activates app, stealing focus from active app | `[.borderless, .nonactivatingPanel]` styleMask prevents the panel from becoming the application's key window. NSTextField inside a nonactivatingPanel takes text input without activating the app — the existing EventBridge `CGEventTap` is unaffected. |
| `currentSessionID` set after `runSession` starts but before `connect(to:)` completes | `currentSessionID` is set at the top of `runSession` before any `await`; `sendTypedInput` checks `currentSessionID != nil` before constructing events. Race window is zero: no event can fire before the session's retry loop has established a session. |
| HUD text accumulating across sessions (no clear on reconnect) | `beginResponseStreaming()` is called on THINKING, which fires at the start of every inference turn — including the first turn after reconnect. The clear is deferred to `showOperatorInput()` / implicit in the first `beginResponseStreaming()`. Add explicit `textArea.clear()` at the top of `beginResponseStreaming()` as a safety net. |
| NSVisualEffectView `.hudWindow` material only available on macOS 10.14+ | Project targets macOS 15+ (Package.swift: `.macOS(.v15)`). No risk. |
| Dismiss timer not cancelled when session ends | `scheduleAutoDismiss()` fires on IDLE which is sent by Rust on session shutdown. This is the correct place to arm the dismiss. The DispatchWorkItem holds only a `[weak self]` reference to HUDWindow — if the window is deallocated, the item fires and is a no-op. |
