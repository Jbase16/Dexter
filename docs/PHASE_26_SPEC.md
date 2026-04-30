# Phase 26 Implementation Plan: HUD Markdown Rendering

## Context

Dexter's responses frequently contain shell commands, file paths, code snippets, and
numbered steps. After Phase 25, these all render in the HUD as raw markdown text —
literal backticks, triple-fence delimiters, asterisks, and `# ` prefixes visible to
the operator. For the most common response type (technical assistance), this is
immediately and noticeably bad.

Phase 26 delivers a lightweight markdown renderer that converts the accumulated
response text to styled `NSAttributedString` when `is_final` arrives. The operator
sees tokens streaming in as plain text for immediate feedback, then the HUD "snaps"
to the formatted version on completion.

**Scope:** Pure Swift, HUD files only. No Rust changes. No proto changes.
No new package dependencies.

---

## Architecture Decisions

### No new dependencies — custom lightweight parser

The system is intentionally lean. Adding `swift-markdown`, `Down`, or another
CommonMark library brings hundreds of kilobytes of compiled code and a dependency
surface to maintain. The 90% of Dexter's actual output — code blocks, inline code,
bold, lists, and occasional headers — is covered by ~200 lines of custom Swift.
The remaining 10% (tables, nested lists, blockquotes, raw HTML) degrades gracefully
to plain text, which is acceptable.

### Hybrid streaming + final render

Attempting to parse markdown incrementally as tokens arrive is complex: a `*` is
ambiguous until subsequent tokens reveal whether it's a bullet or emphasis delimiter.
The chosen approach avoids this entirely:

- **During streaming:** tokens are appended as plain white text (current behavior).
  The operator gets immediate visual feedback with no latency.
- **On `is_final`:** `HUDTextView.finalizeWithMarkdown()` replaces the accumulated
  plain text with a fully rendered `NSAttributedString`. The "snap" to formatted
  output coincides with TTS beginning playback — the operator is listening at exactly
  the moment their attention shifts from watching text stream to processing content.

The raw accumulated text is stored in `HUDTextView.responseRawText: String`
alongside the live `NSTextStorage` append. No second network call, no re-inference.

### NSAttributedString throughout — no SwiftUI

`NSTextView` already owns an `NSTextStorage` backed `NSAttributedString`. Styled
output is produced by `NSAttributedString` APIs directly. No bridging layer needed.

---

## Supported Markdown Features

### Block-level

| Element | Syntax | Rendering |
|---------|--------|-----------|
| Fenced code block | ```` ``` ```` … ```` ``` ```` | Monospace font, dimmed color, 8pt left indent |
| Header 1 | `# text` | 18pt bold, extra leading |
| Header 2 | `## text` | 16pt semibold |
| Header 3 | `### text` | 15pt medium |
| Unordered list item | `- text` or `* text` | `•` + 12pt indent |
| Ordered list item | `1. text` | number preserved + 12pt indent |
| Blank line | empty line | paragraph break (8pt spacing) |

### Inline (applied within non-code-block lines)

| Element | Syntax | Rendering |
|---------|--------|-----------|
| Inline code | `` `code` `` | Monospace, dimmed color |
| Bold | `**text**` | `.semibold` weight |
| Italic | `*text*` or `_text_` | italic trait |

### Explicitly out of scope (plain text fallback)

- Tables
- Blockquotes (`> `)
- Nested lists (indented `  -`)
- Strikethrough (`~~text~~`)
- Clickable links (`[text](url)`) — NSTextView link delegates deferred to Phase 27+
- Raw HTML

---

## Styling Constants

Add to `enum C` in `HUDWindow.swift`:

```swift
// Markdown styling — all nonisolated(unsafe) because NSFont/NSColor
// are non-Sendable; these are write-once constants, never mutated.
nonisolated(unsafe) static let codeFont      = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
nonisolated(unsafe) static let h1Font        = NSFont.systemFont(ofSize: 18, weight: .bold)
nonisolated(unsafe) static let h2Font        = NSFont.systemFont(ofSize: 16, weight: .semibold)
nonisolated(unsafe) static let h3Font        = NSFont.systemFont(ofSize: 15, weight: .medium)
static let codeColor  = NSColor(white: 0.78, alpha: 1.0)   // dimmed white for code
static let listIndent: CGFloat = 14
```

`codeColor` and `listIndent` are value types — no `nonisolated(unsafe)` needed.

---

## File Map

| Change   | File                                    |
|----------|-----------------------------------------|
| New      | `Sources/Dexter/HUD/MarkdownRenderer.swift` |
| Modified | `Sources/Dexter/HUD/HUDTextView.swift`  |
| Modified | `Sources/Dexter/HUD/HUDWindow.swift`    |

---

## 1. `MarkdownRenderer.swift`

Pure function. No stored state. Accepts a complete markdown string, returns
`NSAttributedString`. Called only from `HUDTextView.finalizeWithMarkdown()`.

### 1a. Top-level entry point

```swift
import AppKit

/// Converts a markdown string to a styled NSAttributedString for display in HUDTextView.
///
/// Supported: fenced code blocks, H1–H3, unordered/ordered lists, inline code,
/// bold, italic. Everything else is rendered as plain text — no errors, no crashes.
/// Called once per response on is_final; not used during token streaming.
enum MarkdownRenderer {

    static func render(_ markdown: String) -> NSAttributedString {
        let result = NSMutableAttributedString()
        let lines  = markdown.components(separatedBy: "\n")

        var inCodeBlock    = false
        var codeLines:     [String] = []

        for line in lines {
            // ── Fenced code block boundary ──────────────────────────────────
            if line.hasPrefix("```") {
                if inCodeBlock {
                    result.append(codeBlock(codeLines))
                    codeLines   = []
                    inCodeBlock = false
                } else {
                    inCodeBlock = true
                    // Language hint on the ``` line is discarded — no syntax
                    // highlighting in Phase 26; the monospace font is sufficient.
                }
                continue
            }

            if inCodeBlock {
                codeLines.append(line)
                continue
            }

            // ── Block-level elements ─────────────────────────────────────────
            if line.hasPrefix("### ") {
                result.append(header(String(line.dropFirst(4)), font: C.h3Font))
            } else if line.hasPrefix("## ") {
                result.append(header(String(line.dropFirst(3)), font: C.h2Font))
            } else if line.hasPrefix("# ") {
                result.append(header(String(line.dropFirst(2)), font: C.h1Font))
            } else if line.hasPrefix("- ") || line.hasPrefix("* ") {
                result.append(listItem(String(line.dropFirst(2)), prefix: "•"))
            } else if let (number, text) = orderedListItem(line) {
                result.append(listItem(text, prefix: "\(number)."))
            } else if line.isEmpty {
                result.append(paragraph())
            } else {
                // Normal text — apply inline formatting.
                result.append(inlineFormatted(line))
                result.append(newline())
            }
        }

        // Flush any unclosed code block (malformed markdown).
        if inCodeBlock && !codeLines.isEmpty {
            result.append(codeBlock(codeLines))
        }

        return result
    }
}
```

### 1b. Block helpers

```swift
extension MarkdownRenderer {

    // Fenced code block: each line in monospace, left-indented, dimmed color.
    // A blank line is added before and after for breathing room.
    private static func codeBlock(_ lines: [String]) -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.headIndent      = C.listIndent
        style.firstLineHeadIndent = C.listIndent
        style.paragraphSpacingBefore = 4

        let attrs: [NSAttributedString.Key: Any] = [
            .font:            C.codeFont,
            .foregroundColor: C.codeColor,
            .paragraphStyle:  style,
        ]
        let block = NSMutableAttributedString(string: "\n", attributes: attrs)
        for line in lines {
            block.append(NSAttributedString(string: line + "\n", attributes: attrs))
        }
        block.append(NSAttributedString(string: "\n", attributes: attrs))
        return block
    }

    // Header: bold/semibold font, trailing newline, 4pt extra space before.
    private static func header(_ text: String, font: NSFont) -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.paragraphSpacingBefore = 6
        style.paragraphSpacing       = 2
        return NSAttributedString(string: text + "\n", attributes: [
            .font:            font,
            .foregroundColor: C.responseColor,
            .paragraphStyle:  style,
        ])
    }

    // Unordered or ordered list item: prefix + space + inline-formatted text.
    private static func listItem(_ text: String, prefix: String) -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.headIndent          = C.listIndent + 8
        style.firstLineHeadIndent = C.listIndent

        let result = NSMutableAttributedString(
            string: "\(prefix) ",
            attributes: [
                .font:            C.responseFont,
                .foregroundColor: C.responseColor,
                .paragraphStyle:  style,
            ]
        )
        let body = inlineFormatted(text, paragraphStyle: style)
        result.append(body)
        result.append(NSAttributedString(string: "\n", attributes: [
            .font: C.responseFont,
            .paragraphStyle: style,
        ]))
        return result
    }

    // Blank line — 8pt paragraph spacing before next block.
    private static func paragraph() -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.paragraphSpacingBefore = 8
        return NSAttributedString(string: "\n", attributes: [
            .font:           C.responseFont,
            .paragraphStyle: style,
        ])
    }

    private static func newline() -> NSAttributedString {
        NSAttributedString(string: "\n", attributes: [.font: C.responseFont])
    }

    // Parse "1. text", "12. text" — returns (number, text) or nil.
    private static func orderedListItem(_ line: String) -> (Int, String)? {
        guard let dotRange = line.range(of: ". "),
              let number   = Int(line[line.startIndex ..< dotRange.lowerBound])
        else { return nil }
        return (number, String(line[dotRange.upperBound...]))
    }
}
```

### 1c. Inline formatter

The inline pass is a manual scanner rather than `NSRegularExpression` — regex
requires escaping and error handling that adds noise for a small pattern set.

```swift
extension MarkdownRenderer {

    /// Apply bold, italic, and inline-code formatting within a single line of text.
    /// Unrecognised markers are emitted as-is (no data loss on malformed input).
    static func inlineFormatted(
        _ text:           String,
        paragraphStyle:   NSParagraphStyle? = nil
    ) -> NSAttributedString {
        let result = NSMutableAttributedString()
        var idx    = text.startIndex

        func baseAttrs(_ font: NSFont) -> [NSAttributedString.Key: Any] {
            var a: [NSAttributedString.Key: Any] = [
                .font:            font,
                .foregroundColor: C.responseColor,
            ]
            if let ps = paragraphStyle { a[.paragraphStyle] = ps }
            return a
        }

        while idx < text.endIndex {
            // ── Inline code: `...` ──────────────────────────────────────────
            if text[idx] == "`",
               let closeIdx = text[text.index(after: idx)...].firstIndex(of: "`") {
                let content = String(text[text.index(after: idx) ..< closeIdx])
                result.append(NSAttributedString(string: content, attributes: [
                    .font:            C.codeFont,
                    .foregroundColor: C.codeColor,
                ]))
                idx = text.index(after: closeIdx)
                continue
            }

            // ── Bold: **...** ───────────────────────────────────────────────
            if text[idx...].hasPrefix("**"),
               let closeRange = text[text.index(idx, offsetBy: 2)...].range(of: "**") {
                let from    = text.index(idx, offsetBy: 2)
                let content = String(text[from ..< closeRange.lowerBound])
                result.append(NSAttributedString(string: content,
                    attributes: baseAttrs(NSFont.systemFont(ofSize: 14, weight: .semibold))))
                idx = closeRange.upperBound
                continue
            }

            // ── Italic: *...* or _..._ ──────────────────────────────────────
            let isItalicMarker = (text[idx] == "*" && !text[idx...].hasPrefix("**"))
                              || text[idx] == "_"
            let marker: Character = text[idx] == "_" ? "_" : "*"
            if isItalicMarker,
               let closeIdx = text[text.index(after: idx)...].firstIndex(of: marker) {
                let content = String(text[text.index(after: idx) ..< closeIdx])
                // Derive italic variant of base font.
                let italicDesc  = C.responseFont.fontDescriptor
                    .withSymbolicTraits(.italic)
                let italicFont  = NSFont(descriptor: italicDesc, size: 14)
                               ?? C.responseFont
                result.append(NSAttributedString(string: content,
                    attributes: baseAttrs(italicFont)))
                idx = text.index(after: closeIdx)
                continue
            }

            // ── Plain character ─────────────────────────────────────────────
            result.append(NSAttributedString(
                string:     String(text[idx]),
                attributes: baseAttrs(C.responseFont)
            ))
            idx = text.index(after: idx)
        }

        return result
    }
}
```

---

## 2. `HUDTextView.swift` — changes

### 2a. Add state for deferred markdown render

```swift
// After the existing stored properties:

/// Raw markdown text accumulated during streaming.
/// Replaced by the formatted version in finalizeWithMarkdown() on is_final.
private var responseRawText = ""

/// Index into textStorage where the current response begins.
/// Set by markResponseStart(); used by finalizeWithMarkdown() to
/// replace only the response portion while preserving the operator turn.
private var responseStartLocation: Int = 0
```

### 2b. Add `markResponseStart()`

Called by `HUDWindow.beginResponseStreaming()` after any operator-turn text has been
appended — records the offset so `finalizeWithMarkdown()` knows where to cut.

```swift
/// Record the current end of textStorage as the start of the response region.
/// Must be called after showOperatorTurn() and before the first appendToken().
func markResponseStart() {
    responseRawText       = ""
    responseStartLocation = textView.textStorage?.length ?? 0
}
```

### 2c. Update `appendToken(_:)`

Accumulate raw text alongside the live plain-text append:

```swift
func appendToken(_ text: String) {
    responseRawText += text   // accumulate for deferred markdown render
    let attrs: [NSAttributedString.Key: Any] = [
        .font:            C.responseFont,
        .foregroundColor: C.responseColor,
    ]
    textView.textStorage?.append(NSAttributedString(string: text, attributes: attrs))
    textView.scrollToEndOfDocument(nil)
}
```

### 2d. Add `finalizeWithMarkdown()`

Replaces the streamed plain text with the rendered markdown version:

```swift
/// Replace the plain-text response region with formatted markdown.
///
/// Called by HUDWindow.responseComplete() on is_final. The operator-turn
/// prefix (before responseStartLocation) is preserved exactly; only the
/// response region is replaced. No-ops if no response text was accumulated.
func finalizeWithMarkdown() {
    guard !responseRawText.isEmpty,
          let storage = textView.textStorage else { return }

    let rendered = MarkdownRenderer.render(responseRawText)
    let range    = NSRange(
        location: responseStartLocation,
        length:   storage.length - responseStartLocation
    )
    storage.replaceCharacters(in: range, with: rendered)
    textView.scrollToEndOfDocument(nil)
}
```

### 2e. Update `clear()`

Reset the accumulated buffer and location tracking:

```swift
func clear() {
    textView.textStorage?.setAttributedString(NSAttributedString())
    responseRawText       = ""
    responseStartLocation = 0
}
```

---

## 3. `HUDWindow.swift` — changes

### 3a. Add styling constants to `enum C`

```swift
nonisolated(unsafe) static let codeFont = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
nonisolated(unsafe) static let h1Font   = NSFont.systemFont(ofSize: 18, weight: .bold)
nonisolated(unsafe) static let h2Font   = NSFont.systemFont(ofSize: 16, weight: .semibold)
nonisolated(unsafe) static let h3Font   = NSFont.systemFont(ofSize: 15, weight: .medium)
static let codeColor  = NSColor(white: 0.78, alpha: 1.0)
static let listIndent: CGFloat = 14
```

### 3b. Call `markResponseStart()` in `beginResponseStreaming()`

```swift
func beginResponseStreaming() {
    cancelDismiss()
    textArea.markResponseStart()   // ← add this line
    show()
}
```

### 3c. Call `finalizeWithMarkdown()` in `responseComplete()`

```swift
func responseComplete() {
    textArea.finalizeWithMarkdown()   // ← add this line
    scheduleDismiss()
}
```

---

## 4. Execution Order

1. Add styling constants to `enum C` in `HUDWindow.swift`
2. Write `HUD/MarkdownRenderer.swift` — entry point, block helpers, inline formatter
3. Edit `HUD/HUDTextView.swift` — add properties, `markResponseStart()`,
   update `appendToken()`, add `finalizeWithMarkdown()`, update `clear()`
4. Edit `HUD/HUDWindow.swift` — wire `markResponseStart()` + `finalizeWithMarkdown()`
5. `cd src/swift && swift build` — target: 0 errors, 0 warnings from project code

---

## 5. Acceptance Criteria

### Automated

`swift build` in `src/swift/` succeeds with 0 errors, 0 warnings from project code.

### Manual

| # | Criterion | Verification |
|---|-----------|-------------|
| 1 | Code blocks render monospace | Ask "how do I list files?" → `ls -la` renders in monospace, dimmed |
| 2 | Triple-fence delimiters gone | No literal ```` ``` ```` visible in HUD output |
| 3 | Inline code rendered | "Use `grep`" → `grep` in monospace, no backticks shown |
| 4 | Bold rendered | A `**bold**` phrase → heavier weight, no asterisks |
| 5 | Italic rendered | A `*italic*` phrase → italic style, no asterisks |
| 6 | Unordered list | "Steps:\n- One\n- Two" → bullet points with indent |
| 7 | Ordered list | "1. First\n2. Second" → numbered with indent |
| 8 | Header rendered | A `## Section` → larger/bolder font, no `##` visible |
| 9 | Streaming plain, final formatted | Watch HUD during a long response: plain text streams, snaps to formatted on completion |
| 10 | Operator turn preserved | "You: ..." prefix remains correctly styled after markdown snap |
| 11 | Malformed markdown safe | Unclosed `` ` `` or `**` renders as plain text, no crash |
| 12 | Empty response safe | `is_final` with empty content → no crash, no content change |

---

## 6. Key Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| Inline scanner gets stuck in an infinite loop on malformed input (e.g. unmatched `*`) | The `isItalicMarker` branch requires finding a closing marker before consuming the opener. If no close is found, the `*` falls through to the plain-character branch and `idx` advances by exactly 1 — loop always makes forward progress. |
| `replaceCharacters(in:with:)` range is invalid if `responseStartLocation > storage.length` | Guard: `range.location + range.length <= storage.length` — if violated (can't happen in normal flow but defensible), fall back to appending the rendered string rather than replacing. |
| `NSFont.monospacedSystemFont` unavailable on older macOS | macOS 10.15+; project targets macOS 15+. No risk. |
| `fontDescriptor.withSymbolicTraits(.italic)` returns nil for some system fonts | Nil-coalesces to `C.responseFont` — italic falls back to plain weight rather than crashing. |
| Long code blocks push the text view past `HUD_MAX_HEIGHT` | `NSScrollView` handles overflow with vertical scrolling — already configured in Phase 25. No additional work needed. |
| Markdown snap is visually jarring on short responses | For 1–3 token responses (e.g. "yes", "done"), the snap is imperceptible — the formatted and plain versions are identical for plain text. Only stylistically rich responses produce a visible change, which is the correct behavior. |
