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

            // On-demand operator status: cheap unary Health + ActionHistory RPCs, rendered
            // locally in the HUD. This does not touch the session stream, model router,
            // TTS, or action pipeline.
            window.hud.onHealthRequest = { [weak c, weak window] in
                window?.hud.showHealthLoading()
                Task { [weak c, weak window] in
                    let report = await c?.fetchOperatorStatusReport()
                        ?? DexterHealthHUDReport(
                            markdown: DexterClient.unavailableHealthMarkdown(reason: "Dexter client is not ready."),
                            restartTargets: []
                        )
                    await MainActor.run {
                        window?.hud.showHealthReport(report)
                    }
                }
            }

            // On-demand recent action receipts: cheap unary ActionHistory RPC,
            // rendered locally from the append-only Rust audit log.
            window.hud.onActionHistoryRequest = { [weak c, weak window] in
                window?.hud.showActionHistoryLoading()
                Task { [weak c, weak window] in
                    let markdown = await c?.fetchActionHistoryMarkdown()
                        ?? """
                        ### Recent Actions

                        Dexter client is not ready.
                        """
                    await MainActor.run {
                        window?.hud.showActionHistory(markdown)
                    }
                }
            }

            // On-demand "why did/didn't that action run?": rendered in the HUD,
            // but classified by Rust's daemon-backed ActionDiagnostic RPC.
            window.hud.onActionDiagnosticRequest = { [weak c, weak window] in
                window?.hud.showActionDiagnosticLoading()
                Task { [weak c, weak window] in
                    let markdown = await c?.fetchActionDiagnosticMarkdown()
                        ?? """
                        ### Action Diagnostic

                        Dexter client is not ready.
                        """
                    await MainActor.run {
                        window?.hud.showActionDiagnostic(markdown)
                    }
                }
            }

            // Operator-triggered worker recovery from the HUD health surface.
            // This is a daemon recovery RPC, not a model action and not an inferred side effect.
            window.hud.onHealthRestartRequest = { [weak c, weak window] target in
                window?.hud.showHealthRestarting(target)
                Task { [weak c, weak window] in
                    let report = await c?.restartWorkerAndFetchHealthReport(target)
                        ?? DexterHealthHUDReport(
                            markdown: DexterClient.unavailableHealthMarkdown(reason: "Dexter client is not ready."),
                            restartTargets: [target]
                        )
                    await MainActor.run {
                        window?.hud.showHealthReport(report)
                    }
                }
            }

            if HUDSmokeConfig.enabled {
                HUDSmokeConfig.log(
                    "enabled text='\(HUDSmokeConfig.text)' health=\(HUDSmokeConfig.healthRequest) actionHistory=\(HUDSmokeConfig.actionHistoryRequest) actionDiagnostic=\(HUDSmokeConfig.actionDiagnosticRequest) restart=\(HUDSmokeConfig.restartComponent?.smokeName ?? "none") submitDelaySecs=\(HUDSmokeConfig.submitDelaySecs) exitAfterSecs=\(HUDSmokeConfig.exitAfterSecs)"
                )
                Task {
                    try? await Task.sleep(for: .seconds(HUDSmokeConfig.submitDelaySecs))
                    await MainActor.run {
                        if HUDSmokeConfig.idleOnly {
                            HUDSmokeConfig.log("idleOnly")
                        } else if HUDSmokeConfig.actionDiagnosticRequest {
                            window.hud.performActionDiagnosticRequestForSmoke()
                        } else if HUDSmokeConfig.actionHistoryRequest {
                            window.hud.performActionHistoryRequestForSmoke()
                        } else if HUDSmokeConfig.healthRequest {
                            window.hud.performHealthRequestForSmoke()
                        } else {
                            HUDSmokeConfig.log("autoSubmit")
                            if HUDSmokeConfig.fromVoice {
                                window.hud.showOperatorInput(HUDSmokeConfig.text)
                                Task { await c.sendVoiceSmokeInput(HUDSmokeConfig.text) }
                            } else {
                                window.hud.onTextSubmit?(HUDSmokeConfig.text)
                            }
                        }
                    }

                    if let restartComponent = HUDSmokeConfig.restartComponent {
                        try? await Task.sleep(for: .seconds(HUDSmokeConfig.restartDelaySecs))
                        await MainActor.run {
                            window.hud.performHealthRestartForSmoke(restartComponent)
                        }
                    }

                    try? await Task.sleep(for: .seconds(HUDSmokeConfig.exitAfterSecs))
                    await MainActor.run {
                        HUDSmokeConfig.log("terminating")
                        NSApp.terminate(nil)
                    }
                }
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

private enum HUDSmokeConfig {
    static let enabled: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let text: String = {
        ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_TEXT"] ?? "what's 2 plus 2"
    }()

    static let healthRequest: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_HEALTH"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let actionHistoryRequest: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_ACTION_HISTORY"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let actionDiagnosticRequest: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_ACTION_DIAGNOSTIC"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let idleOnly: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_IDLE_ONLY"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let restartComponent: DexterWorkerRestartTarget? = {
        guard let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_RESTART_COMPONENT"],
              !raw.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return nil
        }
        return DexterWorkerRestartTarget.parse(raw)
    }()

    static let submitDelaySecs: Int64 = {
        parseSecs("DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS", defaultValue: 3)
    }()

    static let restartDelaySecs: Int64 = {
        parseSecs("DEXTER_HUD_SMOKE_RESTART_DELAY_SECS", defaultValue: 3)
    }()

    static let exitAfterSecs: Int64 = {
        parseSecs("DEXTER_HUD_SMOKE_EXIT_AFTER_SECS", defaultValue: 18)
    }()

    static let fromVoice: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_FROM_VOICE"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static func log(_ message: String) {
        guard enabled else { return }
        print("[HUDSmoke] \(message)")
    }

    private static func parseSecs(_ key: String, defaultValue: Int64) -> Int64 {
        guard let raw = ProcessInfo.processInfo.environment[key],
              let value = Int64(raw),
              value >= 0 else {
            return defaultValue
        }
        return value
    }
}
