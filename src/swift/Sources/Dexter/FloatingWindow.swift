import AppKit

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
    private var saveDebounceItem: DispatchWorkItem?

    private(set) var animatedEntity: AnimatedEntity!
    private(set) var hud: HUDWindow!

    // MARK: - Multi-monitor tracking

    // The screen Dexter is currently associated with. Updated on every manual drag
    // (windowDidMove) and at the start of flyToScreen so the follow-timer doesn't
    // re-trigger a flight that is already in progress or has just completed.
    private var lastTrackedScreen: NSScreen?

    // Set to true during flyToScreen's animation so windowDidMove doesn't
    // overwrite lastTrackedScreen mid-flight with the wrong intermediate screen.
    private var isAnimatingFlight: Bool = false

    // Polls NSEvent.mouseLocation; fires flyToScreen when the cursor crosses
    // to a different display. Retained for the process lifetime.
    private var screenFollowTimer: Timer?

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

        // Poll mouse position every 400 ms. Crossing to a new display triggers
        // a smooth flight to the upper-right corner of that display.
        // 400 ms is responsive without hammering the event system.
        screenFollowTimer = Timer.scheduledTimer(withTimeInterval: 0.4, repeats: true) {
            [weak self] _ in self?.checkScreenAndFollow()
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

    // MARK: - Multi-monitor follow

    private func checkScreenAndFollow() {
        guard !isAnimatingFlight else { return }
        let mouse = NSEvent.mouseLocation
        guard let target = NSScreen.screens.first(where: { $0.frame.contains(mouse) }) else { return }
        guard target != lastTrackedScreen else { return }
        flyToScreen(target)
    }

    /// Animate Dexter to the upper-right corner of `screen`.
    /// The HUD follows via the windowDidMove → hud.follow chain that fires
    /// for each intermediate frame during the NSAnimationContext animation.
    private func flyToScreen(_ screen: NSScreen) {
        // Lock the target now so the timer doesn't re-fire mid-flight.
        lastTrackedScreen  = screen
        isAnimatingFlight  = true

        let margin: CGFloat = 20
        let vf = screen.visibleFrame
        let target = NSRect(
            x: vf.maxX - frame.width  - margin,
            y: vf.maxY - frame.height - margin,
            width:  frame.width,
            height: frame.height
        )

        NSAnimationContext.runAnimationGroup({ ctx in
            ctx.duration       = 0.55
            ctx.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
            self.animator().setFrame(target, display: true)
        }, completionHandler: { [weak self] in
            guard let self else { return }
            self.isAnimatingFlight = false
            // Snap HUD to final position in case windowDidMove didn't fire
            // for every animated frame (AppKit does not guarantee this).
            self.hud.follow(entityFrame: self.frame)
            self.persistFrameNow()
        })
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
        // the entire 200×400pt window rect to intercept mouse events at the window-
        // server level, even though only the ~55pt-radius orb circle is visible.
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
        // is fixed (200×400pt) so Auto Layout constraints are unnecessary overhead.
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
        let frame = NSRectFromString(str)
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
    /// Window size matches the dimensions used in Phase 1 (200 × 400 points).
    ///
    /// AppKit window frames are always in points, not pixels. NSStringFromRect and
    /// NSRectFromString operate on the same point-coordinate system, so round-tripping
    /// through UserDefaults is exact.
    private static func defaultFrame() -> NSRect {
        let screen = NSScreen.main ?? NSScreen.screens[0]
        let size   = NSSize(width: 200, height: 400)
        return NSRect(
            x: screen.visibleFrame.midX - size.width  / 2,
            y: screen.visibleFrame.minY + 80,
            width:  size.width,
            height: size.height
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
        // Sync the tracked screen so the follow-timer doesn't fly Dexter back
        // if the operator manually drags him to a different display.
        // Suppressed during flyToScreen to avoid overwriting the destination
        // screen mid-animation with the source screen.
        if !isAnimatingFlight, let current = screen {
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
/// Implementation: override `hitTest(_:)`. Return nil to pass through, return self
/// (or the relevant subview) to claim the event. Pixel-level alpha is sampled from
/// a cached bitmap, rebuilt only when the view is marked dirty.
///
/// Caching strategy: `setNeedsDisplay(_:)` is the canonical AppKit invalidation
/// point — called by the system before redraws, on resize, and on backing store
/// changes. Overriding it (rather than the `needsDisplay` property) catches all
/// system-initiated invalidations, not just ones set via the property. The cache
/// is nil'd on every invalidation and rebuilt lazily on the next hit test.
/// Phase 12 (Metal rendering) inherits this mechanism without modification.
final class PassthroughView: NSView {

    /// Cached alpha bitmap. Nil means stale — rebuild on next hit test.
    /// Rebuilt at most once per display cycle, not once per mouse event.
    private var cachedBitmap: NSBitmapImageRep?

    // ── Invalidation ──────────────────────────────────────────────────────────

    override func setNeedsDisplay(_ invalidRect: NSRect) {
        // Any dirty region invalidates the whole hit-test cache.
        // Partial-rect precision isn't worth the complexity here —
        // a mouse event that falls outside the dirty rect is rare, and
        // a false pass-through is less bad than a stale hit claim.
        cachedBitmap = nil
        super.setNeedsDisplay(invalidRect)
    }

    override func setFrameSize(_ newSize: NSSize) {
        // Bounds changed → cached bitmap dimensions are wrong → discard.
        cachedBitmap = nil
        super.setFrameSize(newSize)
    }

    // ── Hit testing ───────────────────────────────────────────────────────────

    override func hitTest(_ point: NSPoint) -> NSView? {
        // Give subviews first opportunity to claim the event.
        for subview in subviews.reversed() {
            if let hit = subview.hitTest(convert(point, to: subview)) { return hit }
        }

        // Rebuild bitmap only when cache has been invalidated.
        // On a typical 60 Hz mouse-move sequence, this rebuilds at most once
        // per frame (when content changes), not 60+ times per frame.
        if cachedBitmap == nil {
            guard let bitmap = bitmapImageRepForCachingDisplay(in: bounds) else {
                return super.hitTest(point)
            }
            cacheDisplay(in: bounds, to: bitmap)
            cachedBitmap = bitmap
        }

        let pixelX = Int(point.x)
        let pixelY = Int(bounds.height - point.y)  // Flip: NSView is bottom-left origin
        let alpha  = cachedBitmap?.colorAt(x: pixelX, y: pixelY)?.alphaComponent ?? 0

        // Threshold at 1% — fully transparent pixels pass through.
        return alpha < 0.01 ? nil : self
    }

    override var isFlipped: Bool { false }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}
