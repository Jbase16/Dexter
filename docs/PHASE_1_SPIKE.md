# Phase 1 — Feasibility Spike
## Dexter Build Plan · Session 3 · 2026-03-05

> **Goal:** Prove the two existential risks before any depth is built on top of them.
> Phase 1 is deliberately minimal. Every line of code exists to answer a specific question.
> Scope creep here delays every phase that follows.

---

## What We Are Proving

| Risk | Question | Failure consequence |
|------|----------|---------------------|
| **Windowing** | Can an `NSPanel` at `.screenSaver` level stay present and correctly click-through across all Mission Control spaces, fullscreen apps, and multiple displays? | The "always present" requirement is architecturally impossible — entire UI approach must change |
| **IPC** | Can a Swift client connect to a Rust `tonic` gRPC server over a Unix domain socket, exchange typed proto messages, and recover cleanly from stale sockets? | Cross-process communication cannot be built on this stack — transport must change |

Both must pass before Phase 2 adds any depth.

---

## Files to Create

```
/Users/jason/Developer/Dex/
├── Makefile
├── src/
│   ├── shared/
│   │   └── proto/
│   │       └── dexter.proto
│   ├── rust-core/
│   │   ├── Cargo.toml
│   │   ├── build.rs
│   │   └── src/
│   │       ├── main.rs
│   │       └── ipc/
│   │           ├── mod.rs
│   │           └── server.rs
│   └── swift/
│       ├── Package.swift
│       └── Sources/
│           └── Dexter/
│               ├── App.swift
│               ├── FloatingWindow.swift
│               ├── ConnectionIndicator.swift
│               └── Bridge/
│                   ├── DexterClient.swift
│                   └── generated/
│                       ├── dexter.pb.swift       ← protoc output, committed
│                       └── dexter.grpc.swift     ← protoc output, committed
```

---

## 1. Proto — `src/shared/proto/dexter.proto`

Phase 1 only strictly requires `Ping`. The `Session` stream shape is defined now because
the message envelope contract should be settled before both sides are built independently.
Changing proto after Swift and Rust have consumed it creates drift.

```protobuf
syntax = "proto3";
package dexter.v1;

service DexterService {
  // Phase 1: liveness + roundtrip proof
  rpc Ping(PingRequest) returns (PingResponse);

  // Phase 1 stub — shape only. Wired to real orchestrator in Phase 6.
  rpc Session(stream ClientEvent) returns (stream ServerEvent);
}

// ── Ping ─────────────────────────────────────────────────────────────────────

message PingRequest  { string trace_id = 1; }
message PingResponse { string trace_id = 1; string core_version = 2; }

// ── Session stream ────────────────────────────────────────────────────────────

message ClientEvent {
  string trace_id   = 1;
  string session_id = 2;
  oneof event {
    TextInput text_input = 3;
  }
}

message ServerEvent {
  string trace_id = 1;
  oneof event {
    TextResponse      text_response  = 2;
    EntityStateChange entity_state   = 3;
  }
}

// ── Message types ─────────────────────────────────────────────────────────────

message TextInput    { string content = 1; }
message TextResponse { string content = 1; bool is_final = 2; }

message EntityStateChange { EntityState state = 1; }

enum EntityState {
  ENTITY_STATE_UNSPECIFIED = 0;
  ENTITY_STATE_IDLE        = 1;
  ENTITY_STATE_LISTENING   = 2;
  ENTITY_STATE_THINKING    = 3;
  ENTITY_STATE_SPEAKING    = 4;
  ENTITY_STATE_ALERT       = 5;
  ENTITY_STATE_FOCUSED     = 6;
}
```

---

## 2. Rust Core

### `src/rust-core/Cargo.toml`

```toml
[package]
name    = "dexter-core"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio        = { version = "1",    features = ["full"] }
tonic        = { version = "0.12", features = ["transport"] }
prost        = "0.13"
tokio-stream = { version = "0.1",  features = ["net"] }
tracing      = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
anyhow       = "1"

[build-dependencies]
tonic-build = "0.12"
```

> **Why these versions:** tonic 0.12 pairs with prost 0.13 — they must be aligned or
> the generated types won't match the runtime. `tokio-stream` with the `net` feature
> provides `UnixListenerStream`, which is how tonic binds to a Unix domain socket without
> a TCP listener.

---

### `src/rust-core/build.rs`

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(false)  // Swift owns the client; no Rust client needed
        .compile_protos(
            &["../shared/proto/dexter.proto"],
            &["../shared/proto/"],
        )?;
    Ok(())
}
```

> **Why `build_client(false)`:** The Rust process is the server. The Swift process is the
> client. Generating both sides would produce dead code and inflate compile time.
> `tonic_build` outputs into `OUT_DIR`; files are included at compile time via
> `tonic::include_proto!("dexter.v1")`.

---

### `src/rust-core/src/main.rs`

```rust
mod ipc;

use anyhow::Result;
use tracing::info;

// Named constants — no magic strings anywhere in the codebase.
pub const SOCKET_PATH:  &str = "/tmp/dexter.sock";
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter("dexter_core=debug,tonic=info")
        .init();

    info!(version = CORE_VERSION, socket = SOCKET_PATH, "Dexter core starting");

    cleanup_stale_socket(SOCKET_PATH).await?;
    ipc::serve(SOCKET_PATH).await?;

    Ok(())
}

/// Checks whether a stale socket file exists from a previous crash.
///
/// Strategy: attempt a real connection rather than just checking file existence.
/// A live socket answers; a stale one refuses. This is more reliable than a
/// file check, which would incorrectly treat a running instance as stale.
async fn cleanup_stale_socket(path: &str) -> Result<()> {
    if std::path::Path::new(path).exists() {
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => {
                // Connection succeeded — another core instance is running.
                anyhow::bail!(
                    "Another Dexter core is already running at {}. \
                     Stop it before starting a new instance.",
                    path
                );
            }
            Err(_) => {
                // Connection refused — socket file is stale from a crash.
                std::fs::remove_file(path)?;
                info!(path, "Removed stale socket file");
            }
        }
    }
    Ok(())
}
```

---

### `src/rust-core/src/ipc/mod.rs`

```rust
mod server;
pub use server::serve;
```

---

### `src/rust-core/src/ipc/server.rs`

```rust
use std::pin::Pin;

use tokio::net::UnixListener;
use tokio_stream::{wrappers::UnixListenerStream, Stream};
use tonic::{transport::Server, Request, Response, Status, Streaming};
use tracing::info;

use crate::CORE_VERSION;

// Pull the generated proto types into scope.
pub mod proto {
    tonic::include_proto!("dexter.v1");
}

use proto::{
    dexter_service_server::{DexterService, DexterServiceServer},
    ClientEvent, EntityState, EntityStateChange, PingRequest, PingResponse,
    ServerEvent,
};

// ── Service implementation ────────────────────────────────────────────────────

pub struct CoreService;

/// The stream type returned by the Session RPC.
/// Pinned boxed trait object — standard pattern for tonic server-side streaming.
type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerEvent, Status>> + Send>>;

#[tonic::async_trait]
impl DexterService for CoreService {
    async fn ping(
        &self,
        request: Request<PingRequest>,
    ) -> Result<Response<PingResponse>, Status> {
        let trace_id = request.into_inner().trace_id;
        info!(trace_id = %trace_id, "Ping received");
        Ok(Response::new(PingResponse {
            trace_id,
            core_version: CORE_VERSION.to_string(),
        }))
    }

    type SessionStream = SessionStream;

    /// Phase 1 stub: accepts the stream, emits one IDLE state event, then holds open.
    /// Real orchestration is wired in Phase 6.
    async fn session(
        &self,
        _request: Request<Streaming<ClientEvent>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let idle_event = ServerEvent {
            trace_id: spike_trace_id(),
            event: Some(proto::server_event::Event::EntityState(
                EntityStateChange {
                    state: EntityState::Idle.into(),
                },
            )),
        };

        // Single IDLE event followed by an indefinitely-held open stream.
        // Swift client should receive IDLE and update its connection indicator.
        let stream = tokio_stream::once(Ok(idle_event));
        Ok(Response::new(Box::pin(stream)))
    }
}

// ── Server bind ───────────────────────────────────────────────────────────────

/// Binds a tonic gRPC server to the given Unix domain socket path.
///
/// Uses `UnixListenerStream` from tokio-stream, which wraps tokio's `UnixListener`
/// in a Stream that tonic's `serve_with_incoming` can consume directly.
/// This is the correct path for UDS in tonic — `serve()` is TCP-only.
pub async fn serve(socket_path: &str) -> anyhow::Result<()> {
    let listener = UnixListener::bind(socket_path)?;
    let stream   = UnixListenerStream::new(listener);

    info!(socket = socket_path, "gRPC server listening");

    Server::builder()
        .add_service(DexterServiceServer::new(CoreService))
        .serve_with_incoming(stream)
        .await?;

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Phase 1 trace ID — no uuid crate yet, timestamp-based placeholder.
/// Replaced with proper UUID v4 in Phase 2 when the constants/config layer exists.
fn spike_trace_id() -> String {
    format!(
        "spike-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    )
}
```

---

## 3. Swift Shell

### `src/swift/Package.swift`

grpc-swift 2.x splits into three packages. All three are required:
- `grpc-swift` — core protocol implementation
- `grpc-swift-nio-transport` — NIO-based transport layer (includes UDS support)
- `grpc-swift-protobuf` — Protobuf serialization bridge

```swift
// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "Dexter",
    platforms: [.macOS(.v15)],
    dependencies: [
        .package(
            url: "https://github.com/grpc/grpc-swift.git",
            from: "2.0.0"
        ),
        .package(
            url: "https://github.com/grpc/grpc-swift-nio-transport.git",
            from: "1.0.0"
        ),
        .package(
            url: "https://github.com/grpc/grpc-swift-protobuf.git",
            from: "1.0.0"
        ),
    ],
    targets: [
        .executableTarget(
            name: "Dexter",
            dependencies: [
                .product(name: "GRPCCore",              package: "grpc-swift"),
                .product(name: "GRPCNIOTransportHTTP2", package: "grpc-swift-nio-transport"),
                .product(name: "GRPCProtobuf",          package: "grpc-swift-protobuf"),
            ],
            path: "Sources/Dexter"
        ),
    ]
)
```

> **Why grpc-swift 2.x over 1.x:** v2 is written for Swift Concurrency (async/await)
> natively. v1 used NIO `EventLoopFuture` everywhere — functional but verbose and
> incompatible with Swift 6 strict concurrency checking. Since we're on Swift 6.2, v2
> is the correct target. The three-package split is intentional: the core protocol is
> transport-agnostic; NIO is one transport option, not the only one.

---

### `src/swift/Sources/Dexter/App.swift`

```swift
import AppKit

/// Application entry point.
///
/// Uses NSApplicationDelegate rather than the @main SwiftUI App struct because
/// we need direct control over NSApplication lifecycle — specifically, setting
/// the activation policy to .accessory before the run loop starts, which prevents
/// a dock icon from ever appearing.
@main
final class DexterApp: NSObject, NSApplicationDelegate {

    private var floatingWindow: FloatingWindow?
    private var client: DexterClient?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Must be set before any window becomes visible.
        // .accessory = no dock icon, no application menu, no Cmd+Tab entry.
        NSApp.setActivationPolicy(.accessory)

        let window = FloatingWindow()
        self.floatingWindow = window
        window.makeKeyAndOrderFront(nil)

        // Connect to Rust core in the background.
        // DexterClient handles retry on connection failure — core may not be up yet.
        Task {
            let c = DexterClient()
            self.client = c
            await c.connect(to: window)
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool {
        // Dexter has no "last window" in the conventional sense.
        // The floating window closing should not terminate the process.
        false
    }
}
```

> **Why `NSApplicationDelegate` and not `@main` SwiftUI `App`:** SwiftUI's `App`
> protocol creates a dock icon and application menu by default. Suppressing both requires
> calling `NSApp.setActivationPolicy(.accessory)` before the run loop starts — something
> that is difficult to hook reliably through SwiftUI's lifecycle. With `NSApplicationDelegate`
> the call order is explicit and guaranteed.

---

### `src/swift/Sources/Dexter/FloatingWindow.swift`

```swift
import AppKit

/// The always-present floating window that hosts Dexter's visual presence.
///
/// Uses NSPanel over NSWindow because NSPanel supports the .nonactivatingPanel
/// style mask, which prevents the panel from stealing keyboard focus from whatever
/// application the operator is actively using. An NSWindow at .screenSaver level
/// would steal focus on click — making Dexter actively obstructive rather than
/// present-but-passive.
final class FloatingWindow: NSPanel {

    private(set) var connectionIndicator: ConnectionIndicator!

    init() {
        let screen = NSScreen.main ?? NSScreen.screens[0]

        // Default position: lower-right corner with padding.
        // Persisted position state is added in Phase 2.
        let size   = CGSize(width: 200, height: 400)
        let origin = CGPoint(
            x: screen.visibleFrame.maxX - size.width  - 20,
            y: screen.visibleFrame.minY + 20
        )

        super.init(
            contentRect: CGRect(origin: origin, size: size),
            styleMask:   [.borderless, .nonactivatingPanel],
            backing:     .buffered,
            defer:       false
        )

        configureWindow()
        buildContentView()
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

        // Operator can drag Dexter by clicking anywhere on the panel body.
        isMovableByWindowBackground = true

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

        // Phase 1 visual: a simple disc showing gRPC connection state.
        // Replaced entirely by the Metal AnimatedEntity in Phase 12.
        let indicator = ConnectionIndicator()
        indicator.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(indicator)
        NSLayoutConstraint.activate([
            indicator.centerXAnchor.constraint(equalTo: content.centerXAnchor),
            indicator.centerYAnchor.constraint(equalTo: content.centerYAnchor),
            indicator.widthAnchor.constraint(equalToConstant: 60),
            indicator.heightAnchor.constraint(equalToConstant: 60),
        ])
        self.connectionIndicator = indicator
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
```

---

### `src/swift/Sources/Dexter/ConnectionIndicator.swift`

```swift
import AppKit

/// Phase 1 visual indicator of gRPC connection state.
///
/// A simple colored disc — no Metal, no animation.
/// Replaced entirely by the AnimatedEntity Metal renderer in Phase 12.
/// Exists solely to give visible confirmation that the spike is working
/// without requiring any rendering infrastructure to be in place.
final class ConnectionIndicator: NSView {

    enum State {
        case disconnected  // Red  — no connection to Rust core
        case connecting    // Yellow — attempting connection
        case connected     // Green  — ping confirmed, core is alive
    }

    var state: State = .disconnected {
        didSet { needsDisplay = true }
    }

    override func draw(_ dirtyRect: NSRect) {
        let color: NSColor = switch state {
        case .disconnected: .systemRed
        case .connecting:   .systemYellow
        case .connected:    .systemGreen
        }
        color.setFill()
        NSBezierPath(ovalIn: bounds.insetBy(dx: 4, dy: 4)).fill()
    }
}
```

---

### `src/swift/Sources/Dexter/Bridge/DexterClient.swift`

> **Why `actor` and not `@MainActor final class`:** grpc-swift 2.x declares its
> request/response closures as `@Sendable`. Capturing an `@MainActor`-isolated `self`
> inside a `@Sendable` closure is a Swift 6 error — the main actor and the closure's
> execution context are different isolation domains. Using `actor DexterClient`
> makes the type `Sendable` automatically, satisfies the closure capture constraint,
> and isolates `eventContinuation` from data races without any `@unchecked Sendable`
> workarounds.

```swift
import Foundation
import AppKit
import GRPCCore
import GRPCNIOTransportHTTP2

/// Manages the connection to the Rust core over gRPC-on-UDS.
///
/// Declared as an `actor` rather than a `@MainActor final class` for two reasons:
/// 1. grpc-swift 2.x closures are `@Sendable`; actors are `Sendable` by definition,
///    satisfying the capture constraint without unsafe annotations.
/// 2. `eventContinuation` is mutable state accessed across async boundaries;
///    actor isolation eliminates the data race without external locking.
///
/// In later phases, `send(_:)` is the operator-facing API for injecting events
/// into the active session stream from anywhere in the codebase.
actor DexterClient {

    private static let socketPath = "/tmp/dexter.sock"
    private static let retryDelay = Duration.milliseconds(500)

    /// Live continuation for the client-event channel.
    ///
    /// Non-nil only while a session is established. The requestProducer in
    /// `runSession` iterates this stream — keeping it open holds the writer
    /// alive without any sleep. `send(_:)` yields into it from Phase 6 onward.
    private var eventContinuation: AsyncStream<Dexter_V1_ClientEvent>.Continuation?

    // MARK: - Connection lifecycle

    func connect(to window: FloatingWindow) async {
        await MainActor.run { window.connectionIndicator.state = .connecting }

        // Retry loop — Rust core may start after the Swift shell.
        // runSession() returns normally when the server closes the stream,
        // and throws on connection failure. Either way, retry fires immediately.
        while !Task.isCancelled {
            do {
                try await runSession(window: window)
            } catch {
                await MainActor.run { window.connectionIndicator.state = .connecting }
                try? await Task.sleep(for: Self.retryDelay)
            }
        }
    }

    // MARK: - Session

    private func runSession(window: FloatingWindow) async throws {
        // HTTP2ClientTransport.Posix from GRPCNIOTransportHTTP2 supports UDS
        // via the .unixDomainSocket(path:) target — this is the correct API
        // for connecting to a tonic server bound with UnixListenerStream.
        let transport = try HTTP2ClientTransport.Posix(
            target: .unixDomainSocket(path: Self.socketPath),
            config: .defaults(transportSecurity: .plaintext)
        )

        try await withGRPCClient(transport: transport) { client in
            let stub = Dexter_V1_DexterService.Client(wrapping: client)

            // Confirm liveness before opening the session stream.
            let pong = try await stub.ping(
                Dexter_V1_PingRequest.with { $0.traceID = "spike-001" }
            )
            print("[DexterClient] Ping OK — core version: \(pong.coreVersion)")
            await MainActor.run { window.connectionIndicator.state = .connected }

            // ── Client-event channel ──────────────────────────────────────────
            //
            // AsyncStream.makeStream() produces a Sendable stream that bridges
            // the actor's isolated send API to the @Sendable requestProducer closure.
            //
            // The stream is the mechanism that holds the writer open without sleep:
            // the requestProducer's for-await loop blocks on an empty stream,
            // consuming events as they arrive. When the continuation is finished
            // (in the defer below), the loop exits and the writer closes cleanly.
            let (clientEvents, continuation) = AsyncStream<Dexter_V1_ClientEvent>.makeStream()
            self.eventContinuation = continuation
            defer {
                continuation.finish()   // unblocks requestProducer on any exit path
                self.eventContinuation = nil
            }

            // ── Bidirectional session stream ──────────────────────────────────
            //
            // requestProducer: captures clientEvents (Sendable) — no actor crossing.
            //   Blocks on an empty stream; exits when continuation.finish() is called.
            //
            // handler (response iterator): captures nothing from self.
            //   Iterates server events until the server closes the stream.
            //   When the for-await loop exits, this closure returns, the call
            //   completes, and runSession() returns — triggering the retry loop
            //   in connect() immediately, with no arbitrary sleep in between.
            try await stub.session { [clientEvents] writer in
                for await event in clientEvents {
                    try await writer.write(event)
                }
            } handler: { serverEvents in
                for try await event in serverEvents {
                    // Phase 1: log only. Entity state drives animation in Phase 12.
                    if case .entityState(let change) = event.event {
                        print("[DexterClient] Entity state → \(change.state)")
                    }
                }
                // Loop exits naturally when server closes stream.
                // No explicit signal needed — the return unwinds the call stack.
            }
        }
    }

    // MARK: - Public send API (used from Phase 6 onward)

    /// Inject an operator event into the active session stream.
    /// Silently dropped if no session is currently established.
    func send(_ event: Dexter_V1_ClientEvent) {
        eventContinuation?.yield(event)
    }
}
```

---

## 4. Makefile

```makefile
# ── Paths ──────────────────────────────────────────────────────────────────────

PROTO_DIR     := src/shared/proto
PROTO_FILE    := $(PROTO_DIR)/dexter.proto
SWIFT_GEN_DIR := src/swift/Sources/Dexter/Bridge/generated
RUST_CORE_DIR := src/rust-core
SWIFT_DIR     := src/swift

# ── Runtime constants ─────────────────────────────────────────────────────────
#
# SOCKET_PATH must match the constant in src/rust-core/src/main.rs.
# SOCKET_TIMEOUT_SECS: how long wait-for-core polls before giving up.
# 30 seconds accommodates a cold cargo build on first run.

SOCKET_PATH         := /tmp/dexter.sock
SOCKET_TIMEOUT_SECS := 30

# ── Targets ────────────────────────────────────────────────────────────────────

.PHONY: all setup proto run-core run-swift wait-for-core run clean

all: proto

## setup: verify all required toolchains and protoc plugins are available
setup:
	@echo "==> Checking toolchains"
	@rustc --version
	@cargo --version
	@swift --version
	@python3 --version
	@ollama --version
	@echo "==> Checking protoc plugins"
	@which protoc             || (echo "ERROR: protoc not found — brew install protobuf" && exit 1)
	@which protoc-gen-swift   || (echo "ERROR: protoc-gen-swift not found — brew install swift-protobuf" && exit 1)
	@which protoc-gen-grpc-swift || (echo "ERROR: protoc-gen-grpc-swift not found — brew install grpc-swift" && exit 1)
	@echo "==> All checks passed"

## proto: compile dexter.proto → Swift and Rust artifacts
proto: $(PROTO_FILE)
	@echo "==> Generating Swift proto artifacts → $(SWIFT_GEN_DIR)"
	@mkdir -p $(SWIFT_GEN_DIR)
	protoc \
		--proto_path=$(PROTO_DIR) \
		--swift_out=$(SWIFT_GEN_DIR) \
		--grpc-swift_out=$(SWIFT_GEN_DIR) \
		$(PROTO_FILE)
	@echo "==> Rust proto artifacts compiled by build.rs during cargo build"
	@echo "==> Proto generation complete"

## run-core: start the Rust daemon
run-core:
	cd $(RUST_CORE_DIR) && cargo run

## run-swift: start the Swift UI shell (requires run-core already running)
run-swift:
	cd $(SWIFT_DIR) && swift run

## wait-for-core: block until the Rust core socket is present and accepting connections.
##
## Uses `nc -z -U` (netcat, zero-I/O mode, Unix socket) to attempt a real
## connection — not just a file existence check. A stale socket file from a
## previous crash passes `-S` (file exists and is a socket) but fails `nc -z -U`
## (connection refused), so the probe correctly distinguishes the two states.
##
## Exits 0 when the core is ready, exits 1 with a clear error after timeout.
## The timeout accommodates a cold `cargo build` on first run (~30s on Apple Silicon).
wait-for-core:
	@echo "==> Waiting for Rust core at $(SOCKET_PATH) (timeout: $(SOCKET_TIMEOUT_SECS)s)..."
	@elapsed=0; \
	while [ $$elapsed -lt $(SOCKET_TIMEOUT_SECS) ]; do \
		if nc -z -U "$(SOCKET_PATH)" 2>/dev/null; then \
			echo "==> Core ready after $${elapsed}s"; \
			exit 0; \
		fi; \
		sleep 1; \
		elapsed=$$((elapsed + 1)); \
	done; \
	echo "ERROR: Rust core did not become ready within $(SOCKET_TIMEOUT_SECS)s."; \
	echo "       Check 'make run-core' output for compilation or startup errors."; \
	kill 0; \
	exit 1

## run: start both processes. Swift shell waits for the core socket to accept
##      connections before launching — no fixed sleep, no silent race condition.
##      Ctrl-C kills both processes.
run:
	@trap 'kill 0' INT; \
	$(MAKE) run-core & \
	$(MAKE) wait-for-core && $(MAKE) run-swift & \
	wait

## clean: remove socket file and build artifacts
clean:
	rm -f $(SOCKET_PATH)
	rm -f $(SWIFT_GEN_DIR)/*.swift
	cd $(RUST_CORE_DIR) && cargo clean
```

---

## 5. Proto Compilation Notes

Two plugins are required and must be installed before `make proto` will succeed:

| Plugin | Package | Install |
|--------|---------|---------|
| `protoc-gen-swift` | swift-protobuf | `brew install swift-protobuf` |
| `protoc-gen-grpc-swift` | grpc-swift | `brew install grpc-swift` |

The Swift generated files (`dexter.pb.swift`, `dexter.grpc.swift`) are **committed to
the repository**. They are not generated at build time. This keeps the Swift build
reproducible without requiring protoc on the build machine.

The Rust generated files are **not committed**. They live in `OUT_DIR` (Cargo's build
artifact directory) and are compiled fresh by `build.rs` on every `cargo build`.

---

## 6. Acceptance Criteria

Phase 1 is complete when all of the following pass manually:

| # | Test | Pass condition |
|---|------|----------------|
| 1 | `make proto` | Runs without error. `dexter.pb.swift` and `dexter.grpc.swift` appear in `Bridge/generated/`. `cargo build` succeeds with no warnings. |
| 2 | `make run-core` (first run) | Process starts, logs UDS bind, stays running. |
| 3 | `make run-core` (second instance) | Detects live socket, exits immediately with a clear error message. |
| 4 | Kill core, `make run-core` (restart) | Stale socket cleaned up, new instance binds and runs cleanly. |
| 5 | `make run-swift` with core running | `ConnectionIndicator` transitions yellow → green within 2 seconds. Console prints `Ping OK — core version: 0.1.0`. |
| 6 | Window level — spaces | Floating window visible on every Mission Control space. |
| 7 | Window level — fullscreen | Floating window visible over a fullscreen application on the same display. |
| 8 | Window level — Exposé | Window remains in place (stationary) during Mission Control activation. Does not appear in Cmd+Tab switcher. |
| 9 | Click-through | Clicks on transparent area around the disc reach the application below. Clicks on the disc itself are received by the Swift process. |

All 9 must pass before Phase 2 begins.

---

## 7. What Phase 1 Deliberately Does Not Do

The following are out of scope and must not be added during this phase:

- No Metal rendering — the connection indicator is a plain `NSBezierPath` disc
- No Ollama interaction — no models, no inference
- No config file or constants module — those are Phase 2
- No structured session state written by Rust
- No `Session` stream logic — the stub sends one `IDLE` event and sleeps
- No personality, no routing, no retrieval
- No voice capture

The goal is surgical confirmation that the two load-bearing risks are resolved.
Everything else waits.

---

*Phase 1 plan authored: 2026-03-05*
*Depends on: IMPLEMENTATION_PLAN.md v1.1, SESSION_STATE.json v1.2*
*Next phase on completion: Phase 2 — Foundation (constants, config, logging, Makefile hardening)*
