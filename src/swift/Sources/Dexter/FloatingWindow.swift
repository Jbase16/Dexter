import AppKit

private enum FloatingWindowSmokeLog {
    static let enabled: Bool = {
        let raw = ProcessInfo.processInfo.environment["DEXTER_HUD_SMOKE"] ?? ""
        return ["1", "true", "yes"].contains(raw.lowercased())
    }()

    static func log(_ message: String) {
        guard enabled else { return }
        print("[HUDSmoke] \(message)")
    }
}

/// The always-present floating window that hosts Dexter's visual presence.
///
/// Uses NSPanel over NSWindow because NSPanel supports the .nonactivatingPanel
/// style mask, which prevents the panel from stealing keyboard focus from whatever
/// application the operator is actively using. An NSWindow at .screenSaver level
/// would steal focus on click — making Dexter actively obstructive rather than
/// present-but-passive.
final class FloatingWindow: NSPanel {

    // ── Position persistence (Phase 11) ──────────────────────────────────────
    //
    // Window frame is saved to UserDefaults as an NSStringFromRect string.
    // On every move, scheduleSaveFrame() debounces writes by saveDebounceMs
    // to avoid hitting UserDefaults on every drag pixel.
    //
    // loadOrDefaultFrame() validates the saved frame against connected screens
    // so a position stored for a now-disconnected monitor doesn't strand the window
    // off-screen on the next launch.
    private static let frameDefaultsKey = "com.dexter.windowFrame"
    private static let saveDebounceMs   = 250
    private static let entityWindowSize = NSSize(width: 136, height: 136)
    private var saveDebounceItem: DispatchWorkItem?

    private(set) var animatedEntity: AnimatedEntity!
    private(set) var hud: HUDWindow!

    // MARK: - Hotkey repositioning

    // The screen Dexter is currently associated with. Updated on every manual drag
    // and hotkey-reposition tick so ensureOnScreen and persistence remain anchored
    // to the operator's last intentional placement.
    private var lastTrackedScreen: NSScreen?

    // While the placement key is held, Dexter can be snapped to and dragged from
    // the current mouse location. This preserves the old "bring Dexter with me"
    // workflow, but only during an intentional placement gesture.
    private var hotkeyRepositionTimer: Timer?
    private var lastHotkeyMouseLocation: NSPoint?
    private var hotkeyRepositionActive = false
    private var hotkeyRepositionObserver: NSObjectProtocol?
    private var placementCommandObserver: NSObjectProtocol?

    init() {
        // Load last-known frame from UserDefaults; fall back to the default position
        // if this is the first launch or the saved center is off all connected screens.
        super.init(
            contentRect: FloatingWindow.loadOrDefaultFrame(),
            styleMask:   [.borderless, .nonactivatingPanel],
            backing:     .buffered,
            defer:       false
        )

        configureWindow()
        buildContentView()

        // HUDWindow is a separate NSPanel positioned to the left of the entity.
        // Created after buildContentView() so self.frame is valid for initial placement.
        hud = HUDWindow(entityWindow: self)

        // NSWindowDelegate receives windowDidMove() for position persistence.
        delegate = self

        // Wire entity double-tap: toggle HUD and focus input for immediate typing.
        animatedEntity.onDoubleTap = { [weak self] in self?.toggleHUD() }

        // Seed the tracked screen from the initial window position.
        lastTrackedScreen = self.screen

        // No passive screen-follow timer. Dexter now moves between displays only
        // during an intentional hotkey-held reposition gesture.
        hotkeyRepositionObserver = NotificationCenter.default.addObserver(
            forName: .dexterHotkeyRepositionChanged,
            object: nil,
            queue: .main
        ) { [weak self] notification in
            let active = notification.userInfo?["active"] as? Bool ?? false
            MainActor.assumeIsolated {
                self?.setHotkeyRepositionActive(active)
            }
        }

        placementCommandObserver = DistributedNotificationCenter.default().addObserver(
            forName: .dexterPlacementCommand,
            object: nil,
            queue: .main
        ) { [weak self] notification in
            let command = (notification.userInfo?["command"] as? String ?? "snap")
            MainActor.assumeIsolated {
                self?.handlePlacementCommand(command)
            }
        }
    }

    @MainActor deinit {
        hotkeyRepositionTimer?.invalidate()
        if let hotkeyRepositionObserver {
            NotificationCenter.default.removeObserver(hotkeyRepositionObserver)
        }
        if let placementCommandObserver {
            DistributedNotificationCenter.default().removeObserver(placementCommandObserver)
        }
    }

    // MARK: - HUD toggle

    private func toggleHUD() {
        if hud.isHUDVisible {
            hud.hideManual()
        } else {
            hud.showForTyping()
        }
    }

    // MARK: - Hotkey repositioning

    func setHotkeyRepositionActive(_ active: Bool) {
        guard active != hotkeyRepositionActive else { return }
        hotkeyRepositionActive = active

        if active {
            beginHotkeyReposition()
        } else {
            endHotkeyReposition()
        }
    }

    func snapToCurrentMouseLocation() {
        setHotkeyRepositionActive(false)
        snapToMouseLocation(NSEvent.mouseLocation)
    }

    func performPlacementSmokeSequence(_ rawSequence: String) {
        smokeLogPlacementSnapshot("initial")
        let commands = rawSequence
            .split { $0 == "," || $0 == " " || $0 == "\n" || $0 == "\t" }
            .map(String.init)

        for command in commands {
            handlePlacementCommand(command)
            smokeLogPlacementSnapshot("after-\(command.lowercased())")
        }

        setHotkeyRepositionActive(false)
        smokeLogPlacementSnapshot("final")
    }

    private func beginHotkeyReposition() {
        let mouse = NSEvent.mouseLocation
        snapToMouseLocation(mouse)
        lastHotkeyMouseLocation = mouse
        hotkeyRepositionTimer?.invalidate()

        let timer = Timer(timeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.applyHotkeyMouseDelta()
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        hotkeyRepositionTimer = timer
    }

    private func endHotkeyReposition() {
        hotkeyRepositionTimer?.invalidate()
        hotkeyRepositionTimer = nil
        lastHotkeyMouseLocation = nil
        persistFrameNow()
    }

    private func applyHotkeyMouseDelta() {
        applyHotkeyMouseDelta(
            currentMouseLocation: NSEvent.mouseLocation,
            primaryMouseButtonDown: isPrimaryMouseButtonDown
        )
    }

    private func applyHotkeyMouseDelta(currentMouseLocation current: NSPoint, primaryMouseButtonDown: Bool) {
        guard hotkeyRepositionActive,
              let previous = lastHotkeyMouseLocation else { return }

        guard primaryMouseButtonDown else {
            lastHotkeyMouseLocation = current
            return
        }

        let deltaX = current.x - previous.x
        let deltaY = current.y - previous.y

        guard abs(deltaX) >= 0.25 || abs(deltaY) >= 0.25 else { return }

        var nextFrame = frame
        nextFrame.origin.x += deltaX
        nextFrame.origin.y += deltaY
        setFrame(nextFrame, display: true)

        hud.follow(entityFrame: nextFrame)
        if let currentScreen = screen {
            lastTrackedScreen = currentScreen
        }
        lastHotkeyMouseLocation = current
    }

    private var isPrimaryMouseButtonDown: Bool {
        (NSEvent.pressedMouseButtons & 1) != 0
    }

    private func snapToMouseLocation(_ mouse: NSPoint) {
        let target = clampedFrameCentered(on: mouse)
        setFrame(target, display: true)
        hud.follow(entityFrame: target)
        if let currentScreen = screen {
            lastTrackedScreen = currentScreen
        }
        persistFrameNow()
    }

    private func clampedFrameCentered(on point: NSPoint) -> NSRect {
        var origin = NSPoint(
            x: point.x - frame.width / 2,
            y: point.y - frame.height / 2
        )

        if let screen = NSScreen.screens.first(where: { $0.frame.contains(point) }) {
            let vf = screen.visibleFrame
            origin.x = min(max(origin.x, vf.minX), vf.maxX - frame.width)
            origin.y = min(max(origin.y, vf.minY), vf.maxY - frame.height)
        }

        return NSRect(
            x: origin.x,
            y: origin.y,
            width: frame.width,
            height: frame.height
        )
    }

    private func handlePlacementCommand(_ rawCommand: String) {
        let command = rawCommand
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()

        switch command {
        case "snap":
            FloatingWindowSmokeLog.log("placement command=snap")
            snapToCurrentMouseLocation()
            smokeLogPlacementSnapshot("after-command-snap")
        case "start":
            FloatingWindowSmokeLog.log("placement command=start")
            setHotkeyRepositionActive(true)
            smokeLogPlacementSnapshot("after-command-start")
        case "stop":
            FloatingWindowSmokeLog.log("placement command=stop")
            setHotkeyRepositionActive(false)
            smokeLogPlacementSnapshot("after-command-stop")
        default:
            if command.hasPrefix("synthetic-nodrag:") {
                performSyntheticPlacementDelta(command, primaryMouseButtonDown: false)
            } else if command.hasPrefix("synthetic-drag:") {
                performSyntheticPlacementDelta(command, primaryMouseButtonDown: true)
            }
        }
    }

    private func performSyntheticPlacementDelta(_ command: String, primaryMouseButtonDown: Bool) {
        let parts = command.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count == 3,
              let dx = Double(parts[1]),
              let dy = Double(parts[2]) else {
            FloatingWindowSmokeLog.log("placement synthetic-invalid command=\(command)")
            return
        }

        let before = centeredSyntheticPlacementFrame()
        setFrame(before, display: true)
        hud.follow(entityFrame: before)

        hotkeyRepositionTimer?.invalidate()
        hotkeyRepositionTimer = nil
        hotkeyRepositionActive = true
        let anchor = NSPoint(x: before.midX, y: before.midY)
        lastHotkeyMouseLocation = anchor

        applyHotkeyMouseDelta(
            currentMouseLocation: NSPoint(x: anchor.x + dx, y: anchor.y + dy),
            primaryMouseButtonDown: primaryMouseButtonDown
        )

        let after = frame
        let actualDx = after.minX - before.minX
        let actualDy = after.minY - before.minY
        let moved = abs(actualDx) >= 0.25 || abs(actualDy) >= 0.25
        let label = primaryMouseButtonDown ? "synthetic-drag" : "synthetic-nodrag"
        FloatingWindowSmokeLog.log(
            String(
                format: "placement %@ expectedDx=%.1f expectedDy=%.1f actualDx=%.1f actualDy=%.1f moved=%@",
                label,
                dx,
                dy,
                actualDx,
                actualDy,
                moved ? "true" : "false"
            )
        )
    }

    private func centeredSyntheticPlacementFrame() -> NSRect {
        let screen = lastTrackedScreen ?? self.screen ?? NSScreen.main ?? NSScreen.screens[0]
        let vf = screen.visibleFrame
        return NSRect(
            x: round(vf.midX - frame.width / 2),
            y: round(vf.midY - frame.height / 2),
            width: frame.width,
            height: frame.height
        )
    }

    private func smokeLogPlacementSnapshot(_ label: String) {
        let contentBounds = contentView?.bounds ?? .zero
        let cornerHit = contentView?.hitTest(NSPoint(x: 1, y: 1)) != nil
        let centerHit = contentView?.hitTest(NSPoint(x: contentBounds.midX, y: contentBounds.midY)) != nil
        let topCenterHit = contentView?.hitTest(NSPoint(x: contentBounds.midX, y: contentBounds.maxY - 1)) != nil
        let bottomCenterHit = contentView?.hitTest(NSPoint(x: contentBounds.midX, y: contentBounds.minY + 1)) != nil
        let leftCenterHit = contentView?.hitTest(NSPoint(x: contentBounds.minX + 1, y: contentBounds.midY)) != nil
        let rightCenterHit = contentView?.hitTest(NSPoint(x: contentBounds.maxX - 1, y: contentBounds.midY)) != nil
        let f = frame
        FloatingWindowSmokeLog.log(
            "placement \(label) frame=\(NSStringFromRect(f)) size=\(Int(round(f.width)))x\(Int(round(f.height))) cornerHit=\(cornerHit) topCenterHit=\(topCenterHit) bottomCenterHit=\(bottomCenterHit) leftCenterHit=\(leftCenterHit) rightCenterHit=\(rightCenterHit) centerHit=\(centerHit) movableByBackground=\(isMovableByWindowBackground) ignoresMouse=\(ignoresMouseEvents)"
        )
    }

    // MARK: - Configuration

    private func configureWindow() {
        // Place Dexter above all application windows, the Dock, and the menu bar.
        // .screenSaver (level 1000) is the correct level for this — not .floating
        // (level 3, which sits above normal windows but below system chrome).
        level = .screenSaver

        isOpaque   = false
        hasShadow  = false  // Shadow rendered by Metal entity in later phases
        backgroundColor = .clear

        // Round 3 / behavioral fix: `isMovableByWindowBackground = true` was causing
        // the entire entity window rect to intercept mouse events at the window-
        // server level, even though only the orb circle is visible.
        // Result: a large invisible border around the orb blocked clicks to windows
        // below. Removing this is safe because AnimatedEntity already implements
        // mouseDown/mouseDragged/mouseUp for dragging, and its hitTest only claims
        // events inside the orb circle. Events outside the circle fall through
        // PassthroughView (bitmap alpha < 1%) → nil → passed to windows below.
        isMovableByWindowBackground = false

        // Behavior across Mission Control spaces and Exposé:
        // .canJoinAllSpaces  — present on every space, not just the one it was created on
        // .stationary        — does not move or disappear during Exposé/Mission Control
        // .ignoresCycle      — omitted from Cmd+Tab application switcher
        collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle]
    }

    private func buildContentView() {
        // PassthroughView provides selective mouse event pass-through:
        // transparent pixels forward clicks to windows below,
        // opaque pixels (Dexter's rendered form) receive events normally.
        let content = PassthroughView(frame: .zero)
        content.autoresizingMask = [.width, .height]
        contentView = content

        // AnimatedEntity is a Metal MTKView that fills the window.
        // It handles its own hit testing via circle SDF — PassthroughView's
        // bitmap cache cannot detect Metal pixels (rendered through CAMetalLayer,
        // not captured by bitmapImageRepForCachingDisplay).
        // autoresizingMask keeps it frame-locked to the panel; the window size
        // is fixed and square so Auto Layout constraints are unnecessary overhead.
        let entity = AnimatedEntity(frame: content.bounds)
        entity.autoresizingMask = [.width, .height]
        content.addSubview(entity)
        self.animatedEntity = entity
    }
    // MARK: - Position Persistence

    /// Load the saved frame from UserDefaults and validate it against connected screens.
    /// Returns `defaultFrame()` if: no value saved, string cannot be parsed,
    /// or the frame center does not lie within any screen.frame (monitor removed).
    private static func loadOrDefaultFrame() -> NSRect {
        guard let str = UserDefaults.standard.string(forKey: frameDefaultsKey),
              !str.isEmpty else { return defaultFrame() }
        let frame = FloatingWindow.normalizedFrame(NSRectFromString(str))
        // NSRectFromString returns NSZeroRect for an unparseable string.
        guard frame != .zero else { return defaultFrame() }
        let center = NSPoint(x: frame.midX, y: frame.midY)
        // A connected screen whose frame contains the saved center must exist;
        // otherwise the window would be invisible off-screen after monitor removal.
        guard NSScreen.screens.contains(where: { $0.frame.contains(center) }) else {
            return defaultFrame()
        }
        return frame
    }

    /// Horizontal center of the main screen's visible frame, 80pt above the bottom edge.
    /// The window is a tight square around the orb so transparent space above and
    /// below Dexter does not block clicks into underlying apps.
    ///
    /// AppKit window frames are always in points, not pixels. NSStringFromRect and
    /// NSRectFromString operate on the same point-coordinate system, so round-tripping
    /// through UserDefaults is exact.
    private static func defaultFrame() -> NSRect {
        let screen = NSScreen.main ?? NSScreen.screens[0]
        let size = entityWindowSize
        return NSRect(
            x: screen.visibleFrame.midX - size.width  / 2,
            y: screen.visibleFrame.minY + 80,
            width:  size.width,
            height: size.height
        )
    }

    private static func normalizedFrame(_ frame: NSRect) -> NSRect {
        guard frame != .zero else { return frame }
        let expected = entityWindowSize
        guard abs(frame.width - expected.width) > 0.5 || abs(frame.height - expected.height) > 0.5 else {
            return frame
        }
        return NSRect(
            x: frame.midX - expected.width / 2,
            y: frame.midY - expected.height / 2,
            width: expected.width,
            height: expected.height
        )
    }

    /// Re-validate window position against currently connected screens.
    /// Called by App.swift when NSApplication.didChangeScreenParametersNotification fires.
    /// Animates the window to the default position if its center is off all live screens.
    func ensureOnScreen() {
        let center = NSPoint(x: frame.midX, y: frame.midY)
        guard NSScreen.screens.contains(where: { $0.frame.contains(center) }) else {
            setFrame(FloatingWindow.defaultFrame(), display: true, animate: true)
            persistFrameNow()   // save the corrected position immediately (no debounce needed)
            return
        }
    }
}

// MARK: - NSWindowDelegate

// Extension keeps the delegation machinery separate from the window configuration.
// scheduleSaveFrame() uses a DispatchWorkItem debounce (saveDebounceMs = 250):
// cancel-and-reschedule on every drag pixel, fire once when dragging stops.
// This avoids synchronous UserDefaults writes (which block the main thread) on
// every NSMouseDragged event at 60+ Hz.
extension FloatingWindow: NSWindowDelegate {

    func windowDidMove(_ notification: Notification) {
        // The operator is dragging — debounce so we write UserDefaults at most
        // once per saveDebounceMs interval rather than on every drag pixel.
        scheduleSaveFrame()
        // Keep the HUD pinned to the left of the entity as the operator drags.
        hud.follow(entityFrame: frame)
        // Sync the tracked screen so Dexter's last intentional placement follows
        // manual drags and hotkey-held repositioning.
        if let current = screen {
            lastTrackedScreen = current
        }
    }

    private func scheduleSaveFrame() {
        saveDebounceItem?.cancel()
        let item = DispatchWorkItem { [weak self] in self?.persistFrameNow() }
        saveDebounceItem = item
        DispatchQueue.main.asyncAfter(
            deadline: .now() + .milliseconds(FloatingWindow.saveDebounceMs),
            execute:  item
        )
    }

    func persistFrameNow() {
        // NSStringFromRect produces a stable, human-readable string: "{{x, y}, {w, h}}".
        // NSRectFromString is the inverse — used in loadOrDefaultFrame().
        UserDefaults.standard.set(
            NSStringFromRect(frame),
            forKey: FloatingWindow.frameDefaultsKey
        )
    }
}

// MARK: - PassthroughView

/// A view that passes mouse events through to windows below when the pixel
/// under the cursor is fully transparent.
///
/// The alternative — `window.ignoresMouseEvents = true` — would make the entire
/// window click-through, including Dexter's rendered form. We need selective
/// pass-through: transparent regions forward to apps below, opaque regions
/// (where Dexter is rendered) receive events for drag, interaction, etc.
///
/// Implementation: override `hitTest(_:)`. Return nil to pass through, return the
/// relevant subview to claim the event. The root view itself never claims the
/// transparent background; Dexter's visible Metal orb is handled by
/// `AnimatedEntity.hitTest(_:)`.
final class PassthroughView: NSView {

    // ── Invalidation ──────────────────────────────────────────────────────────

    override func setNeedsDisplay(_ invalidRect: NSRect) {
        super.setNeedsDisplay(invalidRect)
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
    }

    // ── Hit testing ───────────────────────────────────────────────────────────

    override func hitTest(_ point: NSPoint) -> NSView? {
        guard bounds.contains(point) else { return nil }

        // Give subviews first opportunity to claim the event.
        for subview in subviews.reversed() {
            if let hit = subview.hitTest(convert(point, to: subview)) { return hit }
        }

        // The floating entity window has no clickable background. Returning self
        // here would turn the square NSPanel back into an invisible click blocker.
        return nil
    }

    override var isFlipped: Bool { false }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}
