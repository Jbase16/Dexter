import AppKit

/// Single-line text input field at the bottom of the HUD.
///
/// Calls `onSubmit` with the trimmed text when the operator presses Return,
/// then clears itself. Paste (Cmd+V) works via standard NSTextField behavior.
///
/// Does not activate the application — `HUDWindow` uses `.nonactivatingPanel`.
/// `acceptsFirstMouse(for:)` returns `true` so the operator can click-to-focus
/// in a single click without a separate activation click first.
final class HUDInputField: NSTextField, NSTextFieldDelegate {

    /// Called with the submitted text when the operator presses Return.
    /// Fired on the main thread; callers are responsible for Task-hopping to actors.
    var onSubmit: ((String) -> Void)?

    override init(frame: NSRect) {
        super.init(frame: frame)

        placeholderString = "Type a message…"
        isBordered        = false
        isBezeled         = false
        drawsBackground   = false
        textColor         = .white
        font              = C.responseFont

        // Subdued placeholder that's legible against the dark vibrancy background.
        (cell as? NSTextFieldCell)?.placeholderAttributedString = NSAttributedString(
            string: "Type a message…",
            attributes: [
                .foregroundColor: NSColor.white.withAlphaComponent(0.4),
                .font:            C.responseFont,
            ]
        )

        delegate = self
    }

    required init?(coder: NSCoder) { fatalError("IB not used") }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    // MARK: - Key equivalents

    /// Forward ⌘C/⌘V/⌘X/⌘A directly to the field editor, bypassing NSApplication's
    /// key-equivalent dispatch chain.
    ///
    /// NSPanel receives character keystrokes even when the app is not frontmost
    /// (documented Apple special-case for panels), but ⌘-sequences take a different
    /// path: NSApplication.sendEvent scans the menu bar for key equivalents first.
    /// When Dexter is not the active application, that scan short-circuits and the
    /// `copy:`/`paste:` actions never reach the field editor — which is why right-click
    /// (contextual menu, different event path) works but ⌘V does not.
    ///
    /// Solution: detect the ⌘ combo here and call the NSText action methods directly
    /// on currentEditor(), exactly as the contextual menu does internally.
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        // Only act when our field editor is active and ⌘ is the sole modifier.
        guard event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command,
              let editor = currentEditor()
        else { return super.performKeyEquivalent(with: event) }

        switch event.charactersIgnoringModifiers {
        case "v": editor.paste(nil);      return true
        case "c": editor.copy(nil);       return true
        case "x": editor.cut(nil);        return true
        case "a": editor.selectAll(nil);  return true
        default:  return super.performKeyEquivalent(with: event)
        }
    }

    // MARK: - NSTextFieldDelegate

    func control(
        _ control: NSControl,
        textView: NSTextView,
        doCommandBy selector: Selector
    ) -> Bool {
        guard selector == #selector(NSResponder.insertNewline(_:)) else { return false }
        let text = stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return true }
        onSubmit?(text)
        stringValue = ""
        return true
    }
}
