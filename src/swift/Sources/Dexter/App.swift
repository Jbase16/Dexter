import AppKit

/// Application delegate — lifecycle callbacks only.
///
/// Entry point is in main.swift, which explicitly constructs NSApplication,
/// sets the activation policy before the run loop starts, and calls app.run().
/// @main is NOT used here because NSApplicationDelegate does not provide
/// static func main(), so @main would compile but never start the run loop.
final class DexterApp: NSObject, NSApplicationDelegate {

    private static let lifecycleConfirmationDelay: TimeInterval = 0.8

    private var floatingWindow: FloatingWindow?
    private var client: DexterClient?

    func applicationDidFinishLaunching(_ notification: Notification) {
        installApplicationMenu()

        let window = FloatingWindow()
        self.floatingWindow = window

        // orderFrontRegardless() bypasses the app-activation requirement.
        // makeKeyAndOrderFront(nil) silently fails when activation policy is
        // .accessory because the app cannot become the active application in
        // the traditional sense. orderFrontRegardless() is the correct call
        // for windows that must appear unconditionally.
        window.orderFrontRegardless()

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
                            markdown: """
                            ### \(target.buttonTitle)

                            Status: failed

                            Dexter client is not ready.
                            """,
                            restartTargets: [target]
                        )
                    await MainActor.run {
                        window?.hud.showHealthRestartResult(report, target: target)
                    }
                }
            }

            // Full app controls from the HUD. These mirror the app menu items but are
            // reachable from Dexter's own UI without making the app menu visible.
            window.hud.onDexterRestartRequest = { [weak self, weak window] in
                self?.beginDexterRestart(from: window)
            }

            window.hud.onDexterNewSessionRequest = { [weak self, weak c, weak window] in
                self?.beginNewSession(using: c, from: window)
            }

            window.hud.onDexterQuitRequest = { [weak self, weak window] in
                self?.beginDexterQuit(from: window)
            }

            if HUDSmokeConfig.enabled {
                HUDSmokeConfig.log(
                    "enabled text='\(HUDSmokeConfig.text)' health=\(HUDSmokeConfig.healthRequest) actionHistory=\(HUDSmokeConfig.actionHistoryRequest) actionDiagnostic=\(HUDSmokeConfig.actionDiagnosticRequest) ambientInbox=\(HUDSmokeConfig.ambientInboxRequest) diagnosticBundle=\(HUDSmokeConfig.diagnosticBundleRequest) newSession=\(HUDSmokeConfig.newSessionRequest) lifecycle=\(HUDSmokeConfig.lifecycleConfirmationAction ?? "none") restart=\(HUDSmokeConfig.restartComponent?.smokeName ?? "none") submitDelaySecs=\(HUDSmokeConfig.submitDelaySecs) sessionReadyTimeoutSecs=\(HUDSmokeConfig.sessionReadyTimeoutSecs) actionSurfaceSequenceDelaySecs=\(HUDSmokeConfig.actionSurfaceSequenceDelaySecs) exitAfterSecs=\(HUDSmokeConfig.exitAfterSecs)"
                )
                Task {
                    try? await Task.sleep(for: .seconds(HUDSmokeConfig.submitDelaySecs))
                    await MainActor.run {
                        if let placementSequence = HUDSmokeConfig.placementSequence {
                            window.performPlacementSmokeSequence(placementSequence)
                        } else if let lifecycleAction = HUDSmokeConfig.lifecycleAction {
                            HUDSmokeConfig.log("lifecycleActionRequest action=\(lifecycleAction)")
                            performLifecycleActionForSmoke(lifecycleAction, window: window)
                        } else if let lifecycleAction = HUDSmokeConfig.lifecycleConfirmationAction {
                            window.hud.performLifecycleConfirmationForSmoke(lifecycleAction)
                        } else if HUDSmokeConfig.idleOnly {
                            HUDSmokeConfig.log("idleOnly")
                        } else if HUDSmokeConfig.newSessionRequest {
                            window.hud.performNewSessionRequestForSmoke()
                        } else if HUDSmokeConfig.actionHistoryRequest && HUDSmokeConfig.actionDiagnosticRequest {
                            window.hud.performActionHistoryRequestForSmoke()
                            Task { @MainActor in
                                try? await Task.sleep(for: .seconds(HUDSmokeConfig.actionSurfaceSequenceDelaySecs))
                                window.hud.performActionDiagnosticRequestForSmoke()
                            }
                        } else if HUDSmokeConfig.actionDiagnosticRequest {
                            window.hud.performActionDiagnosticRequestForSmoke()
                        } else if HUDSmokeConfig.actionHistoryRequest {
                            window.hud.performActionHistoryRequestForSmoke()
                        } else if HUDSmokeConfig.healthRequest {
                            window.hud.performHealthRequestForSmoke()
                        } else if HUDSmokeConfig.ambientInboxRequest {
                            HUDSmokeConfig.log("ambientInboxRequest")
                            Task { [weak c, weak window] in
                                guard let markdown = await c?.fetchAmbientInboxMarkdownForSmoke() else { return }
                                await MainActor.run {
                                    window?.hud.showAmbientNotice(markdown)
                                }
                            }
                        } else if HUDSmokeConfig.diagnosticBundleRequest {
                            HUDSmokeConfig.log("diagnosticBundleRequest")
                            createDiagnosticBundle(nil)
                        } else {
                            HUDSmokeConfig.log("autoSubmit")
                            Task { [weak window] in
                                let sessionReady = await c.waitForSessionReadyForSmoke(
                                    timeoutSecs: HUDSmokeConfig.sessionReadyTimeoutSecs
                                )
                                await MainActor.run {
                                    if !sessionReady {
                                        HUDSmokeConfig.log("autoSubmitSkipped sessionReady=false")
                                    } else if let actionJSON = HUDSmokeConfig.actionJSON {
                                        HUDSmokeConfig.log("syntheticActionSubmit")
                                        Task { await c.sendSyntheticActionForSmoke(actionJSON) }
                                    } else if HUDSmokeConfig.fromVoice {
                                        window?.hud.showOperatorInput(HUDSmokeConfig.text)
                                        Task { await c.sendVoiceSmokeInput(HUDSmokeConfig.text) }
                                    } else {
                                        window?.hud.onTextSubmit?(HUDSmokeConfig.text)
                                    }
                                }
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
        if !HUDSmokeConfig.keepCoreOnExit {
            DexterProcessControl.terminateRustCore()
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool {
        // Dexter has no "last window" in the conventional sense.
        // The floating window closing should not terminate the process.
        false
    }

    @MainActor private func installApplicationMenu() {
        let mainMenu = NSMenu()

        let appMenuItem = NSMenuItem()
        mainMenu.addItem(appMenuItem)

        let appMenu = NSMenu(title: "Dexter")
        appMenuItem.submenu = appMenu

        appMenu.addItem(
            withTitle: "New Session",
            action: #selector(newSession(_:)),
            keyEquivalent: "n"
        ).target = self

        appMenu.addItem(NSMenuItem.separator())
        appMenu.addItem(
            withTitle: "Show Dexter Status",
            action: #selector(showDexterStatus(_:)),
            keyEquivalent: "s"
        ).target = self
        appMenu.addItem(
            withTitle: "Show Recent Actions",
            action: #selector(showRecentActions(_:)),
            keyEquivalent: "l"
        ).target = self
        appMenu.addItem(
            withTitle: "Explain Latest Action",
            action: #selector(explainLatestAction(_:)),
            keyEquivalent: "/"
        ).target = self
        appMenu.addItem(
            withTitle: "Create Diagnostic Bundle",
            action: #selector(createDiagnosticBundle(_:)),
            keyEquivalent: "b"
        ).target = self

        appMenu.addItem(NSMenuItem.separator())
        appMenu.addItem(
            withTitle: "Move Dexter to Mouse",
            action: #selector(moveDexterToMouse(_:)),
            keyEquivalent: "d"
        ).target = self
        appMenu.addItem(
            withTitle: "Toggle Dexter Placement Drag",
            action: #selector(toggleDexterPlacementDrag(_:)),
            keyEquivalent: ""
        ).target = self
        appMenu.addItem(
            withTitle: "Stop Dexter Placement Drag",
            action: #selector(stopDexterPlacementDrag(_:)),
            keyEquivalent: ""
        ).target = self

        appMenu.addItem(NSMenuItem.separator())
        appMenu.addItem(
            withTitle: "Restart Dexter",
            action: #selector(restartDexter(_:)),
            keyEquivalent: "r"
        ).target = self
        appMenu.addItem(NSMenuItem.separator())
        appMenu.addItem(
            withTitle: "Quit Dexter",
            action: #selector(quitDexter(_:)),
            keyEquivalent: "q"
        ).target = self

        NSApp.mainMenu = mainMenu
    }

    @MainActor @objc private func quitDexter(_ sender: Any?) {
        beginDexterQuit(from: floatingWindow)
    }

    @MainActor @objc private func restartDexter(_ sender: Any?) {
        beginDexterRestart(from: floatingWindow)
    }

    @MainActor @objc private func newSession(_ sender: Any?) {
        beginNewSession(using: client, from: floatingWindow)
    }

    @MainActor private func beginNewSession(using client: DexterClient?, from window: FloatingWindow?) {
        guard let client, let window else { return }
        window.hud.showNewSessionStarting()
        Task { [weak window] in
            let result = await client.startNewSessionAndWait()
            await MainActor.run {
                switch result {
                case .ready:
                    window?.hud.showNewSessionReady()
                case .failed(let reason):
                    window?.hud.showNewSessionFailed(reason)
                }
            }
        }
    }

    @MainActor @objc private func showDexterStatus(_ sender: Any?) {
        let targetWindow = floatingWindow
        targetWindow?.hud.showHealthLoading()
        Task { [weak self, weak targetWindow] in
            let report = await self?.client?.fetchOperatorStatusReport()
                ?? DexterHealthHUDReport(
                    markdown: DexterClient.unavailableHealthMarkdown(reason: "Dexter client is not ready."),
                    restartTargets: []
                )
            await MainActor.run {
                targetWindow?.hud.showHealthReport(report)
            }
        }
    }

    @MainActor @objc private func showRecentActions(_ sender: Any?) {
        let targetWindow = floatingWindow
        targetWindow?.hud.showActionHistoryLoading()
        Task { [weak self, weak targetWindow] in
            let markdown = await self?.client?.fetchActionHistoryMarkdown()
                ?? """
                ### Recent Actions

                Dexter client is not ready.
                """
            await MainActor.run {
                targetWindow?.hud.showActionHistory(markdown)
            }
        }
    }

    @MainActor @objc private func explainLatestAction(_ sender: Any?) {
        let targetWindow = floatingWindow
        targetWindow?.hud.showActionDiagnosticLoading()
        Task { [weak self, weak targetWindow] in
            let markdown = await self?.client?.fetchActionDiagnosticMarkdown()
                ?? """
                ### Action Diagnostic

                Dexter client is not ready.
                """
            await MainActor.run {
                targetWindow?.hud.showActionDiagnostic(markdown)
            }
        }
    }

    @MainActor @objc private func createDiagnosticBundle(_ sender: Any?) {
        let targetWindow = floatingWindow
        targetWindow?.hud.showDiagnosticBundleStarting()
        Task.detached(priority: .utility) {
            let markdown = DexterProcessControl.createDiagnosticBundleMarkdown()
            await MainActor.run {
                targetWindow?.hud.showDiagnosticBundleResult(markdown)
            }
        }
    }

    @MainActor private func performLifecycleActionForSmoke(_ action: String, window: FloatingWindow) {
        switch action.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "restart":
            beginDexterRestart(from: window)
        case "quit":
            beginDexterQuit(from: window)
        case "new_session", "new-session", "newsession", "session":
            beginNewSession(using: client, from: window)
        default:
            window.hud.performLifecycleConfirmationForSmoke(action)
        }
    }

    @MainActor @objc private func moveDexterToMouse(_ sender: Any?) {
        floatingWindow?.snapToCurrentMouseLocation()
    }

    @MainActor @objc private func startDexterPlacementDrag(_ sender: Any?) {
        floatingWindow?.setHotkeyRepositionActive(true)
    }

    @MainActor @objc private func toggleDexterPlacementDrag(_ sender: Any?) {
        floatingWindow?.toggleHotkeyReposition()
    }

    @MainActor @objc private func stopDexterPlacementDrag(_ sender: Any?) {
        floatingWindow?.setHotkeyRepositionActive(false)
    }

    @MainActor private func beginDexterRestart(from window: FloatingWindow?) {
        let targetWindow = window ?? floatingWindow
        targetWindow?.hud.showDexterRestarting()
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.lifecycleConfirmationDelay) {
            DexterProcessControl.openRestartTerminal()
            NSApp.terminate(nil)
        }
    }

    @MainActor private func beginDexterQuit(from window: FloatingWindow?) {
        let targetWindow = window ?? floatingWindow
        targetWindow?.hud.showDexterQuitting()
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.lifecycleConfirmationDelay) {
            NSApp.terminate(nil)
        }
    }
}

private enum DexterProcessControl {
    private static let repoPath = "/Users/jason/Developer/Dex"

    static func terminateRustCore() {
        run("/bin/bash", ["\(repoPath)/scripts/stop-dexter.sh", "--core-only", "--quiet"])
    }

    static func openRestartTerminal() {
        if writeRestartSentinelIfConfigured() {
            return
        }
        let command = shellQuote("\(repoPath)/scripts/restart-dexter-ui.sh")
        let script = """
        tell application "Terminal"
            activate
            set dexterTab to do script "\(appleScriptString(command))"
            set custom title of dexterTab to "Dexter Live Logs"
        end tell
        """
        run("/usr/bin/osascript", ["-e", script])
    }

    static func createDiagnosticBundleMarkdown() -> String {
        let result = runCapturing("/bin/bash", ["\(repoPath)/scripts/diagnostic-bundle.sh"])
        let output = [result.stdout, result.stderr]
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
            .joined(separator: "\n")

        guard result.exitCode == 0 else {
            return """
            ### Diagnostic Bundle

            Status: failed

            Exit status: \(result.exitCode)

            ```text
            \(output.isEmpty ? "No output returned." : output)
            ```
            """
        }

        let reportPath = diagnosticBundlePath(from: output, marker: "[INFO] diagnostic bundle written:")
        let latestPath = diagnosticBundlePath(from: output, marker: "[INFO] latest diagnostic bundle:")
        let reportLine = reportPath.map { "- Report: `\($0)`" } ?? "- Report: path unavailable"
        let latestLine = latestPath.map { "- Latest: `\($0)`" } ?? "- Latest: path unavailable"

        return """
        ### Diagnostic Bundle

        Status: created

        \(reportLine)
        \(latestLine)

        This bundle is local markdown for launch, model, process, health, and acceptance evidence.
        """
    }

    private static func run(_ executable: String, _ arguments: [String]) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        do {
            try process.run()
        } catch {
            print("[DexterProcessControl] failed to run \(executable): \(error)")
        }
    }

    private static func runCapturing(
        _ executable: String,
        _ arguments: [String]
    ) -> (exitCode: Int32, stdout: String, stderr: String) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.currentDirectoryURL = URL(fileURLWithPath: repoPath, isDirectory: true)
        process.environment = ProcessInfo.processInfo.environment

        let stdoutPipe = Pipe()
        let stderrPipe = Pipe()
        process.standardOutput = stdoutPipe
        process.standardError = stderrPipe

        do {
            try process.run()
            process.waitUntilExit()
        } catch {
            return (127, "", "failed to run \(executable): \(error)")
        }

        let stdout = String(
            data: stdoutPipe.fileHandleForReading.readDataToEndOfFile(),
            encoding: .utf8
        ) ?? ""
        let stderr = String(
            data: stderrPipe.fileHandleForReading.readDataToEndOfFile(),
            encoding: .utf8
        ) ?? ""
        return (process.terminationStatus, stdout, stderr)
    }

    private static func diagnosticBundlePath(from output: String, marker: String) -> String? {
        output
            .split(whereSeparator: \.isNewline)
            .map(String.init)
            .first { $0.hasPrefix(marker) }?
            .dropFirst(marker.count)
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private static func writeRestartSentinelIfConfigured() -> Bool {
        let key = "DEXTER_PROCESS_CONTROL_RESTART_SENTINEL"
        guard let path = ProcessInfo.processInfo.environment[key],
              !path.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return false
        }

        do {
            try "restart requested\n".write(
                toFile: path,
                atomically: true,
                encoding: .utf8
            )
            print("[DexterProcessControl] restart sentinel wrote \(path)")
        } catch {
            print("[DexterProcessControl] failed to write restart sentinel \(path): \(error)")
        }
        return true
    }

    private static func shellQuote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    private static func appleScriptString(_ value: String) -> String {
        value
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
            .replacingOccurrences(of: "\n", with: "\\n")
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

    static let actionJSON: String? = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_ACTION_JSON"] ?? ""
        let normalized = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return normalized.isEmpty ? nil : normalized
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

    static let diagnosticBundleRequest: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_DIAGNOSTIC_BUNDLE"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let ambientInboxRequest: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_AMBIENT_INBOX"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let newSessionRequest: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_NEW_SESSION"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let placementSequence: String? = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_PLACEMENT_SEQUENCE"] ?? ""
        let normalized = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return normalized.isEmpty ? nil : normalized
    }()

    static let lifecycleConfirmationAction: String? = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_LIFECYCLE_CONFIRMATION"] ?? ""
        let normalized = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return normalized.isEmpty ? nil : normalized
    }()

    static let lifecycleAction: String? = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_LIFECYCLE_ACTION"] ?? ""
        let normalized = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return normalized.isEmpty ? nil : normalized
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

    static let sessionReadyTimeoutSecs: Int64 = {
        parseSecs("DEXTER_HUD_SMOKE_SESSION_READY_TIMEOUT_SECS", defaultValue: 30)
    }()

    static let restartDelaySecs: Int64 = {
        parseSecs("DEXTER_HUD_SMOKE_RESTART_DELAY_SECS", defaultValue: 3)
    }()

    static let actionSurfaceSequenceDelaySecs: Int64 = {
        parseSecs("DEXTER_HUD_SMOKE_ACTION_SURFACE_SEQUENCE_DELAY_SECS", defaultValue: 6)
    }()

    static let exitAfterSecs: Int64 = {
        parseSecs("DEXTER_HUD_SMOKE_EXIT_AFTER_SECS", defaultValue: 18)
    }()

    static let fromVoice: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_FROM_VOICE"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static let keepCoreOnExit: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE_KEEP_CORE_ON_EXIT"] ?? ""
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
