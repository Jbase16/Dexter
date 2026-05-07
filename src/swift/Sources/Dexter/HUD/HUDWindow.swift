import AppKit

// MARK: - Shared HUD constants (module-visible)
//
// Declared internal (no modifier = internal in Swift) so HUDTextView.swift
// and HUDInputField.swift can reference C.responseFont etc. without duplication.
// Do NOT add `private` — that would make C file-scoped and break compilation
// in the other two HUD files that depend on it.

enum C {
    static let width:        CGFloat      = 360
    static let height:       CGFloat      = 300
    static let gap:          CGFloat      = 12     // horizontal gap between entity and HUD
    static let cornerRadius: CGFloat      = 12
    static let fadeDuration: TimeInterval = 0.20
    /// Minimum dwell time after is_final before auto-hide. Short replies still get this floor
    /// so the operator can read them; longer responses extend via `dismissDelayFor(text:)`.
    static let dismissDelayMin: TimeInterval = 12.0
    /// Maximum dwell time. Caps long-form output (debug dumps, full conversation extracts)
    /// so the HUD doesn't sit forever consuming screen real-estate.
    static let dismissDelayMax: TimeInterval = 90.0
    /// Reading speed for the linear extension above the floor: ~17 chars/sec ≈ 200 WPM × 5 chars/word.
    static let readingCharsPerSec: Double = 17.0
    static let inputHeight:  CGFloat      = 34
    static let inputPadding: CGFloat      = 8
    // nonisolated(unsafe): NSFont is not Sendable, but these are write-once
    // compile-time constants and are never mutated — concurrent reads are safe.
    nonisolated(unsafe) static let responseFont = NSFont.systemFont(ofSize: 14)
    nonisolated(unsafe) static let operatorFont = NSFont.systemFont(ofSize: 13, weight: .medium)
    static let responseColor = NSColor.white
    static let operatorColor = NSColor.white.withAlphaComponent(0.55)
    // Markdown styling — nonisolated(unsafe) because NSFont is non-Sendable;
    // these are write-once constants, never mutated — concurrent reads are safe.
    nonisolated(unsafe) static let codeFont = NSFont.monospacedSystemFont(ofSize: 12.5, weight: .regular)
    nonisolated(unsafe) static let h1Font   = NSFont.systemFont(ofSize: 17, weight: .bold)
    nonisolated(unsafe) static let h2Font   = NSFont.systemFont(ofSize: 15, weight: .semibold)
    nonisolated(unsafe) static let h3Font   = NSFont.systemFont(ofSize: 14, weight: .medium)
    static let codeColor       = NSColor(white: 0.85, alpha: 1.0)   // bright enough to read clearly
    static let codeBackground  = NSColor(white: 0.12, alpha: 1.0)   // dark tint behind code blocks
    static let inlineCodeColor = NSColor(calibratedRed: 0.75, green: 0.85, blue: 1.0, alpha: 1.0) // soft blue for inline code
    static let listIndent:     CGFloat = 14
    static let codeIndent:     CGFloat = 10  // horizontal padding inside code block text
    /// Paragraph spacing added after every prose line — prevents walls of text.
    static let proseLineSpacing: CGFloat = 3

    /// Height of the history panel when expanded.
    /// 280pt ≈ 5–6 lines of code or 10+ lines of conversational text.
    static let historyHeight:    CGFloat = 280

    /// Square hit target for the history toggle button.
    static let toggleButtonSize: CGFloat = 22

    /// Thin divider between the history panel and the current exchange area.
    static let historyDividerH:  CGFloat = 1

    /// TTS mute button — sits to the right of the input field in the input bar.
    static let muteButtonSize: CGFloat = 26
    static let muteButtonGap:  CGFloat = 6
}

private enum HUDSmokeLog {
    static let enabled: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static func log(_ message: String) {
        guard enabled else { return }
        print("[HUDSmoke] \(message)")
    }
}

// MARK: - HUDWindow

/// Floating conversation HUD — displays Dexter's streaming response text
/// and accepts typed input from the operator.
///
/// Positioned to the left of the entity window, bottom-aligned. Follows the
/// entity when it is dragged (FloatingWindow.windowDidMove calls follow(entityFrame:)).
///
/// ## Lifecycle
///
///   showOperatorInput(_:)   — operator spoke or typed; HUD appears with the input text
///   beginResponseStreaming() — THINKING received; HUD stays visible, ready for tokens
///   appendToken(_:)         — non-final TextResponse token; streamed into the text view
///   responseComplete()      — is_final received; dismiss timer armed (10 s default)
///   scheduleAutoDismiss()   — safety net on IDLE/LISTENING if responseComplete didn't fire
///
/// ## Typed input
///
///   `onTextSubmit` is set by App.swift and called when the operator presses Return
///   in the input field. App.swift bridges the main-thread callback to DexterClient
///   via `Task { await c?.sendTypedInput(text) }`.
final class HUDWindow: NSPanel {

    // MARK: - Public hooks

    /// Set by App.swift. Called on the main thread when the operator submits typed text.
    var onTextSubmit: ((String) -> Void)?

    /// Set by App.swift. Called when the mute button is toggled.
    /// `true` = TTS muted (text-only responses); `false` = TTS active.
    var onMuteToggle: ((Bool) -> Void)?

    // MARK: - Subviews

    private let textArea:   HUDTextView
    private let inputField: HUDInputField

    // History panel — built in buildContent(), added to effect view.
    private let historyView: HUDHistoryView

    // Toggle button — shows/hides the history panel.
    private let toggleButton: NSButton

    // Mute button — toggles TTS on/off.
    private let muteButton: NSButton

    // MARK: - Mode state

    private var ttsMuted: Bool = false

    // Whether the history panel is currently expanded.
    private var historyVisible: Bool = false

    // Thin divider between history panel and current exchange area.
    // Stored directly so toggleHistory() can show/hide it without a tag lookup.
    private var historyDivider: NSBox?

    // Operator text for the in-progress turn.
    // Set by showOperatorInput(_:); cleared by responseComplete() after recording.
    // Empty for proactive responses (showOperatorInput is never called for those).
    private var pendingTurnOperatorText: String = ""

    // MARK: - Auto-dismiss

    private var dismissItem: DispatchWorkItem?

    // MARK: - Init

    // Borderless NSPanel cannot become key by default — override so the input
    // field receives keyboard events when the operator clicks into the HUD.
    // .nonactivatingPanel still prevents Dexter from becoming the active app.
    override var canBecomeKey: Bool { true }

    init(entityWindow: FloatingWindow) {
        let origin    = HUDWindow.origin(for: entityWindow.frame)
        let initRect  = NSRect(x: origin.x, y: origin.y, width: C.width, height: C.height)

        // Input bar occupies the bottom strip; text area fills the rest.
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

        // Mute button: right-aligned in the input bar, vertically centred.
        let muteX = C.width - C.inputPadding - C.muteButtonSize
        let muteY = C.inputPadding + (C.inputHeight - C.muteButtonSize) / 2
        let muteRect = NSRect(x: muteX, y: muteY,
                              width: C.muteButtonSize, height: C.muteButtonSize)

        textArea   = HUDTextView(frame: textRect)
        inputField = HUDInputField(frame: NSRect(
            x:      C.inputPadding,
            y:      C.inputPadding,
            // Leave room for the mute button + gap to its left.
            width:  C.width - C.inputPadding * 2 - C.muteButtonSize - C.muteButtonGap,
            height: C.inputHeight
        ))
        historyView  = HUDHistoryView(frame: historyRect)
        toggleButton = NSButton(frame: toggleRect)
        muteButton   = NSButton(frame: muteRect)

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
        alphaValue         = 0   // hidden until first response

        buildContent(inputRect: inputRect)

        inputField.onSubmit = { [weak self] text in self?.onTextSubmit?(text) }
    }

    // MARK: - Layout

    private func buildContent(inputRect: NSRect) {
        guard let content = contentView else { return }

        // NSVisualEffectView provides the dark translucent HUD material.
        // .hudWindow material + .behindWindow blending = standard macOS HUD appearance.
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

        // Thin separator between text area and input field.
        let sep = NSBox(frame: NSRect(
            x:      C.inputPadding,
            y:      inputRect.maxY - 1,
            width:  C.width - C.inputPadding * 2,
            height: 1
        ))
        sep.boxType          = .separator
        sep.autoresizingMask = [.width]
        effect.addSubview(sep)

        // History view — above the base window height, initially hidden.
        // Height is fixed; y position tracks expansion via the window frame change.
        historyView.isHidden         = true
        historyView.autoresizingMask = [.width]   // stays at fixed height; y tracks expansion
        effect.addSubview(historyView)

        // Thin divider between history and current exchange area.
        // Positioned at the top of the base content area; visible only when history is shown.
        let histDiv = NSBox(frame: NSRect(
            x:      0,
            y:      C.height - C.historyDividerH,
            width:  C.width,
            height: C.historyDividerH
        ))
        histDiv.boxType          = .separator
        histDiv.autoresizingMask = [.width, .minYMargin]   // tracks top of base content
        histDiv.isHidden         = true
        effect.addSubview(histDiv)
        historyDivider = histDiv

        // Toggle button — subdued clock icon in the top-right corner.
        // autoresizingMask [.minXMargin, .minYMargin]: left and bottom margins are flexible,
        // so the button stays pinned to the top-right corner as the window grows upward.
        if let clockImage = NSImage(systemSymbolName: "clock", accessibilityDescription: "Toggle history") {
            toggleButton.image        = clockImage
            toggleButton.imageScaling = .scaleProportionallyDown
        } else {
            toggleButton.title = "◎"   // fallback glyph if SF Symbol unavailable
        }
        toggleButton.bezelStyle       = .inline
        toggleButton.isBordered       = false
        toggleButton.alphaValue       = 0.4   // subdued when history is hidden
        toggleButton.autoresizingMask = [.minXMargin, .minYMargin]
        toggleButton.target           = self
        toggleButton.action           = #selector(toggleHistory)
        effect.addSubview(toggleButton)

        // Mute button — stays at bottom-right of input bar as window grows upward.
        muteButton.bezelStyle       = .inline
        muteButton.isBordered       = false
        muteButton.alphaValue       = 0.5
        muteButton.autoresizingMask = [.minXMargin]
        muteButton.target           = self
        muteButton.action           = #selector(toggleMute)
        updateMuteIcon()
        effect.addSubview(muteButton)
    }

    // MARK: - TTS mute toggle

    @objc private func toggleMute() {
        ttsMuted.toggle()
        updateMuteIcon()
        onMuteToggle?(ttsMuted)
    }

    private func updateMuteIcon() {
        let name = ttsMuted ? "speaker.slash" : "speaker.wave.2"
        muteButton.image        = NSImage(systemSymbolName: name, accessibilityDescription: nil)
        muteButton.imageScaling = .scaleProportionallyDown
        // Full opacity when muted so the silenced state is unmistakably visible.
        muteButton.alphaValue   = ttsMuted ? 1.0 : 0.5
    }

    // MARK: - History toggle

    @objc private func toggleHistory() {
        historyVisible.toggle()

        let targetHeight = historyVisible
            ? C.height + C.historyHeight + C.historyDividerH
            : C.height

        // Animate window height upward while keeping the bottom edge (y origin) fixed.
        var newFrame = frame
        newFrame.size.height = targetHeight
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration       = 0.22
            ctx.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
            animator().setFrame(newFrame, display: true)
        }

        historyView.isHidden    = !historyVisible
        toggleButton.alphaValue = historyVisible ? 1.0 : 0.4

        // Show/hide the divider between the history panel and the current exchange area.
        historyDivider?.isHidden = !historyVisible

        if historyVisible {
            historyView.scrollToBottom()
        }
    }

    // MARK: - First-responder override

    /// Cancel the auto-dismiss timer and promote to key window when the input
    /// field gains focus.
    ///
    /// `.nonactivatingPanel` prevents the app from becoming active (correct — we never
    /// want Dexter to steal focus from the operator's current app), but the panel CAN
    /// and MUST become the key window so ⌘C/⌘V reach the field editor rather than
    /// being intercepted by the previously active application's key window.
    ///
    /// Without `makeKey()` here: user clicks the field → `acceptsFirstMouse` returns
    /// true → click is delivered → field editor activates → but the PANEL is still not
    /// the key window → ⌘C/⌘V get routed to the other app's key window. Right-click
    /// works regardless (contextual menus use a separate event path).
    override func makeFirstResponder(_ responder: NSResponder?) -> Bool {
        let result = super.makeFirstResponder(responder)
        // Promote to key window for any interaction with the HUD — input field OR
        // response text area. This ensures ⌘C works when the operator clicks into
        // the text area to copy a response, not just when they click the input field.
        // `.nonactivatingPanel` still prevents Dexter from stealing app focus.
        if result, responder != nil {
            cancelDismiss()
            makeKey()
        }
        return result
    }

    // MARK: - Positioning

    /// Reposition to the left of the entity window, bottom-aligned.
    /// Called by FloatingWindow.windowDidMove so the HUD tracks drags.
    func follow(entityFrame: NSRect) {
        setFrameOrigin(HUDWindow.origin(for: entityFrame))
    }

    private static func origin(for entityFrame: NSRect) -> NSPoint {
        NSPoint(
            x: entityFrame.minX - C.gap - C.width,
            y: entityFrame.minY
        )
    }

    // MARK: - DexterClient API

    /// Show the operator's voice transcript or typed text, then make the HUD visible.
    /// Clears any previous response so each turn starts fresh.
    func showOperatorInput(_ text: String) {
        HUDSmokeLog.log("showOperatorInput chars=\(text.count)")
        pendingTurnOperatorText = text   // capture before clearing textArea
        textArea.clear()
        textArea.showOperatorTurn(text)
        show()
    }

    /// THINKING state received — response is about to stream in.
    /// Cancels any pending dismiss and ensures the HUD is visible.
    func beginResponseStreaming() {
        HUDSmokeLog.log("beginResponseStreaming")
        cancelDismiss()
        // Record the insertion point so finalizeWithMarkdown() knows where the
        // operator-turn prefix ends and the response region begins.
        textArea.markResponseStart()
        // Clear if there was no prior operator input (e.g. proactive response).
        show()
    }

    /// Append a streaming token from a non-final TextResponse.
    func appendToken(_ text: String) {
        textArea.appendToken(text)
    }

    /// is_final TextResponse received — snap plain-text stream to formatted markdown,
    /// record this turn in the persistent history, then arm the auto-dismiss timer.
    func responseComplete() {
        textArea.finalizeWithMarkdown()

        // Record this completed turn to the persistent history.
        // currentResponseText is the raw markdown that was just rendered by finalizeWithMarkdown().
        // For proactive responses, pendingTurnOperatorText is "" (showOperatorInput was never
        // called) — appendTurn displays those as "(observation)" in history.
        let respText = textArea.currentResponseText
        if !respText.isEmpty {
            historyView.appendTurn(
                operatorText: pendingTurnOperatorText,
                responseText: respText
            )
        }
        pendingTurnOperatorText = ""   // reset so the next proactive response gets ""

        HUDSmokeLog.log("responseComplete responseChars=\(respText.count) visible=\(isHUDVisible)")
        scheduleDismiss()
    }

    /// Safety-net dismiss trigger. Called when entity transitions to IDLE or LISTENING.
    /// Only arms the timer if responseComplete() hasn't already done so — prevents
    /// resetting a timer that was already set with the correct deadline.
    ///
    /// Also suppressed when the input field is the current first responder: the user
    /// is actively typing, so arming the timer here would dismiss mid-input.
    /// makeFirstResponder() cancels an existing timer when focus moves to the field,
    /// but that guard cannot prevent a *new* timer being armed afterwards by a late-
    /// arriving IDLE/LISTENING state event — hence the symmetric check here.
    func scheduleAutoDismiss() {
        guard dismissItem == nil else { return }
        // scheduleDismiss() already guards against active input — no redundant check needed.
        scheduleDismiss()
    }

    // MARK: - Show / Hide

    /// True when the HUD is visible (alpha ≥ 0.5, mid-fade or fully opaque).
    var isHUDVisible: Bool { alphaValue >= 0.5 }

    /// Show the HUD and immediately focus the input field for typing.
    /// Called when the operator double-clicks the entity.
    func showForTyping() {
        cancelDismiss()
        if alphaValue >= 0.5 {
            // Already visible — just re-focus without re-animating.
            makeKey()
            _ = makeFirstResponder(inputField)
            return
        }
        show()
        // Defer focus until the panel is ordered front; show() calls
        // orderFrontRegardless() synchronously but the run-loop must
        // process one cycle before makeKey() takes effect.
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.makeKey()
            _ = self.makeFirstResponder(self.inputField)
        }
    }

    /// Hide the HUD immediately (operator dismissed via double-click toggle).
    func hideManual() {
        cancelDismiss()
        hide()
    }

    private func show() {
        guard alphaValue < 0.5 else { return }   // already visible — no-op
        HUDSmokeLog.log("show")
        orderFrontRegardless()
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = C.fadeDuration
            animator().alphaValue = 1
        }
    }

    private func hide() {
        HUDSmokeLog.log("hide")
        NSAnimationContext.runAnimationGroup { [weak self] ctx in
            ctx.duration = C.fadeDuration
            self?.animator().alphaValue = 0
        } completionHandler: { [weak self] in
            // completionHandler is nonisolated; dispatch to MainActor to call
            // the MainActor-isolated orderOut(_:).
            DispatchQueue.main.async { self?.orderOut(nil) }
        }
    }

    private func scheduleDismiss() {
        cancelDismiss()
        // Suppress the timer if the operator is actively typing — either the field
        // editor is active (currentEditor() != nil) or the field has un-submitted text.
        // `firstResponder !== inputField` is insufficient because NSPanel's first
        // responder is the internal field editor NSTextView, not the NSTextField itself.
        guard inputField.currentEditor() == nil, inputField.stringValue.isEmpty else { return }

        // Time-aware dismiss: short replies get the 12s floor, longer responses extend
        // proportionally (up to a 90s ceiling) so the operator has time to read them.
        // The floor + ceiling cover both cases that previously failed:
        //   - "flashed and vanished" — 12s floor on a 1-line response = enough to read
        //   - 2k-char message dump — needs ~120s read time but capped at 90s with history scrollback
        let textLen = textArea.currentResponseText.count
        let extension_ = Double(textLen) / C.readingCharsPerSec
        let delay = min(C.dismissDelayMax, max(C.dismissDelayMin, C.dismissDelayMin + extension_))

        let item = DispatchWorkItem { [weak self] in self?.hide() }
        dismissItem = item
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: item)
    }

    private func cancelDismiss() {
        dismissItem?.cancel()
        dismissItem = nil
    }
}
