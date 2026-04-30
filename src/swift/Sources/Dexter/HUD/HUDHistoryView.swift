import AppKit
import Foundation

/// Scrollable conversation history panel for the HUD.
///
/// Accumulates all completed exchanges in the current session as a persistent
/// NSAttributedString. Each turn is appended once via appendTurn(operatorText:responseText:)
/// when HUDWindow.responseComplete() fires — no re-rendering needed on show/hide.
///
/// History persists for the lifetime of the Swift process only. On Dexter restart,
/// history clears (the HUDWindow is rebuilt). Cross-session history lives in the
/// Rust vector store and retrieval pipeline; this is display-layer convenience.
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
        tv.isSelectable           = true   // operator can select and copy (Cmd+C)
        tv.backgroundColor        = .clear
        tv.drawsBackground        = false
        tv.textContainerInset     = NSSize(width: 10, height: 10)
        tv.isVerticallyResizable  = true
        tv.autoresizingMask       = [.width]
        // Wrap at view width; grow vertically without limit — scroll view clips the rest.
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

    // MARK: - Timestamp formatter

    private static let timestampFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f
    }()

    // MARK: - History API

    /// Append a completed turn to the persistent history.
    ///
    /// `operatorText` is empty for proactive responses — displayed as "(observation)".
    /// `responseText` is the raw markdown accumulated during streaming; re-rendered here
    /// using MarkdownRenderer so history matches the formatted output seen in the main HUD.
    ///
    /// Two newlines provide visual breathing room between turns; not added before the
    /// very first entry.
    func appendTurn(operatorText: String, responseText: String) {
        guard let storage = textView.textStorage else { return }

        let result = NSMutableAttributedString()

        // Inter-turn spacer — not added before the first entry.
        if storage.length > 0 {
            result.append(NSAttributedString(
                string: "\n\n",
                attributes: [.font: C.responseFont, .foregroundColor: C.responseColor]
            ))
        }

        // Timestamp — dimmed, shown to the right of the operator label.
        let timestamp = HUDHistoryView.timestampFormatter.string(from: Date())
        let timestampAttr = NSAttributedString(
            string: timestamp,
            attributes: [
                .font:            NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .regular),
                .foregroundColor: NSColor.white.withAlphaComponent(0.25),
            ]
        )

        // Operator line: "You: <text>  HH:MM" or "(observation)  HH:MM".
        let displayLabel = operatorText.isEmpty ? "(observation)" : "You: \(operatorText)"
        let headerLine = NSMutableAttributedString(string: "\(displayLabel)  ", attributes: [
            .font:            C.operatorFont,
            .foregroundColor: C.operatorColor,
        ])
        headerLine.append(timestampAttr)
        headerLine.append(NSAttributedString(
            string: "\n",
            attributes: [.font: C.operatorFont, .foregroundColor: C.operatorColor]
        ))
        result.append(headerLine)

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
