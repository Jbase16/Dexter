import AppKit

/// Converts a markdown string to a styled NSAttributedString for display in HUDTextView.
///
/// ## Supported syntax
///
/// Block elements: fenced code blocks (```), H1–H3 (#, ##, ###), unordered lists (-, *),
/// ordered lists (1. 2. …), blank lines as paragraph breaks.
///
/// Inline elements: **bold**, *italic*/_italic_, `inline code`.
///
/// Everything else renders as plain prose. Malformed markdown never crashes — unclosed
/// fences emit as code, unmatched inline markers fall through to plain text.
///
/// ## Design notes
///
/// Called once per response on is_final. Not used during streaming — tokens stream as
/// plain text for immediate feedback, then this renderer replaces them on completion.
///
/// Prose lines include paragraph spacing (`proseLineSpacing`) so responses don't collapse
/// into a single unreadable wall of text. Code blocks get a background tint and monospace
/// font to visually separate terminal output from narrative.
enum MarkdownRenderer {

    // MARK: - Entry point

    static func render(_ markdown: String) -> NSAttributedString {
        let result = NSMutableAttributedString()
        let lines  = markdown.components(separatedBy: "\n")

        var inCodeBlock = false
        var codeLines:   [String] = []
        // Language hint on the opening ``` line is stored but currently unused —
        // retained for future syntax-highlighting without a protocol change.
        var codeLang: String = ""

        for line in lines {
            // ── Fenced code block boundary ───────────────────────────────────────
            if line.hasPrefix("```") {
                if inCodeBlock {
                    result.append(codeBlock(codeLines, lang: codeLang))
                    codeLines   = []
                    codeLang    = ""
                    inCodeBlock = false
                } else {
                    inCodeBlock = true
                    codeLang    = String(line.dropFirst(3)).trimmingCharacters(in: .whitespaces)
                }
                continue
            }

            if inCodeBlock {
                codeLines.append(line)
                continue
            }

            // ── Block-level elements ─────────────────────────────────────────────
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
                result.append(paragraphBreak())
            } else {
                // Normal prose — apply inline formatting with paragraph spacing.
                result.append(inlineFormatted(line))
                result.append(newline())
            }
        }

        // Flush any unclosed fenced block (malformed markdown — emit as code rather than discard).
        if inCodeBlock && !codeLines.isEmpty {
            result.append(codeBlock(codeLines, lang: codeLang))
        }

        return result
    }
}

// MARK: - Block helpers

extension MarkdownRenderer {

    /// Fenced code block: monospace font, background tint, left indent.
    ///
    /// The background tint visually separates terminal/command output from prose
    /// so the operator can immediately tell where explanation ends and output begins.
    /// Lines are padded to a minimum width so the background fill reads as a solid block.
    private static func codeBlock(_ lines: [String], lang: String = "") -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.headIndent          = C.codeIndent
        style.firstLineHeadIndent = C.codeIndent
        style.paragraphSpacing    = 0
        style.lineSpacing         = 1.5

        let textAttrs: [NSAttributedString.Key: Any] = [
            .font:            C.codeFont,
            .foregroundColor: C.codeColor,
            .backgroundColor: C.codeBackground,
            .paragraphStyle:  style,
        ]

        // Empty-background spacer style (background fills, no visible characters other than newline).
        let spacerAttrs: [NSAttributedString.Key: Any] = [
            .font:            C.codeFont,
            .backgroundColor: C.codeBackground,
            .paragraphStyle:  style,
        ]

        let block = NSMutableAttributedString()
        // Leading spacer — fills the background before the first line of code.
        block.append(NSAttributedString(string: "\n", attributes: spacerAttrs))

        for line in lines {
            // Pad empty lines to a single space so the background tint is still visible.
            let content = line.isEmpty ? " " : line
            block.append(NSAttributedString(string: content + "\n", attributes: textAttrs))
        }

        // Trailing spacer — fills the background below the last line.
        block.append(NSAttributedString(string: "\n", attributes: spacerAttrs))
        return block
    }

    /// Header: bold/semibold font with extra spacing before and after.
    private static func header(_ text: String, font: NSFont) -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.paragraphSpacingBefore = 8
        style.paragraphSpacing       = 4
        return NSAttributedString(string: text + "\n", attributes: [
            .font:            font,
            .foregroundColor: C.responseColor,
            .paragraphStyle:  style,
        ])
    }

    /// Unordered or ordered list item: prefix bullet/number + inline-formatted body.
    private static func listItem(_ text: String, prefix: String) -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        // firstLineHeadIndent positions the bullet; headIndent wraps continuation lines.
        style.headIndent          = C.listIndent + 8
        style.firstLineHeadIndent = C.listIndent
        style.paragraphSpacing    = C.proseLineSpacing

        let result = NSMutableAttributedString(
            string: "\(prefix) ",
            attributes: [
                .font:            C.responseFont,
                .foregroundColor: C.responseColor,
                .paragraphStyle:  style,
            ]
        )
        // Apply inline formatting to the body text, carrying the paragraph style.
        result.append(inlineFormatted(text, paragraphStyle: style))
        result.append(NSAttributedString(string: "\n", attributes: [
            .font:           C.responseFont,
            .paragraphStyle: style,
        ]))
        return result
    }

    /// Blank line — inserts a paragraph gap between content blocks.
    private static func paragraphBreak() -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.paragraphSpacingBefore = 8
        return NSAttributedString(string: "\n", attributes: [
            .font:           C.responseFont,
            .paragraphStyle: style,
        ])
    }

    /// Plain newline with prose paragraph spacing applied.
    ///
    /// `proseLineSpacing` is added as `paragraphSpacing` on every prose line so that
    /// multi-sentence responses breathe rather than collapsing into a single block.
    private static func newline() -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.paragraphSpacing = C.proseLineSpacing
        return NSAttributedString(string: "\n", attributes: [
            .font:           C.responseFont,
            .paragraphStyle: style,
        ])
    }

    /// Parse "1. text", "12. text" etc. Returns (number, bodyText) or nil.
    ///
    /// The prefix before the first ". " must be non-empty and consist entirely of
    /// digit characters — this anchors the match to genuine list items and prevents
    /// silent data loss on lines like "Python 2. The Basics", where range(of: ". ")
    /// would otherwise find "2. " mid-line, discarding "Python ".
    private static func orderedListItem(_ line: String) -> (Int, String)? {
        guard let dotRange = line.range(of: ". ") else { return nil }
        let prefix = line[line.startIndex ..< dotRange.lowerBound]
        guard !prefix.isEmpty,
              prefix.allSatisfy(\.isNumber),
              let number = Int(prefix)
        else { return nil }
        return (number, String(line[dotRange.upperBound...]))
    }
}

// MARK: - Inline formatter

extension MarkdownRenderer {

    /// Apply bold, italic, and inline-code formatting within a single line of text.
    ///
    /// Uses a manual forward scanner rather than NSRegularExpression — the pattern set
    /// is small, and the scanner's safety property (every branch advances `idx` by ≥ 1)
    /// is trivially provable, unlike a regex with optional groups.
    ///
    /// Unrecognised or unmatched markers fall through to the plain-character branch,
    /// advancing `idx` by exactly 1 — loop always terminates and no input is lost.
    static func inlineFormatted(
        _ text:          String,
        paragraphStyle:  NSParagraphStyle? = nil
    ) -> NSAttributedString {
        let result = NSMutableAttributedString()
        var idx    = text.startIndex

        // Default paragraph style for prose — adds breathing room between lines.
        let proseParagraph: NSParagraphStyle = {
            if let ps = paragraphStyle { return ps }
            let style = NSMutableParagraphStyle()
            style.paragraphSpacing = C.proseLineSpacing
            return style
        }()

        // Build attribute dictionaries with the optional paragraph style baked in.
        func attrs(_ font: NSFont, color: NSColor = C.responseColor,
                   bg: NSColor? = nil) -> [NSAttributedString.Key: Any] {
            var a: [NSAttributedString.Key: Any] = [
                .font:            font,
                .foregroundColor: color,
                .paragraphStyle:  proseParagraph,
            ]
            if let bg { a[.backgroundColor] = bg }
            return a
        }

        while idx < text.endIndex {

            // ── Inline code: `…` ───────────────────────────────────────────────
            if text[idx] == "`" {
                let afterOpen = text.index(after: idx)
                if afterOpen < text.endIndex,
                   let closeIdx = text[afterOpen...].firstIndex(of: "`") {
                    let content = String(text[afterOpen ..< closeIdx])
                    result.append(NSAttributedString(
                        string:     content,
                        attributes: attrs(C.codeFont, color: C.inlineCodeColor,
                                         bg: C.codeBackground)
                    ))
                    idx = text.index(after: closeIdx)
                    continue
                }
                // No closing backtick — fall through to plain character below.
            }

            // ── Bold: **…** ────────────────────────────────────────────────────
            if text[idx...].hasPrefix("**") {
                let afterOpen = text.index(idx, offsetBy: 2)
                if afterOpen < text.endIndex,
                   let closeRange = text[afterOpen...].range(of: "**") {
                    let content = String(text[afterOpen ..< closeRange.lowerBound])
                    result.append(NSAttributedString(
                        string:     content,
                        attributes: attrs(NSFont.systemFont(ofSize: 14, weight: .semibold))
                    ))
                    idx = closeRange.upperBound
                    continue
                }
                // No closing ** — fall through to plain character below.
            }

            // ── Italic: *…* or _…_ ────────────────────────────────────────────
            // Guard: a lone `*` that isn't part of `**` is an italic opener.
            let isStar       = text[idx] == "*" && !text[idx...].hasPrefix("**")
            let isUnderscore = text[idx] == "_"
            if isStar || isUnderscore {
                let marker: Character = isUnderscore ? "_" : "*"
                let afterOpen = text.index(after: idx)
                if afterOpen < text.endIndex,
                   let closeIdx = text[afterOpen...].firstIndex(of: marker) {
                    let content    = String(text[afterOpen ..< closeIdx])
                    // withSymbolicTraits returns nil for some system fonts — fall back
                    // to the base weight rather than crashing.
                    let italicDesc = C.responseFont.fontDescriptor.withSymbolicTraits(.italic)
                    let italicFont = NSFont(descriptor: italicDesc, size: 14) ?? C.responseFont
                    result.append(NSAttributedString(
                        string:     content,
                        attributes: attrs(italicFont)
                    ))
                    idx = text.index(after: closeIdx)
                    continue
                }
                // No closing marker — fall through to plain character below.
            }

            // ── Plain character ────────────────────────────────────────────────
            result.append(NSAttributedString(
                string:     String(text[idx]),
                attributes: attrs(C.responseFont)
            ))
            idx = text.index(after: idx)
        }

        return result
    }
}
