# Phase 29 Implementation Plan: HUD Conversation History

## Context

The HUD (Phase 25) shows one exchange at a time. After `is_final`, the auto-dismiss
timer fires and the window fades out in 10 seconds. This is appropriate for brief
answers, but for the most common technical interactions — code blocks, multi-step
explanations, shell commands, filenames — 10 seconds is not enough time to read and
copy. Once dismissed, the response is permanently gone. The only way to recover it is
to ask the same question again.

Phase 29 adds a persistent, scrollable **history panel** that accumulates every
completed exchange in the current session. The history is hidden by default (preserving
the minimal HUD footprint from Phase 25), and revealed by clicking a small toggle
button in the HUD's top-right corner. The panel expands the HUD upward, above the
current exchange area, showing all past turns in chronological order with the most
recent at the bottom.

**No Rust changes. No proto changes. No new dependencies.** All work is in the four
HUD Swift files.

---

## Architecture Decisions

### History stored as completed-turn attributed strings, not a raw turn array

Two models for storing history:
1. **Turn array:** `[(operatorText: String, responseText: String)]` — re-render on display.
2. **Accumulated NSAttributedString:** Append each completed turn to a persistent
   attributed string as it completes.

**Decision: accumulated NSAttributedString appended per turn.**
Re-rendering from a raw array on every history show/hide would re-run
`MarkdownRenderer.render()` for all past turns on every toggle. With many turns and
code blocks, this is noticeable lag. Building the attributed string once (when
`responseComplete()` fires, while the full markdown response is freshly available)
and appending to a persistent NSAttributedString in `HUDHistoryView`'s NSTextStorage
means `toggleHistory()` just calls `setHidden(false)` — the content is already ready.

This also means the history NSTextView is always up-to-date; there is no rendering
work needed when the panel is shown.

### HUD grows upward — bottom edge stays fixed

The HUD's bottom edge is anchored to `entityFrame.minY` (established in `origin(for:)`).
Growing the panel upward = increasing `frame.size.height` while keeping `frame.origin.y`
constant. This is the natural expansion direction: the entity sits at a fixed position,
the HUD grows away from it into available screen space above.

The alternative (separate NSPanel positioned adjacent to HUDWindow) was rejected
because it introduces a second window lifecycle with separate show/hide animation,
potential z-ordering issues at `.screenSaver` level, and no natural visual connection
to the existing HUD.

### textArea fixed height — no vertical autoresize on window expand

The current `textArea` has `autoresizingMask = [.width, .height]`. When `HUDWindow`
expands upward, this would stretch the textArea to fill the new height — turning the
current-exchange area into a giant text view and pushing the history above it.
Incorrect behavior.

Fix: change `HUDTextView.autoresizingMask` to `[.width]`. textArea stays at its
original frame regardless of window height changes. The history view occupies the
expansion space above it.

### Toggle button uses AppKit autoresizing to track top-right corner

The toggle button is positioned at the top-right of the full window. When history
expands (window grows upward), the button must move up with the top edge. This is
expressed via `autoresizingMask = [.minXMargin, .minYMargin]`:
- `.minXMargin` = left margin is flexible → button stays right-aligned
- `.minYMargin` = bottom margin is flexible → button stays top-aligned

No explicit repositioning code needed. AppKit handles it.

### Proactive responses recorded as "(observation)" entries

`showOperatorInput(_:)` is not called for proactive responses — the Rust core
generates them without an operator prompt. DexterClient sets `THINKING` and starts
streaming tokens directly.

Tracking: `pendingTurnOperatorText: String` is set in `showOperatorInput(_:)` and
reset to `""` in `responseComplete()`. When `responseComplete()` records a history
entry:
- Non-empty `pendingTurnOperatorText` → "You: {text}\n{response}"
- Empty `pendingTurnOperatorText` → "(observation)\n{response}"

This distinguishes real exchanges from ambient proactive observations in the history
view without requiring any Rust-side changes.

### History persists for the current session only

History is not written to disk. It lives in `HUDHistoryView.textStorage` for the
lifetime of the Swift process. On Dexter restart or reconnect, history clears (the
HUDWindow is rebuilt). Cross-session conversation history exists in the Rust vector
store and retrieval pipeline — the HUD history is a display-layer convenience for
the current session, not a second memory system.

### Auto-scroll on append; user scroll preserved while history is visible

When a new turn is appended to history, `scrollToBottom()` is called. If the history
panel is currently visible and the user has scrolled up to read a previous entry,
the auto-scroll would jump them away from what they were reading.

**Phase 29:** auto-scroll unconditionally on append. The history panel shows briefly
after each turn and is typically closed during active interaction. The operator
scrolling-while-new-content-arrives scenario is uncommon enough that the simpler
implementation is correct for this phase.

A "scroll-lock" (suppress auto-scroll when the user has scrolled up, resume on next
manual scroll to bottom) is Phase 30+ scope.

---

## Layout Reference

```
  Before (C.height = 300):         After — history visible:
  ┌────────────────────[◎]┐         ┌────────────────────[◎]┐
  │                       │         │  History scroll view  │
  │     textArea          │         │  (C.historyHeight=280)│
  │  (C.height–inputBar)  │         ├────────────────────────┤  ← history divider
  ├────────────────────────┤         │     textArea          │
  │     inputField        │         │  (C.height–inputBar)  │
  └────────────────────────┘         ├────────────────────────┤
                                     │     inputField        │
                                     └────────────────────────┘
```

The toggle button [◎] sits at the top-right in both states — it tracks the top edge
via `autoresizingMask = [.minXMargin, .minYMargin]`.

---

## File Map

| Change   | File                                                           |
|----------|----------------------------------------------------------------|
| New      | `src/swift/Sources/Dexter/HUD/HUDHistoryView.swift`           |
| Modified | `src/swift/Sources/Dexter/HUD/HUDWindow.swift`                |
| Modified | `src/swift/Sources/Dexter/HUD/HUDTextView.swift`              |
| No change | `src/swift/Sources/Dexter/Bridge/DexterClient.swift`          |
| No change | `src/swift/Sources/Dexter/HUD/HUDInputField.swift`            |
| No change | `src/swift/Sources/Dexter/HUD/MarkdownRenderer.swift`         |

---

## 1. `HUDWindow.swift` — constants, new fields, history integration

### 1a. New constants in `C`

```swift
enum C {
    // ... existing constants unchanged ...

    /// Height of the history panel when expanded.
    /// 280pt ≈ 5–6 lines of code or 10+ lines of conversational text.
    static let historyHeight:     CGFloat = 280

    /// Square hit target for the history toggle button.
    static let toggleButtonSize:  CGFloat = 22

    /// Thin divider between the history panel and the current exchange area.
    static let historyDividerH:   CGFloat = 1
}
```

### 1b. New stored properties on `HUDWindow`

```swift
// History panel — built in buildContent(), added to effect view.
private let historyView: HUDHistoryView

// Toggle button — shows/hides the history panel.
private let toggleButton: NSButton

// Whether the history panel is currently expanded.
private var historyVisible: Bool = false

// Operator text for the in-progress turn.
// Set by showOperatorInput(_:); cleared by responseComplete() after recording.
// Empty for proactive responses (showOperatorInput is never called for those).
private var pendingTurnOperatorText: String = ""
```

### 1c. Update `init` to create `historyView` and `toggleButton`

The `HUDHistoryView` and toggle button must be initialized before `super.init` because
they are `let` properties. They are both empty/inert at init time — content is appended
by `responseComplete()`.

```swift
init(entityWindow: FloatingWindow) {
    let origin   = HUDWindow.origin(for: entityWindow.frame)
    let initRect = NSRect(x: origin.x, y: origin.y, width: C.width, height: C.height)

    let inputBarHeight = C.inputHeight + C.inputPadding * 2
    let inputRect = NSRect(x: 0, y: 0, width: C.width, height: inputBarHeight)
    let textRect  = NSRect(x: 0, y: inputBarHeight,
                           width: C.width, height: C.height - inputBarHeight)

    // History view positioned above the base window height (invisible until expanded).
    let historyRect = NSRect(x: 0, y: C.height, width: C.width, height: C.historyHeight)

    // Toggle button: top-right corner of the window. autoresizingMask tracks top-right.
    let btnX = C.width - C.toggleButtonSize - C.inputPadding
    let btnY = C.height - C.toggleButtonSize - C.inputPadding
    let toggleRect = NSRect(x: btnX, y: btnY, width: C.toggleButtonSize, height: C.toggleButtonSize)

    textArea    = HUDTextView(frame: textRect)
    inputField  = HUDInputField(frame: NSRect(
        x: C.inputPadding, y: C.inputPadding,
        width: C.width - C.inputPadding * 2, height: C.inputHeight
    ))
    historyView  = HUDHistoryView(frame: historyRect)
    toggleButton = NSButton(frame: toggleRect)

    super.init(
        contentRect: initRect,
        styleMask:   [.borderless, .nonactivatingPanel],
        backing:     .buffered,
        defer:       false
    )

    level              = .screenSaver
    isOpaque           = false
    backgroundColor    = .clear
    hasShadow          = true
    collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle]
    alphaValue         = 0

    buildContent(inputRect: inputRect)

    inputField.onSubmit = { [weak self] text in self?.onTextSubmit?(text) }
}
```

### 1d. Update `buildContent` to wire historyView and toggleButton

```swift
private func buildContent(inputRect: NSRect) {
    guard let content = contentView else { return }

    let effect = NSVisualEffectView(frame: content.bounds)
    effect.material         = .hudWindow
    effect.blendingMode     = .behindWindow
    effect.state            = .active
    effect.wantsLayer       = true
    effect.layer?.cornerRadius  = C.cornerRadius
    effect.layer?.masksToBounds = true
    effect.autoresizingMask     = [.width, .height]
    content.addSubview(effect)

    effect.addSubview(textArea)
    effect.addSubview(inputField)

    // Existing separator between textArea and inputField.
    let sep = NSBox(frame: NSRect(
        x: C.inputPadding, y: inputRect.maxY - 1,
        width: C.width - C.inputPadding * 2, height: 1
    ))
    sep.boxType          = .separator
    sep.autoresizingMask = [.width]
    effect.addSubview(sep)

    // History view — above the base window height, initially hidden.
    historyView.isHidden     = true
    historyView.autoresizingMask = [.width]   // stays at fixed height; y tracks expansion
    effect.addSubview(historyView)

    // Thin divider between history and current exchange area.
    // Same y as historyView origin — visible only when history is shown.
    let histDiv = NSBox(frame: NSRect(
        x: 0, y: C.height - C.historyDividerH,
        width: C.width, height: C.historyDividerH
    ))
    histDiv.boxType          = .separator
    histDiv.autoresizingMask = [.width, .minYMargin]   // tracks top of base content
    histDiv.isHidden         = true
    histDiv.tag              = 1001   // tag so we can find and toggle it
    effect.addSubview(histDiv)

    // Toggle button.
    if let clockImage = NSImage(systemSymbolName: "clock", accessibilityDescription: "Toggle history") {
        toggleButton.image = clockImage
        toggleButton.imageScaling = .scaleProportionallyDown
    } else {
        toggleButton.title = "◎"   // fallback glyph if SF Symbol unavailable
    }
    toggleButton.bezelStyle      = .inline
    toggleButton.isBordered      = false
    toggleButton.alphaValue      = 0.4   // subdued when history is hidden
    toggleButton.autoresizingMask = [.minXMargin, .minYMargin]  // tracks top-right corner
    toggleButton.target = self
    toggleButton.action = #selector(toggleHistory)
    effect.addSubview(toggleButton)
}
```

### 1e. `toggleHistory()` — expand / collapse

```swift
@objc private func toggleHistory() {
    historyVisible.toggle()

    let targetHeight = historyVisible
        ? C.height + C.historyHeight + C.historyDividerH
        : C.height

    // Animate window height, keeping y (bottom edge) fixed.
    var newFrame = frame
    newFrame.size.height = targetHeight
    NSAnimationContext.runAnimationGroup { ctx in
        ctx.duration = 0.22
        ctx.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        animator().setFrame(newFrame, display: true)
    }

    historyView.isHidden  = !historyVisible
    toggleButton.alphaValue = historyVisible ? 1.0 : 0.4

    // Show/hide the divider between history and current exchange.
    contentView?.subviews
        .compactMap { $0 as? NSVisualEffectView }
        .first?
        .subviews
        .first(where: { $0.tag == 1001 })
        .map { $0.isHidden = !historyVisible }

    if historyVisible {
        historyView.scrollToBottom()
    }
}
```

### 1f. `showOperatorInput` — capture operator text for pending turn

```swift
func showOperatorInput(_ text: String) {
    pendingTurnOperatorText = text   // ← capture before clearing textArea
    textArea.clear()
    textArea.showOperatorTurn(text)
    show()
}
```

### 1g. `responseComplete` — append turn to history

```swift
func responseComplete() {
    textArea.finalizeWithMarkdown()

    // Record this completed turn to the persistent history.
    // `currentResponseText` returns the raw markdown that was just rendered.
    let respText = textArea.currentResponseText
    if !respText.isEmpty {
        historyView.appendTurn(
            operatorText:  pendingTurnOperatorText,
            responseText:  respText
        )
    }
    pendingTurnOperatorText = ""   // reset for the next turn

    scheduleDismiss()
}
```

---

## 2. `HUDTextView.swift` — expose `currentResponseText`, fix autoresizing

### 2a. Expose `currentResponseText`

`responseRawText` is currently `private`. Add a read-only accessor so `HUDWindow`
can read it without promoting the property to `internal`:

```swift
/// The raw markdown text accumulated during the current response.
/// Valid after the last `appendToken()` call and before `clear()`.
/// Used by HUDWindow.responseComplete() to append this turn to history.
var currentResponseText: String { responseRawText }
```

### 2b. Fix `autoresizingMask` — remove `.height`

In `HUDTextView.init(frame:)`, change:

```swift
// Before:
autoresizingMask = [.width, .height]

// After:
autoresizingMask = [.width]
// Height is fixed. HUDWindow grows upward when history expands;
// textArea stays at its original frame rather than stretching to fill the window.
```

---

## 3. `HUDHistoryView.swift` — new file

```swift
import AppKit

/// Scrollable conversation history panel for the HUD.
///
/// Accumulates all completed exchanges in the current session as a persistent
/// NSAttributedString. Each turn is appended once via appendTurn(operatorText:responseText:)
/// when HUDWindow.responseComplete() fires — no re-rendering needed on show/hide.
///
/// All mutation methods must be called on the main thread (same contract as HUDTextView).
final class HUDHistoryView: NSView {

    private let scrollView: NSScrollView
    private let textView:   NSTextView

    override init(frame: NSRect) {
        scrollView = NSScrollView(frame: NSRect(origin: .zero, size: frame.size))
        scrollView.hasVerticalScroller  = true
        scrollView.autohidesScrollers   = true
        scrollView.borderType           = .noBorder
        scrollView.backgroundColor      = .clear
        scrollView.drawsBackground      = false
        scrollView.autoresizingMask     = [.width, .height]

        let tv = NSTextView(frame: scrollView.contentView.bounds)
        tv.isEditable             = false
        tv.isSelectable           = true
        tv.backgroundColor        = .clear
        tv.drawsBackground        = false
        tv.textContainerInset     = NSSize(width: 10, height: 10)
        tv.isVerticallyResizable  = true
        tv.autoresizingMask       = [.width]
        tv.textContainer?.widthTracksTextView  = true
        tv.textContainer?.heightTracksTextView = false
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

    // MARK: - History API

    /// Append a completed turn to the persistent history.
    ///
    /// `operatorText` is empty for proactive responses — displayed as "(observation)".
    /// `responseText` is the raw markdown accumulated during streaming; re-rendered here
    /// using MarkdownRenderer so history matches the formatted output seen in the main HUD.
    ///
    /// A horizontal rule (thin NSBox-style separator) is inserted between turns for
    /// visual separation. The first turn has no preceding separator.
    func appendTurn(operatorText: String, responseText: String) {
        guard let storage = textView.textStorage else { return }

        let result = NSMutableAttributedString()

        // Inter-turn separator — not added before the very first entry.
        if storage.length > 0 {
            // Two newlines provide visual breathing room between turns.
            let sep = NSAttributedString(
                string: "\n\n",
                attributes: [.font: C.responseFont, .foregroundColor: C.responseColor]
            )
            result.append(sep)
        }

        // Operator line: "You: <text>" or "(observation)" for proactive responses.
        let displayLabel = operatorText.isEmpty ? "(observation)" : "You: \(operatorText)"
        result.append(NSAttributedString(
            string: "\(displayLabel)\n",
            attributes: [
                .font:            C.operatorFont,
                .foregroundColor: C.operatorColor,
            ]
        ))

        // Response body — rendered markdown for consistent formatting with main HUD.
        result.append(MarkdownRenderer.render(responseText))

        storage.append(result)
        scrollToBottom()
    }

    /// Scroll history to the most recent entry.
    func scrollToBottom() {
        textView.scrollToEndOfDocument(nil)
    }
}
```

---

## 4. Execution Order

1. Add constants to `C` in `HUDWindow.swift`: `historyHeight`, `toggleButtonSize`,
   `historyDividerH`
2. Add new stored properties to `HUDWindow`: `historyView`, `toggleButton`,
   `historyVisible`, `pendingTurnOperatorText`
3. Update `HUDWindow.init`: create `historyView` and `toggleButton` before `super.init`
4. Update `buildContent`: wire `historyView`, history divider, and `toggleButton` into
   the effect view
5. Add `toggleHistory()` as `@objc` method
6. Update `showOperatorInput(_:)` to set `pendingTurnOperatorText`
7. Update `responseComplete()` to call `historyView.appendTurn` and reset
   `pendingTurnOperatorText`
8. Edit `HUDTextView.swift`:
   a. Change `autoresizingMask` from `[.width, .height]` to `[.width]`
   b. Add `var currentResponseText: String { responseRawText }` getter
9. Create `HUDHistoryView.swift` (new file)
10. `cd src/swift && swift build` — 0 errors, 0 warnings from project code

---

## 5. Acceptance Criteria

### Automated

- `swift build` in `src/swift/`: 0 errors, 0 warnings from project code

### Manual

| # | Criterion | Verification |
|---|-----------|-------------|
| 1 | Toggle button visible | HUD appears with a subdued clock icon (◎) in top-right corner |
| 2 | Toggle expands upward | Click ◎ → HUD grows upward by `C.historyHeight` with animation; history panel appears |
| 3 | Toggle collapses | Click ◎ again → HUD returns to `C.height`; history panel hides |
| 4 | History accumulates | Ask 3 questions → toggle history → all 3 operator + response pairs visible and scrollable |
| 5 | Markdown in history | Response with code block in history → monospace code block rendered (not raw triple-backtick) |
| 6 | Proactive entries labelled | Dexter proactive observation appears as "(observation)" with the response in history |
| 7 | auto-scroll on new turn | History visible; new response completes → history auto-scrolls to show new entry |
| 8 | Operator can scroll history | In expanded history, scroll up to an earlier turn — scroll is stable, not auto-jumped |
| 9 | textArea unchanged | Current exchange area (textArea + inputField) unchanged — same size and behavior |
| 10 | Dismiss still works | HUD still auto-dismisses 10s after response; history panel state preserved (expands again on next response) |
| 11 | Follow entity drag | Drag entity to new position → HUD follows; history panel moves with it |
| 12 | Button alpha | Toggle button is subdued (α=0.4) when history hidden; full opacity (α=1.0) when visible |
| 13 | Text selectable in history | Cmd+C on selected text in history panel copies to clipboard |
| 14 | Session clear | Restart Dexter → history is empty (not persisted across sessions) |

---

## 6. Key Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| `textArea` stretches when window expands (existing `[.width, .height]` mask) | Fixed by changing to `[.width]` in step 8a. Regression protected by manual criterion #9. |
| History divider (tag=1001) lookup brittle if subview structure changes | The `tag` approach is a simple subview lookup in a controlled view hierarchy. The effect view's subviews are all created in `buildContent` — no third-party subviews. Tag 1001 is not used elsewhere. If a future refactor breaks this, the symptom is a visible divider when history is hidden (obvious) rather than a crash. |
| `toggleHistory()` called rapidly (double-click) produces animation conflict | `NSAnimationContext.runAnimationGroup` with `animator()` is idempotent for repeated calls — the new animation overrides the in-progress one at the current animated position. No explicit guard needed. |
| History panel positioned at `y: C.height` places it outside initial window bounds | `NSView` can have subviews outside the window's `contentView.bounds` — they are clipped by the window's `contentView` without error. The history view is invisible (clipped) until `isHidden = false` AND the window has been expanded. The order in `toggleHistory()` is: set frame first, then `isHidden = false` — so the view is never outside bounds while visible. |
| `currentResponseText` exposed after it was `private` | `var currentResponseText: String { responseRawText }` is `internal` (Swift default), accessible within the same module (Dexter target). It is not `public` — external callers cannot reach it. The invariant is unchanged: `responseRawText` is only non-empty between `markResponseStart()` and `clear()`. |
| Proactive responses have empty `pendingTurnOperatorText` — but a previous turn's operator text leaks in | `pendingTurnOperatorText` is reset to `""` at the end of every `responseComplete()`. A proactive response fires `beginResponseStreaming()` without calling `showOperatorInput()` — so `pendingTurnOperatorText` is still `""` from the previous reset. Correct behavior guaranteed. |
| `NSImage(systemSymbolName:)` unavailable on macOS < 11.0 | `systemSymbolName:` was added in macOS 11.0. Machine is macOS 26.3 — no compat issue. The fallback `title = "◎"` guards against the `nil` case defensively regardless. |
