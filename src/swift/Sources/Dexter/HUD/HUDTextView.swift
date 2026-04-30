import AppKit

// MARK: - SelectableTextView

/// NSTextView subclass that guarantees ⌘C / ⌘A work even in a non-activating panel.
///
/// When the HUD panel is key but not the active application, AppKit's normal
/// key-equivalent dispatch sometimes fails to reach the text view's copy: action.
/// Overriding `performKeyEquivalent` and calling the action methods directly
/// bypasses the dispatch chain, matching the fix applied to HUDInputField (⌘V).
private final class SelectableTextView: NSTextView {
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command,
              let key = event.charactersIgnoringModifiers
        else { return super.performKeyEquivalent(with: event) }
        switch key {
        case "c": copy(nil);      return true
        case "a": selectAll(nil); return true
        default:  return super.performKeyEquivalent(with: event)
        }
    }
}

// MARK: - HUDTextView

/// Scrollable text area for streaming response tokens.
///
/// Wraps an NSScrollView + SelectableTextView pair. Exposes a minimal streaming-append API;
/// layout is owned by HUDWindow. All mutation methods must be called on the main thread.
///
/// `C` (the shared HUD constants enum) is defined in HUDWindow.swift as internal,
/// making it visible here across the module.
final class HUDTextView: NSView {

    private let scrollView: NSScrollView
    private let textView:   SelectableTextView

    override init(frame: NSRect) {
        scrollView = NSScrollView(frame: NSRect(origin: .zero, size: frame.size))
        scrollView.hasVerticalScroller  = true
        scrollView.autohidesScrollers   = true
        scrollView.borderType           = .noBorder
        scrollView.backgroundColor      = .clear
        scrollView.drawsBackground      = false
        scrollView.autoresizingMask     = [.width, .height]

        let tv = SelectableTextView(frame: scrollView.contentView.bounds)
        tv.isEditable             = false
        tv.isSelectable           = true   // operator can select and copy text (Cmd+C)
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
        // Height is fixed. HUDWindow grows upward when history expands;
        // textArea stays at its original frame rather than stretching to fill the window.
        autoresizingMask = [.width]
        addSubview(scrollView)
    }

    required init?(coder: NSCoder) { fatalError("IB not used") }

    // MARK: - Deferred markdown render state

    /// Raw markdown text accumulated during streaming.
    /// Replaced by the formatted NSAttributedString in finalizeWithMarkdown() on is_final.
    private var responseRawText = ""

    /// The raw markdown text accumulated during the current response.
    /// Valid after the last appendToken() call and before clear().
    /// Used by HUDWindow.responseComplete() to append this turn to history.
    var currentResponseText: String { responseRawText }

    /// Offset into textStorage where the current response begins.
    /// Set by markResponseStart(); used by finalizeWithMarkdown() to replace
    /// only the response region while preserving the operator-turn prefix.
    private var responseStartLocation: Int = 0

    /// True while a <dexter:action> block is being streamed.
    /// Tokens inside action blocks are suppressed from display — they represent
    /// internal system instructions, not operator-facing response text.
    private var inActionBlock = false

    /// Tokens held back while we wait to confirm whether a partial `<dexter:action>`
    /// tag is being formed. Flushed to the text view once a non-matching character
    /// arrives; discarded if the full opening tag is confirmed.
    ///
    /// Without this buffer, the individual characters of the opening tag — `<`, `d`,
    /// `e`, `x`, … — would each flash briefly in the HUD before the retroactive trim
    /// fires, creating visible noise during action execution.
    private var pendingDisplayBuffer = ""

    // The opening action tag as a constant for prefix-suffix checks.
    private static let actionOpenTag = "<dexter:action>"

    // MARK: - Streaming API

    /// Show the operator's turn ("You: …") in subdued style above the response.
    func showOperatorTurn(_ text: String) {
        let attrs: [NSAttributedString.Key: Any] = [
            .font:            C.operatorFont,
            .foregroundColor: C.operatorColor,
        ]
        textView.textStorage?.append(
            NSAttributedString(string: "You: \(text)\n\n", attributes: attrs)
        )
    }

    /// Record the current end of textStorage as the start of the response region.
    /// Must be called after showOperatorTurn() and before the first appendToken().
    /// Resets the raw-text accumulator for this turn.
    func markResponseStart() {
        responseRawText       = ""
        responseStartLocation = textView.textStorage?.length ?? 0
        inActionBlock         = false
        pendingDisplayBuffer  = ""
    }

    /// Append a streaming response token with full-brightness styling.
    /// Also accumulates raw text for the deferred markdown render on is_final.
    ///
    /// Action blocks (<dexter:action>…</dexter:action>) are suppressed from display —
    /// they are internal system instructions and should never be visible to the operator.
    ///
    /// Two-layer suppression strategy:
    ///
    /// 1. **Pending buffer**: tokens that extend a partial `<dexter:action>` prefix at the
    ///    end of accumulated text are held in `pendingDisplayBuffer` instead of being
    ///    written to the text view. This prevents the individual characters `<`, `d`, `e`,
    ///    `x`, … from flashing briefly in the HUD before the tag is confirmed.
    ///
    /// 2. **Retroactive trim**: if the full opening tag is confirmed, `pendingDisplayBuffer`
    ///    is discarded and the text view is trimmed back to the pre-action prose. This is
    ///    a safety net in case the tag arrived in a single multi-character token.
    func appendToken(_ text: String) {
        responseRawText += text   // always accumulate for finalizeWithMarkdown()

        // ── Already inside an action block ───────────────────────────────────────
        if inActionBlock {
            if responseRawText.contains("</dexter:action>") {
                inActionBlock = false
            }
            return
        }

        // ── Full opening tag detected ────────────────────────────────────────────
        if responseRawText.contains(Self.actionOpenTag) {
            inActionBlock        = true
            pendingDisplayBuffer = ""   // discard buffered chars — they're part of the block
            // Retroactively trim the text view back to text before the opening tag.
            // If the pending buffer kept the tag chars off-screen, this is a no-op in
            // practice; retained as a safety net for single-token `<dexter:action>` delivery.
            if let storage = textView.textStorage,
               let tagRange = responseRawText.range(of: Self.actionOpenTag) {
                let visiblePart = String(responseRawText[..<tagRange.lowerBound])
                let responseLen = storage.length - responseStartLocation
                if responseLen > 0 {
                    let displayRange = NSRange(location: responseStartLocation, length: responseLen)
                    let attrs: [NSAttributedString.Key: Any] = [
                        .font:            C.responseFont,
                        .foregroundColor: C.responseColor,
                    ]
                    storage.replaceCharacters(
                        in:   displayRange,
                        with: NSAttributedString(string: visiblePart, attributes: attrs)
                    )
                    textView.scrollToEndOfDocument(nil)
                }
            }
            return
        }

        // ── Partial tag at end of accumulated text → hold in buffer ──────────────
        // If the current accumulated text ends with any non-empty strict prefix of
        // `<dexter:action>`, we can't display yet — the next token might complete the tag.
        if hasPendingActionTagPrefix() {
            pendingDisplayBuffer += text
            return
        }

        // ── Normal display ───────────────────────────────────────────────────────
        // Flush any pending buffer first (tokens held while waiting on tag confirmation).
        let toDisplay: String
        if !pendingDisplayBuffer.isEmpty {
            toDisplay            = pendingDisplayBuffer + text
            pendingDisplayBuffer = ""
        } else {
            toDisplay = text
        }

        let attrs: [NSAttributedString.Key: Any] = [
            .font:            C.responseFont,
            .foregroundColor: C.responseColor,
        ]
        textView.textStorage?.append(NSAttributedString(string: toDisplay, attributes: attrs))
        textView.scrollToEndOfDocument(nil)
    }

    /// Returns true if the current accumulated response text ends with a non-empty
    /// strict prefix of `<dexter:action>` — signalling that the action opening tag
    /// may be forming and display should be held.
    ///
    /// Checks from the longest possible matching prefix (up to tag length − 1) down
    /// to a single character. Short-circuits on first match for efficiency.
    private func hasPendingActionTagPrefix() -> Bool {
        let tag = Self.actionOpenTag
        let s   = responseRawText
        guard !s.isEmpty else { return false }
        let maxLen = min(tag.count - 1, s.count)
        for length in stride(from: maxLen, through: 1, by: -1) {
            let tagPrefix = tag.prefix(length)
            if s.hasSuffix(tagPrefix) { return true }
        }
        return false
    }

    /// Replace the streamed plain-text response region with formatted markdown.
    ///
    /// Called by HUDWindow.responseComplete() on is_final. The operator-turn prefix
    /// (at indices 0 ..< responseStartLocation) is preserved exactly; only the response
    /// region is replaced. No-ops when no response text was accumulated.
    func finalizeWithMarkdown() {
        guard !responseRawText.isEmpty,
              let storage = textView.textStorage else { return }

        // Pass 1: strip injected context markers that qwen3 occasionally echoes verbatim.
        var cleaned = HUDTextView.stripContextMarkers(responseRawText)

        // Pass 2: strip action blocks — <dexter:action>…</dexter:action> is internal
        // scaffolding and must never appear in the operator-visible response.
        cleaned = HUDTextView.stripActionBlocks(cleaned)

        // Update responseRawText so currentResponseText (used for history) is also clean.
        if cleaned != responseRawText { responseRawText = cleaned }

        let responseLen = storage.length - responseStartLocation
        // Defensive guard: range must be valid.
        guard responseLen >= 0,
              responseStartLocation + responseLen <= storage.length else { return }
        let range = NSRange(location: responseStartLocation, length: responseLen)

        // If stripping removed everything (response was only an action block),
        // clear the region cleanly with no visible content.
        if cleaned.isEmpty {
            storage.replaceCharacters(in: range, with: NSAttributedString())
            return
        }

        let rendered = MarkdownRenderer.render(cleaned)
        storage.replaceCharacters(in: range, with: rendered)
        textView.scrollToEndOfDocument(nil)
    }

    /// Clear all text — called at the start of each new operator turn.
    func clear() {
        textView.textStorage?.setAttributedString(NSAttributedString())
        responseRawText       = ""
        responseStartLocation = 0
        inActionBlock         = false
        pendingDisplayBuffer  = ""
    }

    // MARK: - Action block stripping

    /// Remove <dexter:action>…</dexter:action> blocks from response text.
    ///
    /// These blocks are internal execution scaffolding — JSON action descriptors
    /// that Rust parses to take OS actions. They are never operator-facing content.
    ///
    /// Two cases handled:
    /// - Balanced blocks: `<dexter:action>{…}</dexter:action>` → entire span removed.
    /// - Unclosed blocks: qwen3 stops generation after `}` before emitting the close tag
    ///   (EOS fires at JSON `}`). Everything from `<dexter:action>` to end is removed.
    private static func stripActionBlocks(_ text: String) -> String {
        var result = text
        while let openRange = result.range(of: "<dexter:action>") {
            if let closeRange = result.range(of: "</dexter:action>") {
                result.removeSubrange(openRange.lowerBound..<closeRange.upperBound)
            } else {
                // No closing tag — remove from opening tag to end of string.
                result = String(result[..<openRange.lowerBound])
                break
            }
        }
        return result.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    // MARK: - Context marker stripping

    /// Remove injected context labels that qwen3 echoes verbatim in responses.
    ///
    /// Two-pass strategy mirrors the Rust `strip_context_markers`:
    ///
    /// Pass 1 — Bracket token removal: qwen3 generates `[Context: X]` from training
    /// memory even when injection uses no brackets.  A response like
    /// `[Context: Safari] You're in Safari.` becomes `You're in Safari.` after this pass.
    ///
    /// Pass 2 — Bare line removal: drop any line whose trimmed prefix is a bare label
    /// (`Context: ...`, `DateTime: ...`, etc.) — whole-response echoes with no content.
    private static func stripContextMarkers(_ text: String) -> String {
        let bracketPrefixes = ["[Context:", "[Clipboard:", "[Shell:", "[Memory:"]
        let linePrefixes    = ["Context:", "Clipboard:", "Shell:", "Memory:"]

        // Pass 1: remove bracket-delimited tokens, preserving text after the `]`.
        var result = text
        for prefix in bracketPrefixes {
            while let start = result.range(of: prefix) {
                if let closeIdx = result[start.upperBound...].firstIndex(of: "]") {
                    var end = result.index(after: closeIdx)
                    if end < result.endIndex && result[end] == " " {
                        end = result.index(after: end)
                    }
                    result.removeSubrange(start.lowerBound..<end)
                } else {
                    result = String(result[..<start.lowerBound])
                    break
                }
            }
        }

        // Pass 2: drop lines that are entirely a bare label.
        let filtered = result
            .components(separatedBy: "\n")
            .filter { line in
                let trimmed = line.trimmingCharacters(in: .whitespaces)
                return !linePrefixes.contains { trimmed.hasPrefix($0) }
            }
            .joined(separator: "\n")
        return filtered.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}
