import Metal
import MetalKit
import simd

/// Metal renderer that draws an SDF disc with per-state animation.
///
/// SwiftPM cannot compile `.metal` files (no bundle target for shader objects),
/// so `shaderSource` is an inline MSL string constant compiled at runtime via
/// `MTLDevice.makeLibrary(source:options:)`. This produces the same GPU bytecode
/// as an offline-compiled `.metal` file — the driver performs the same
/// compilation — but happens once at startup rather than at build time.
///
/// Premultiplied alpha is used throughout: the fragment shader outputs
/// `float4(color * alpha, alpha)` and the pipeline blend equation is
/// `src×1 + dst×(1-srcAlpha)`. This is required for correct compositing against
/// the transparent `NSPanel` window — straight alpha causes fringing artifacts
/// at disc edges when composited over coloured backgrounds.
final class GeometricRenderer: EntityRenderer {

    // MARK: - Private state

    private var device:        MTLDevice?
    private var commandQueue:  MTLCommandQueue?
    private var pipelineState: MTLRenderPipelineState?
    private var drawableSize:  CGSize = .zero

    // MARK: - Uniforms

    /// Passed as `buffer(0)` in the fragment shader. Layout must match the
    /// `struct Uniforms` definition inside `shaderSource` exactly.
    private struct Uniforms {
        var resolution: SIMD2<Float>   // drawable dimensions in pixels
        var time:       Float          // seconds since AnimatedEntity was created
        var state:      Int32          // EntityState.shaderValue (0=idle…5=focused)
    }

    // MARK: - Shader source

    /// Full MSL shader source. Compiled at runtime; no `.metal` file needed.
    ///
    /// Vertex (`vs_main`): fullscreen triangle trick — 3 hardcoded clip-space
    /// vertices forming a triangle that fully covers NDC [-1,1]×[-1,1] when
    /// rasterised. No vertex buffer is bound; vertex positions come from the
    /// vertex ID. This avoids allocating a vertex buffer for a trivially simple
    /// geometry and is idiomatic for full-screen-pass shaders in Metal.
    ///
    /// Fragment (`fs_main`): SDF disc in aspect-correct centred coordinates.
    /// Per-state animation drives pulse speed, amplitude, and base colour.
    private static let shaderSource = """
    #include <metal_stdlib>
    using namespace metal;

    struct Uniforms {
        float2 resolution;
        float  time;
        int    state;   // 0=idle, 1=listening, 2=thinking, 3=speaking, 4=alert, 5=focused
    };

    // ── Vertex ────────────────────────────────────────────────────────────────

    vertex float4 vs_main(uint vid [[vertex_id]]) {
        // Three vertices whose triangle covers the full clip-space quad.
        // The rasteriser clips the triangle to [-1,1]×[-1,1]; no vertex data needed.
        float2 pos;
        if      (vid == 0) { pos = float2(-1.0,  3.0); }
        else if (vid == 1) { pos = float2(-1.0, -1.0); }
        else               { pos = float2( 3.0, -1.0); }
        return float4(pos, 0.0, 1.0);
    }

    // ── Fragment ──────────────────────────────────────────────────────────────

    fragment float4 fs_main(float4 fragCoord [[position]],
                             constant Uniforms& u [[buffer(0)]]) {
        // Aspect-correct centred coordinate space where the SHORTER window
        // dimension spans exactly [-1, 1].  A disc of radius r therefore has a
        // visual size of  r × (min(width, height) / 2)  view-points — matching
        // the circle SDF in AnimatedEntity.hitTest(_:) exactly.
        //
        // Strategy: normalise both axes to [-1, 1], then stretch the longer
        // axis so that 1 unit always equals half the shorter pixel dimension.
        //   Portrait  (ar < 1): width is shorter  → x ∈ [-1, 1], y stretched by 1/ar
        //   Landscape (ar ≥ 1): height is shorter → y ∈ [-1, 1], x stretched by ar
        //   Square    (ar = 1): both axes ∈ [-1, 1], no stretching needed
        float ar  = u.resolution.x / u.resolution.y;
        float2 p  = (fragCoord.xy / u.resolution) * 2.0 - 1.0;
        p *= float2(max(ar, 1.0), max(1.0 / ar, 1.0));
        // Metal fragment coordinates have origin at top-left; flip y so the
        // disc is centred at (0,0) regardless of origin convention.
        p.y = -p.y;

        float dist = length(p);

        // ── Per-state animation parameters ───────────────────────────────────
        float  pulseSpeed = 1.2;
        float  pulseAmp   = 0.04;
        float3 baseColor  = float3(0.75, 0.85, 1.00);  // idle: cool blue-white
        float  baseRadius = 0.55;
        float  glowStr    = 0.35;

        if (u.state == 1) {
            // listening: faster pulse, green
            pulseSpeed = 3.0;
            pulseAmp   = 0.06;
            baseColor  = float3(0.35, 0.92, 0.55);
        } else if (u.state == 2) {
            // thinking: slow hue rotation through spectrum
            pulseSpeed = 2.0;
            pulseAmp   = 0.02;
            float angle = u.time * 0.8;
            baseColor   = float3(0.5 + 0.5 * cos(angle),
                                 0.5 + 0.5 * cos(angle + 2.094),
                                 0.5 + 0.5 * cos(angle + 4.189));
        } else if (u.state == 3) {
            // speaking: fast pulse + secondary ripple ring expanding outward
            pulseSpeed = 5.0;
            pulseAmp   = 0.08;
            baseColor  = float3(0.95, 0.95, 1.00);
            glowStr    = 0.50;
        } else if (u.state == 4) {
            // alert: urgent pulse, amber-orange
            pulseSpeed = 4.0;
            pulseAmp   = 0.05;
            baseColor  = float3(1.00, 0.60, 0.20);
        } else if (u.state == 5) {
            // focused: very slow breathe, deep blue
            pulseSpeed = 0.5;
            pulseAmp   = 0.01;
            baseColor  = float3(0.30, 0.60, 1.00);
        }

        float pulse  = sin(u.time * pulseSpeed * 6.2832) * pulseAmp;
        float radius = baseRadius + pulse;

        // ── SDF disc ─────────────────────────────────────────────────────────
        float edge  = 0.015;  // 1pt antialiased border in normalised space
        float alpha = smoothstep(radius + edge, radius - edge, dist);

        // Soft inner glow: an exponential falloff inside and just outside the disc
        float glow = exp(-dist * dist * 4.0) * glowStr;

        // Speaking state: add a secondary ripple ring at an expanding radius
        float ripple = 0.0;
        if (u.state == 3) {
            float rippleR    = fract(u.time * 0.6) * 0.8 + baseRadius;
            float rippleEdge = 0.04;
            ripple = smoothstep(rippleR + rippleEdge, rippleR, dist) *
                     smoothstep(rippleR - rippleEdge, rippleR, dist) * 0.4;
        }

        // Combine: disc body + glow halo + ripple ring
        float3 color      = baseColor;
        float  totalAlpha = clamp(alpha + glow * 0.7 + ripple, 0.0, 1.0);

        // Premultiplied alpha output — required for correct compositing against
        // the transparent NSPanel window. The pipeline blend equation
        // (src×1 + dst×(1-srcAlpha)) completes the premultiplied composite.
        return float4(color * totalAlpha, totalAlpha);
    }
    """

    // MARK: - EntityRenderer

    func setup(device: MTLDevice) {
        self.device = device
        guard let queue = device.makeCommandQueue() else {
            print("[GeometricRenderer] Failed to create command queue")
            return
        }
        commandQueue = queue

        // Runtime shader compilation — same bytecode as offline .metal compilation.
        // `options: nil` uses default optimisation settings (equivalent to -O2).
        let library: MTLLibrary
        do {
            library = try device.makeLibrary(source: Self.shaderSource, options: nil)
        } catch {
            print("[GeometricRenderer] Shader compilation failed: \(error)")
            return
        }

        let desc = MTLRenderPipelineDescriptor()
        desc.vertexFunction   = library.makeFunction(name: "vs_main")
        desc.fragmentFunction = library.makeFunction(name: "fs_main")
        desc.colorAttachments[0].pixelFormat = .bgra8Unorm  // MTKView default

        // Premultiplied alpha blend: result = src×1 + dst×(1−srcAlpha).
        // src.rgb is already multiplied by src.alpha in the fragment shader,
        // so the source blend factor is .one (not .sourceAlpha).
        let ca = desc.colorAttachments[0]!
        ca.isBlendingEnabled           = true
        ca.rgbBlendOperation           = .add
        ca.alphaBlendOperation         = .add
        ca.sourceRGBBlendFactor        = .one
        ca.sourceAlphaBlendFactor      = .one
        ca.destinationRGBBlendFactor   = .oneMinusSourceAlpha
        ca.destinationAlphaBlendFactor = .oneMinusSourceAlpha

        do {
            pipelineState = try device.makeRenderPipelineState(descriptor: desc)
        } catch {
            print("[GeometricRenderer] Pipeline state creation failed: \(error)")
        }
    }

    func resize(drawableSize size: CGSize) {
        drawableSize = size
    }

    @discardableResult
    func render(in view: MTKView, state: EntityState, time: Double) -> Bool {
        guard let pipeline = pipelineState,
              let queue    = commandQueue,
              let rpd      = view.currentRenderPassDescriptor,
              let drawable  = view.currentDrawable else {
            return false
        }

        // Clear to fully transparent on every frame — the NSPanel window is
        // transparent; any leftover pixels from the previous frame would
        // accumulate rather than compositing correctly.
        rpd.colorAttachments[0].loadAction  = .clear
        rpd.colorAttachments[0].clearColor  = MTLClearColorMake(0, 0, 0, 0)
        rpd.colorAttachments[0].storeAction = .store

        guard let cmd     = queue.makeCommandBuffer(),
              let encoder = cmd.makeRenderCommandEncoder(descriptor: rpd) else {
            return false
        }

        // Always read drawableSize from the view directly rather than from the
        // cached self.drawableSize field. On a Retina Mac, MTKView initialises
        // its CAMetalLayer with contentsScale = backingScaleFactor (2×) during
        // init(), so drawableSize is already correct when the view enters the
        // window. Because the size never changes, mtkView(_:drawableSizeWillChange:)
        // is never called, leaving self.drawableSize at .zero forever. The shader
        // would then receive resolution=(0,0), produce NaN UV coordinates, and
        // return alpha=0 on every fragment — invisible disc. view.drawableSize is
        // always authoritative and costs no extra synchronisation.
        let currentDrawableSize = view.drawableSize
        var uniforms = Uniforms(
            resolution: SIMD2<Float>(
                Float(currentDrawableSize.width),
                Float(currentDrawableSize.height)
            ),
            time:  Float(time),
            state: state.shaderValue
        )

        encoder.setRenderPipelineState(pipeline)
        // Pass Uniforms inline (< 4KB → setVertexBytes / setFragmentBytes is
        // more efficient than a MTLBuffer allocation per frame).
        encoder.setFragmentBytes(&uniforms,
                                 length: MemoryLayout<Uniforms>.size,
                                 index: 0)
        // Fullscreen triangle: 3 vertices, no vertex buffer.
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        encoder.endEncoding()

        cmd.present(drawable)
        cmd.commit()
        return true
    }
}
