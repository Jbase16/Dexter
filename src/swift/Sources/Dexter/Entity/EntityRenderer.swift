import Metal
import MetalKit

/// Pluggable visual renderer for AnimatedEntity.
///
/// Phase 12 ships `GeometricRenderer` — an SDF Metal disc with per-state
/// animation. Later phases can swap in a different renderer (e.g. a
/// neural-network-driven particle system) by conforming to this protocol
/// and passing the instance to `AnimatedEntity(frame:renderer:)`.
///
/// Separation rationale (IMPLEMENTATION_PLAN.md §2.2.2): visuals must be
/// swappable without touching `AnimatedEntity` or `FloatingWindow`. The
/// protocol is the boundary that enforces this — no concrete renderer type
/// appears outside of `AnimatedEntity`'s init default argument.
protocol EntityRenderer: AnyObject {

    /// One-time device setup. Called by `AnimatedEntity.init` after the
    /// `MTLDevice` is confirmed. Compile shaders and build `MTLRenderPipelineState`
    /// here; all subsequent `render` calls assume setup has completed.
    func setup(device: MTLDevice)

    /// The `MTKView` drawable size changed. Called by `MTKViewDelegate` before
    /// the next draw. Use to update aspect-ratio-sensitive shader uniforms.
    func resize(drawableSize: CGSize)

    /// Render one frame into `view`.
    ///
    /// - Parameters:
    ///   - view: The `MTKView` whose `currentRenderPassDescriptor` and
    ///     `currentDrawable` should be used.
    ///   - state: The current entity visual state, updated by `DexterClient`
    ///     on every `EntityStateChange` server event.
    ///   - time: Seconds elapsed since `AnimatedEntity` was created. Used as
    ///     the animation clock — continuous, monotonically increasing.
    /// - Returns: `false` if the frame was skipped (e.g. setup not yet complete
    ///   or no drawable available). The return value is advisory; `AnimatedEntity`
    ///   does not act on it.
    ///
    /// `@MainActor` because `MTKView.currentRenderPassDescriptor`,
    /// `currentDrawable`, and `drawableSize` are all main-actor-isolated in the
    /// macOS 26 SDK (they must only be read on the main thread). MetalKit always
    /// calls `draw(in:)` on the main thread, so this annotation is accurate and
    /// eliminates three Swift 6 isolation warnings without changing behaviour.
    @MainActor @discardableResult
    func render(in view: MTKView, state: EntityState, time: Double) -> Bool
}
