import AppKit
import AVFoundation

/// Application delegate — lifecycle callbacks only.
///
/// Entry point is in main.swift, which explicitly constructs NSApplication,
/// sets the activation policy before the run loop starts, and calls app.run().
/// @main is NOT used here because NSApplicationDelegate does not provide
/// static func main(), so @main would compile but never start the run loop.
final class DexterApp: NSObject, NSApplicationDelegate {

    private var floatingWindow: FloatingWindow?
    private var client: DexterClient?

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = FloatingWindow()
        self.floatingWindow = window

        // orderFrontRegardless() bypasses the app-activation requirement.
        // makeKeyAndOrderFront(nil) silently fails when activation policy is
        // .accessory because the app cannot become the active application in
        // the traditional sense. orderFrontRegardless() is the correct call
        // for windows that must appear unconditionally.
        window.orderFrontRegardless()

        // ── Microphone permission (Phase 13) ─────────────────────────────────
        //
        // Request microphone access before DexterClient starts. AVCaptureSession
        // silently produces no audio if permission is denied — surfacing the denial
        // dialog at launch makes the requirement clear to the operator.
        //
        // The request is non-blocking: the system shows the dialog asynchronously
        // and the completion handler fires on an arbitrary thread. DexterClient's
        // VoiceCapture will call AVCaptureDevice.default(for: .audio) which returns
        // nil if access is denied — VoiceCapture degrades gracefully (text-only).
        AVCaptureDevice.requestAccess(for: .audio) { granted in
            if !granted {
                DispatchQueue.main.async {
                    let alert = NSAlert()
                    alert.messageText     = "Microphone Access Required"
                    alert.informativeText = "Dexter needs microphone access for voice interaction. " +
                                           "Grant access in System Settings → Privacy & Security → Microphone."
                    alert.addButton(withTitle: "OK")
                    alert.runModal()
                }
            }
        }

        // Connect to Rust core in the background.
        // DexterClient handles retry on connection failure — core may not be up yet.
        Task {
            let c = DexterClient()
            self.client = c

            // Phase 25: bridge typed input from HUD to the inference pipeline.
            // The closure fires on the main thread (NSTextField delegate); Task { await }
            // hops to the DexterClient actor executor — the established actor-hopping pattern.
            // showOperatorInput is called first (main thread, safe) so the HUD appears and
            // displays the typed text before the response arrives — mirrors the voice path.
            window.hud.onTextSubmit = { [weak c, weak window] text in
                print("[App] onTextSubmit fired: '\(text)' | c=\(c != nil ? "live" : "NIL") | window=\(window != nil ? "live" : "NIL")")
                window?.hud.showOperatorInput(text)
                Task { await c?.sendTypedInput(text) }
            }

            // Wire the HUD mute button to DexterClient's TTS gate.
            window.hud.onMuteToggle = { [weak c] muted in
                Task { await c?.setTtsMuted(muted) }
            }

            await c.connect(to: window)
        }

        // When the operator connects or disconnects a monitor, re-validate the window
        // position to ensure it stays on a live screen.
        //
        // Selector-based addObserver avoids the @Sendable closure constraint imposed by
        // the closure-based overload — NSApplicationDelegate is @MainActor, so the
        // selector target runs on the main actor without any concurrency annotations.
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(screenParametersDidChange),
            name:     NSApplication.didChangeScreenParametersNotification,
            object:   nil
        )
    }

    // @MainActor is required: @objc methods do not automatically inherit the actor
    // isolation of their enclosing @MainActor class in Swift 6. Explicit annotation
    // allows calling @MainActor-isolated FloatingWindow.ensureOnScreen() synchronously.
    @MainActor @objc private func screenParametersDidChange() {
        floatingWindow?.ensureOnScreen()
    }

    func applicationWillTerminate(_ notification: Notification) {
        // The 250ms debounce in FloatingWindow.scheduleSaveFrame() may not fire before
        // process exit if the operator quits immediately after dragging. Flush synchronously
        // here to guarantee the last-known position is persisted on every clean shutdown.
        // persistFrameNow() is idempotent — a redundant call is a harmless no-op write.
        floatingWindow?.persistFrameNow()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool {
        // Dexter has no "last window" in the conventional sense.
        // The floating window closing should not terminate the process.
        false
    }
}
