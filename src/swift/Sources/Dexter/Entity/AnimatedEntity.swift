import AppKit
import MetalKit

/// An `MTKView` that hosts Dexter's Metal-rendered animated presence.
///
/// `AnimatedEntity` is responsible for three things:
///   1. Owning and driving the active `EntityRenderer` (continuous 60 fps loop)
///   2. Exposing `entityState` as a settable property — `DexterClient` calls
///      `window.animatedEntity.entityState = .listening` from a `MainActor.run`
///      block and the next frame pick it up
///   3. Overriding `hitTest(_:)` with circle SDF math — `MTKView` renders through
///      `CAMetalLayer` which is not captured by `bitmapImageRepForCachingDisplay`,
///      so `PassthroughView`'s bitmap cache would see Metal pixels as fully
///      transparent and pass all clicks through. The circle SDF is exact and
///      requires no caching or bitmap capture.
final class AnimatedEntity: MTKView {

    // MARK: - Public API

    /// Current entity visual state. Updated by `DexterClient` on every
    /// `EntityStateChange` server event via `await MainActor.run { ... }`.
    /// The value is read once per frame in `draw(in:)` — no locking needed
    /// because `@MainActor` serialises all writes and the MTKView draw loop
    /// runs on the main thread.
    var entityState: EntityState = .idle

    /// Called when the operator double-clicks the entity.
    /// FloatingWindow sets this to toggle the HUD.
    var onDoubleTap: (() -> Void)?

    // MARK: - Private state

    private let entityRenderer: EntityRenderer

    // Drag tracking — captured in mouseDown, used in mouseDragged.
    // Screen-space coords avoid the feedback loop caused by locationInWindow
    // shifting as the window moves during a drag.
    private var dragScreenOrigin:  NSPoint = .zero   // mouse position at drag start
    private var windowOriginAtDrag: NSPoint = .zero  // window origin at drag start

    /// Monotonic start time used as the animation clock origin.
    /// `CACurrentMediaTime()` is used (not `Date`) for sub-millisecond
    /// precision and resistance to system clock adjustments.
    private let startTime: CFTimeInterval = CACurrentMediaTime()

    // MARK: - Init

    /// - Parameters:
    ///   - frame: Initial frame in superview coordinates (points, not pixels).
    ///   - renderer: The `EntityRenderer` to use. Defaults to `GeometricRenderer`.
    ///     Swappable for testing or future phases without touching this class.
    init(frame: NSRect, renderer: EntityRenderer = GeometricRenderer()) {
        guard let device = MTLCreateSystemDefaultDevice() else {
            // Metal is unavailable — this cannot occur on any Apple Silicon Mac.
            preconditionFailure("[AnimatedEntity] Metal is not available on this machine.")
        }
        self.entityRenderer = renderer
        super.init(frame: frame, device: device)

        // Fully transparent layer — the NSPanel window is clear; Metal must
        // composite its output against a zero-alpha background, not black.
        // isOpaque is a get-only computed property on NSView (overridden below);
        // transparency is also enforced at the CAMetalLayer level.
        clearColor      = MTLClearColorMake(0, 0, 0, 0)
        layer?.isOpaque = false

        // Continuous animation: disable the "only redraw when dirty" mode.
        // The entity animates continuously (breathing, pulsing) even without
        // external state changes, so a push-driven display link is needed.
        isPaused               = false
        enableSetNeedsDisplay  = false
        preferredFramesPerSecond = 60

        // Self is the MTKViewDelegate — receives draw(in:) and resize callbacks.
        delegate = self

        renderer.setup(device: device)
    }

    // MTKView's ObjC superclass declares initWithCoder: as `nonnull instancetype`
    // (non-failable). Swift requires the override to be non-failable to match.
    // The @available unavailable attribute documents that this path is never used.
    @available(*, unavailable)
    required init(coder: NSCoder) { fatalError("AnimatedEntity is not coder-constructible") }

    // MARK: - Layer configuration

    /// Re-apply the opacity flag every time the view enters (or re-enters) a window.
    ///
    /// `layer?.isOpaque = false` in `init` is correct in isolation, but AppKit may
    /// recreate the `CAMetalLayer` when the view is first inserted into a window's
    /// layer tree — resetting `isOpaque` to its default `true`. With `isOpaque = true`
    /// the compositor treats the Metal layer as opaque: the clear-colour background pixels
    /// (alpha = 0) composite as opaque black, which the transparent `NSPanel` then renders
    /// as fully invisible — producing "no disc at all" even when the shader is running.
    ///
    /// `viewDidMoveToWindow()` fires after the real `CAMetalLayer` is connected to
    /// the window's layer hierarchy, so the setting sticks.
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        layer?.isOpaque = false
    }

    // MARK: - Hit testing

    /// Circle SDF hit test matching the shader's rendered disc geometry.
    ///
    /// The shader places a disc of `baseRadius = 0.55` in aspect-correct space
    /// where the shorter dimension spans [-1, 1] (i.e. maps to 1.0 in shader
    /// coordinates). Converting to NSView points:
    ///
    ///   nsViewRadius = shaderBaseRadius × (min(width, height) / 2)
    ///                = 0.55 × (min(200, 400) / 2)   [for the 200×400 window]
    ///                = 0.55 × 100
    ///                = 55pt
    ///
    /// If `GeometricRenderer.shaderSource` changes `baseRadius`, update the
    /// constant here to match. Both are in the same file group for discoverability.
    override func hitTest(_ point: NSPoint) -> NSView? {
        // Give subviews first crack (none in Phase 12, but future-proof).
        for subview in subviews.reversed() {
            if let hit = subview.hitTest(convert(point, to: subview)) { return hit }
        }

        let center = NSPoint(x: bounds.midX, y: bounds.midY)
        // shaderBaseRadius = 0.55 — must match GeometricRenderer fragment shader
        let radius = 0.55 * min(bounds.width, bounds.height) / 2.0
        let dx = point.x - center.x
        let dy = point.y - center.y
        return (dx * dx + dy * dy) <= (radius * radius) ? self : nil
    }

    // NSView coordinate system is bottom-left origin; the shader flips y
    // internally so there is no mismatch in the hit-test geometry.
    override var isFlipped: Bool { false }

    // NSView.isOpaque is a get-only computed property — it cannot be set in init.
    // Returning false makes AppKit treat this view as transparent during compositing,
    // preventing it from painting a white rectangle behind the Metal content.
    override var isOpaque: Bool { false }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    // Disable the default "window moves on mouse-down" behaviour so that
    // mouseDown / mouseDragged / mouseUp are delivered directly to this view.
    // We re-implement dragging in mouseDragged below, giving us the full event
    // stream needed to detect a double-click on mouseUp.
    override var mouseDownCanMoveWindow: Bool { false }

    // MARK: - Drag & double-tap

    override func mouseDown(with event: NSEvent) {
        // Capture screen-space anchor so deltas stay correct as the window moves.
        dragScreenOrigin   = NSEvent.mouseLocation
        windowOriginAtDrag = window?.frame.origin ?? .zero
    }

    override func mouseDragged(with event: NSEvent) {
        guard let win = window else { return }
        let mouse = NSEvent.mouseLocation
        win.setFrameOrigin(NSPoint(
            x: windowOriginAtDrag.x + mouse.x - dragScreenOrigin.x,
            y: windowOriginAtDrag.y + mouse.y - dragScreenOrigin.y
        ))
    }

    override func mouseUp(with event: NSEvent) {
        // clickCount == 2 fires on the *second* up event within the double-click
        // interval — the first up event has clickCount == 1.
        if event.clickCount == 2 {
            onDoubleTap?()
        }
    }
}

// MARK: - MTKViewDelegate

extension AnimatedEntity: MTKViewDelegate {

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {
        // Notify renderer so it can update any aspect-ratio-sensitive state
        // before the next draw. Called on the main thread by MetalKit.
        entityRenderer.resize(drawableSize: size)
    }

    func draw(in view: MTKView) {
        let t = CACurrentMediaTime() - startTime
        entityRenderer.render(in: view, state: entityState, time: t)
    }
}
