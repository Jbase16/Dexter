# Phase 3 — IPC Contract Finalization

## Goal

Finalize `dexter.proto` as the authoritative IPC schema through Phase 10. Prove the
client→server direction of bidirectional streaming (currently untested — Phase 1 only
proved server→client via IDLE event). Add `StreamAudio` RPC definition with a stub
implementation. Run `make proto` to regenerate artifacts. All Phase 1/2 regressions pass.

---

## Where We Are

Phase 1 proved: server sends `EntityStateChange(IDLE)` → Swift receives it → disc green.

**Currently untested:** the client→server direction. The Swift `requestProducer` closure
feeds from an `AsyncStream` that is never yielded to — the Rust session handler ignores
`_request: Request<Streaming<ClientEvent>>` entirely. Phase 3 proves this direction.

**Currently incomplete in proto:**
- `UIAction`, `SystemEvent`, `ActionRequest`, `ActionApproval`, `ActionCategory` — missing
- `AudioChunk`, `AudioResponse`, `TranscriptChunk` — missing
- `StreamAudio` RPC — missing
- `Ping` still present and permanent (liveness/version probe)

---

## Scope

### In Phase 3:
- Proto: all final message types for all future phases
- `make proto` regenerates Swift artifacts from new proto
- Rust: `StreamAudio` stub (returns `UNIMPLEMENTED`), session handler reads inbound stream
- Rust: replace `spike_trace_id()` with `uuid::Uuid::new_v4()` (adds `uuid` crate)
- Swift: sends `SystemEvent(CONNECTED)` immediately on session open; logs `TextResponse`
- Integration test: Rust `#[tokio::test]` proving bidirectional Session + UNIMPLEMENTED StreamAudio
- Phase 1/2 regression: disc still goes green

### NOT in Phase 3:
- STT/TTS implementation (Phase 10)
- Action engine (Phase 8)
- Context observer event routing (Phase 7)
- Orchestrator (Phase 6) — session handler stub is explicit placeholder
- Any Swift UI changes beyond logging

---

## 1. Proto changes

**File:** `src/shared/proto/dexter.proto`

Full replacement:

```protobuf
syntax = "proto3";
package dexter.v1;

service DexterService {
  // Liveness probe and version handshake. Called once at connection time.
  rpc Ping(PingRequest) returns (PingResponse);

  // Bidirectional session stream.
  // Swift sends ClientEvents (user text, UI actions, context observations, action approvals).
  // Rust sends ServerEvents (responses, entity state changes, action requests, audio).
  // Phase 6 orchestrator wires this to real component routing.
  rpc Session(stream ClientEvent) returns (stream ServerEvent);

  // Microphone audio stream → STT transcripts.
  // Swift streams PCM audio chunks from VoiceCapture.
  // Rust routes to the STT worker and streams back transcripts.
  // DEFINED in Phase 3. IMPLEMENTED in Phase 10 (Voice Worker Bridge).
  rpc StreamAudio(stream AudioChunk) returns (stream TranscriptChunk);
}

// ── Ping ──────────────────────────────────────────────────────────────────────

message PingRequest  { string trace_id = 1; }
message PingResponse { string trace_id = 1; string core_version = 2; }

// ── Session stream — client events ────────────────────────────────────────────

message ClientEvent {
  string trace_id   = 1;  // UUID v4 — correlation across components
  string session_id = 2;  // UUID v4 — stable for the lifetime of this Session call
  oneof event {
    TextInput      text_input      = 3;
    UIAction       ui_action       = 4;
    SystemEvent    system_event    = 5;
    ActionApproval action_approval = 6;
  }
}

// ── Session stream — server events ────────────────────────────────────────────

message ServerEvent {
  string trace_id = 1;
  oneof event {
    TextResponse      text_response  = 2;
    EntityStateChange entity_state   = 3;
    AudioResponse     audio_response = 4;
    ActionRequest     action_request = 5;
  }
}

// ── Client event payloads ─────────────────────────────────────────────────────

message TextInput {
  string content = 1;
}

message UIAction {
  UIActionType type    = 1;
  string       payload = 2;  // JSON-encoded, type-specific. Empty when not needed.
}

enum UIActionType {
  UI_ACTION_TYPE_UNSPECIFIED = 0;
  UI_ACTION_TYPE_DISMISS     = 1;  // User dismissed Dexter from view
  UI_ACTION_TYPE_DRAG        = 2;  // payload: {"x": 120, "y": 840} — new position
  UI_ACTION_TYPE_RESIZE      = 3;  // payload: {"width": 200, "height": 400}
}

message SystemEvent {
  SystemEventType type    = 1;
  string          payload = 2;  // JSON-encoded, type-specific. Empty when not needed.
}

enum SystemEventType {
  SYSTEM_EVENT_TYPE_UNSPECIFIED   = 0;
  SYSTEM_EVENT_TYPE_CONNECTED     = 1;  // Swift shell connected; session open
  SYSTEM_EVENT_TYPE_APP_FOCUSED   = 2;  // payload: {"bundle_id": "com.example.app", "name": "Xcode"}
  SYSTEM_EVENT_TYPE_APP_UNFOCUSED = 3;
  SYSTEM_EVENT_TYPE_SCREEN_LOCKED = 4;
}

// Operator's approval or rejection of an ActionRequest from the server.
message ActionApproval {
  string action_id     = 1;  // Must match ActionRequest.action_id
  bool   approved      = 2;
  string operator_note = 3;  // Optional note attached to approval/rejection
}

// ── Server event payloads ─────────────────────────────────────────────────────

message TextResponse {
  string content  = 1;
  bool   is_final = 2;  // False during streaming tokens; true on last chunk.
}

message EntityStateChange {
  EntityState state = 1;
}

enum EntityState {
  ENTITY_STATE_UNSPECIFIED = 0;
  ENTITY_STATE_IDLE        = 1;
  ENTITY_STATE_LISTENING   = 2;
  ENTITY_STATE_THINKING    = 3;
  ENTITY_STATE_SPEAKING    = 4;
  ENTITY_STATE_ALERT       = 5;
  ENTITY_STATE_FOCUSED     = 6;
}

// Server requests explicit operator confirmation before executing an action.
// Swift presents a confirmation dialog; operator response is sent as ActionApproval.
// SAFE actions skip this flow entirely — they are executed and logged without confirmation.
message ActionRequest {
  string         action_id   = 1;  // UUID — correlates with ActionApproval
  string         description = 2;  // Human-readable description for the confirmation UI
  ActionCategory category    = 3;
  string         payload     = 4;  // JSON-encoded action parameters (type-specific)
}

enum ActionCategory {
  ACTION_CATEGORY_UNSPECIFIED = 0;
  ACTION_CATEGORY_SAFE        = 1;  // Execute immediately, no confirmation needed
  ACTION_CATEGORY_CAUTIOUS    = 2;  // Execute + write to audit log
  ACTION_CATEGORY_DESTRUCTIVE = 3;  // Requires explicit operator confirmation
}

// TTS audio pushed to Swift for AVAudioEngine playback.
// Sequenced so Swift can detect gaps and maintain ordering.
message AudioResponse {
  bytes  data            = 1;  // Raw PCM, same format as AudioChunk input
  uint32 sequence_number = 2;
}

// ── Audio stream ──────────────────────────────────────────────────────────────

// Streaming microphone audio → STT transcripts.
// DEFINED Phase 3. IMPLEMENTED Phase 10 (Voice Worker Bridge).

message AudioChunk {
  bytes  data            = 1;  // Raw PCM: 16kHz, 16-bit, mono
  uint32 sequence_number = 2;
  uint32 sample_rate     = 3;  // Always 16000 for now; field exists for future resampler
}

message TranscriptChunk {
  string text            = 1;
  bool   is_final        = 2;  // False during partial transcription; true when segment complete
  uint32 sequence_number = 3;  // Correlates with AudioChunk.sequence_number
}
```

---

## 2. Cargo.toml changes

```toml
uuid = { version = "1", features = ["v4"] }   # Proper trace/session IDs, replaces spike_trace_id()
```

Dev-dependencies (for integration tests only):
```toml
[dev-dependencies]
tower = "0.4"   # service_fn for UDS test connector
```

---

## 3. `server.rs` changes

### 3.1 Import additions

```rust
use futures::StreamExt;   // .next() on Streaming<T>
// OR use tonic's .message() — no extra import needed
use uuid::Uuid;
```

Actually `tonic::Streaming<T>` has `.message() -> Result<Option<T>, Status>` as a
first-class method — no StreamExt import required. Use that.

### 3.2 `spike_trace_id()` → `new_trace_id()`

```rust
fn new_trace_id() -> String {
    Uuid::new_v4().to_string()
}
```

Remove `spike_trace_id()`.

### 3.3 `StreamAudio` stub

Add `type StreamAudioStream` and `stream_audio` to `DexterService` impl:

```rust
type StreamAudioStream = Pin<Box<dyn Stream<Item = Result<TranscriptChunk, Status>> + Send>>;

/// Phase 3 stub. Returns UNIMPLEMENTED — implemented in Phase 10 (Voice Worker Bridge).
///
/// The RPC is defined and the stub is present so the proto contract is complete
/// and the generated types compile. Phase 10 replaces this body with real routing.
async fn stream_audio(
    &self,
    _request: Request<Streaming<AudioChunk>>,
) -> Result<Response<Self::StreamAudioStream>, Status> {
    Err(Status::unimplemented(
        "StreamAudio: Phase 10 (Voice Worker Bridge) — not yet implemented",
    ))
}
```

### 3.4 Session handler: read from inbound stream

The session handler currently ignores `_request`. Replace with a reader task that
processes `ClientEvent`s, plus a hold-open task that keeps the response stream alive
until the reader signals it is done. Phase 6 orchestrator replaces both tasks.

**Lifetime design — why two tasks and why oneshot:**

The reader task terminates when the client closes its half of the stream (normal session
end) or on a stream error. The hold-open task keeps the response stream alive so the
server can push events even after the client stops sending — this separation matters
in Phase 6 when async retrieval or background processing may complete after the last
ClientEvent arrived.

The original plan used `std::future::pending()` in the hold-open task, which leaks: the
task lives forever, holds `tx` alive, and `ReceiverStream` never closes after session
end. On Swift restart a new Session RPC call starts a new pair of tasks, but the old
hold-open task still occupies memory and holds a zombie sender.

Fix: a `tokio::sync::oneshot` channel ties hold-open lifetime to reader lifetime.
The reader task owns the oneshot `Sender` by moving it into the task — it is
**dropped** (not `.send()`ed) when the task exits, which resolves the `Receiver` in
the hold-open task. No explicit signaling needed; drop IS the signal.

`oneshot` is in `tokio::sync` which is already available via `tokio = { features = ["full"] }`.
No new dependencies.

```rust
async fn session(
    &self,
    request: Request<Streaming<ClientEvent>>,
) -> Result<Response<Self::SessionStream>, Status> {
    let session_trace = new_trace_id();
    let (tx, rx) = mpsc::channel::<Result<ServerEvent, Status>>(16);
    let mut inbound = request.into_inner();

    // Send IDLE immediately so Swift can set its visual state.
    let idle = ServerEvent {
        trace_id: new_trace_id(),
        event: Some(proto::server_event::Event::EntityState(
            EntityStateChange { state: EntityState::Idle.into() },
        )),
    };
    tx.send(Ok(idle))
        .await
        .map_err(|_| Status::internal("session channel closed before IDLE send"))?;

    info!(session = %session_trace, "Session opened — IDLE sent");

    // Oneshot channel: reader task holds the Sender and drops it on exit.
    // The hold-open task awaits the Receiver — resolves (either Ok or Err) when
    // the Sender is dropped, which is when the reader task exits for any reason.
    let (reader_done_tx, reader_done_rx) = tokio::sync::oneshot::channel::<()>();

    // Reader task: drain inbound ClientEvents, log them, send echo responses.
    // Phase 6 orchestrator replaces this entire task with real event routing.
    //
    // Holds: tx_reader (clone), reader_done_tx (signal).
    // On exit: drops both — tx_reader removes one sender ref, reader_done_tx
    // resolves hold-open's receiver, triggering hold-open to drop tx and close stream.
    let tx_reader = tx.clone();
    tokio::spawn(async move {
        // Move reader_done_tx into this task. Dropped when the task exits,
        // signaling hold-open to terminate regardless of why this task exits.
        let _signal_done = reader_done_tx;

        loop {
            match inbound.message().await {
                Ok(Some(event)) => {
                    info!(
                        session  = %session_trace,
                        trace_id = %event.trace_id,
                        kind     = event.event.as_ref()
                            .map(|e| match e {
                                proto::client_event::Event::TextInput(_)      => "text_input",
                                proto::client_event::Event::UiAction(_)       => "ui_action",
                                proto::client_event::Event::SystemEvent(_)    => "system_event",
                                proto::client_event::Event::ActionApproval(_) => "action_approval",
                            })
                            .unwrap_or("unknown"),
                        "ClientEvent received"
                    );

                    // Phase 3 echo: acknowledge SystemEvent(CONNECTED) with a TextResponse.
                    // This is the minimal bidirectional proof — Phase 6 replaces with routing.
                    if let Some(proto::client_event::Event::SystemEvent(ref sys)) = event.event {
                        if sys.r#type == proto::SystemEventType::Connected as i32 {
                            let ack = ServerEvent {
                                trace_id: event.trace_id.clone(),
                                event: Some(proto::server_event::Event::TextResponse(
                                    TextResponse {
                                        content:  "session ready".to_string(),
                                        is_final: true,
                                    },
                                )),
                            };
                            // Ignore send error — stream may have closed.
                            let _ = tx_reader.send(Ok(ack)).await;
                        }
                    }
                }
                Ok(None) => {
                    // Client closed the request stream — normal session end.
                    info!(session = %session_trace, "Client closed session stream");
                    break;
                }
                Err(e) => {
                    info!(session = %session_trace, error = %e, "Session stream error");
                    break;
                }
            }
        }
        // _signal_done dropped here → reader_done_rx in hold-open resolves.
        // tx_reader dropped here → one fewer sender reference.
    });

    // Hold-open task: keeps tx alive until the reader signals it is done.
    //
    // This decouples inbound stream lifetime from outbound stream lifetime:
    // the server can continue pushing events after the client stops sending.
    //
    // Phase 6 replacement: the CoreOrchestrator holds its own tx clone for the
    // session lifetime and drives event dispatch. This hold-open task is removed
    // and the reader task sends ClientEvents to the orchestrator via a typed
    // channel rather than processing them directly.
    tokio::spawn(async move {
        let _hold = tx;
        // Suspend until reader task exits. Oneshot Receiver resolves on either
        // Ok(()) (explicit send) or Err(RecvError) (sender dropped) — both signal
        // that the reader is done. We don't distinguish them; either way it's time
        // to close the outbound stream.
        let _ = reader_done_rx.await;
        // _hold dropped here → last sender ref gone → ReceiverStream closes →
        // tonic sends END_STREAM trailers → Swift onResponse loop exits cleanly.
    });

    Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
}
```

---

## 4. Swift `DexterClient.swift` changes

### 4.1 Send `SystemEvent(CONNECTED)` on session open

Before entering the session call, yield one `ClientEvent` into the continuation so the
`requestProducer` closure sends it immediately:

```swift
// Generate a stable session ID for the lifetime of this Session call.
let sessionID = UUID().uuidString

// Yield the CONNECTED event before the session loop starts.
// AsyncStream buffers it, so requestProducer sends it as the first event.
let connectedEvent = Dexter_V1_ClientEvent.with {
    $0.traceID   = UUID().uuidString
    $0.sessionID = sessionID
    $0.systemEvent = Dexter_V1_SystemEvent.with {
        $0.type = .connected
    }
}
continuation.yield(connectedEvent)
```

### 4.2 Log `TextResponse` in `onResponse` handler

```swift
for try await event in response.messages {
    switch event.event {
    case .entityState(let change):
        print("[DexterClient] Entity state → \(change.state)")
        await MainActor.run { window.connectionIndicator.state = .connected }
    case .textResponse(let resp):
        print("[DexterClient] Text response: '\(resp.content)' final=\(resp.isFinal)")
    case .audioResponse:
        break  // Phase 10
    case .actionRequest(let req):
        print("[DexterClient] Action request: \(req.description_p) [\(req.category)]")
    case .none:
        break
    }
}
```

Note: `description` is a proto field but also a Swift protocol method — protoc-gen-swift
renames it to `description_p`. Verify the generated name after `make proto` and adjust.

### 4.3 Session ID on all events (for future use)

Store `sessionID` on the actor and include it in any future event yields. No code change
needed for Phase 3 beyond the initial CONNECTED yield.

---

## 5. Integration test

**File:** `src/rust-core/src/ipc/server.rs` (in `#[cfg(test)]` module at bottom)

Tests the Session RPC bidirectional roundtrip AND verifies StreamAudio returns UNIMPLEMENTED.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use proto::{
        dexter_service_client::DexterServiceClient,
        client_event, ClientEvent, SystemEvent, SystemEventType,
    };
    use tokio::net::UnixStream;
    use tonic::transport::Endpoint;
    use tower::service_fn;

    /// Binds the CoreService on a test socket and returns its path.
    async fn spawn_test_server() -> String {
        let path = format!(
            "/tmp/dexter-test-{}.sock",
            Uuid::new_v4().simple()
        );
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            Server::builder()
                .add_service(DexterServiceServer::new(CoreService))
                .serve_with_incoming(UnixListenerStream::new(listener))
                .await
                .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        path
    }

    async fn make_client(path: String) -> DexterServiceClient<tonic::transport::Channel> {
        let channel = Endpoint::from_static("http://localhost")
            .connect_with_connector(service_fn(move |_: tonic::transport::Uri| {
                let p = path.clone();
                async move { UnixStream::connect(p).await }
            }))
            .await
            .unwrap();
        DexterServiceClient::new(channel)
    }

    /// Verifies:
    /// 1. Server→client: first event is EntityStateChange(IDLE)
    /// 2. Client→server: sending SystemEvent(CONNECTED) causes server to log it and reply
    /// 3. Server→client: second event is TextResponse { content: "session ready", is_final: true }
    #[tokio::test]
    async fn session_bidirectional_roundtrip() {
        let socket = spawn_test_server().await;
        let mut client = make_client(socket.clone()).await;

        let connected_event = ClientEvent {
            trace_id:   Uuid::new_v4().to_string(),
            session_id: Uuid::new_v4().to_string(),
            event: Some(client_event::Event::SystemEvent(SystemEvent {
                r#type:  SystemEventType::Connected.into(),
                payload: String::new(),
            })),
        };

        let request_stream = tokio_stream::iter(vec![connected_event]);
        let mut stream = client.session(request_stream).await.unwrap().into_inner();

        // ── First event: IDLE ───────────────────────────────────────────────────
        let first = stream.message().await.unwrap().unwrap();
        assert!(
            matches!(
                first.event,
                Some(proto::server_event::Event::EntityState(
                    EntityStateChange { state, .. }
                )) if state == proto::EntityState::Idle as i32
            ),
            "First server event should be EntityStateChange(IDLE), got: {:?}", first.event
        );

        // ── Second event: CONNECTED ack ─────────────────────────────────────────
        let second = stream.message().await.unwrap().unwrap();
        match second.event {
            Some(proto::server_event::Event::TextResponse(ref resp)) => {
                assert_eq!(resp.content, "session ready");
                assert!(resp.is_final);
            }
            other => panic!("Expected TextResponse, got: {:?}", other),
        }

        std::fs::remove_file(&socket).ok();
    }

    /// Verifies StreamAudio returns UNIMPLEMENTED (Phase 10 stub).
    #[tokio::test]
    async fn stream_audio_returns_unimplemented() {
        let socket = spawn_test_server().await;
        let mut client = make_client(socket.clone()).await;

        let chunk = proto::AudioChunk {
            data:            vec![0u8; 32],
            sequence_number: 0,
            sample_rate:     16000,
        };
        let result = client.stream_audio(tokio_stream::iter(vec![chunk])).await;
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::Unimplemented,
            "StreamAudio should return UNIMPLEMENTED in Phase 3"
        );

        std::fs::remove_file(&socket).ok();
    }
}
```

---

## 6. Execution order

1. Edit `src/shared/proto/dexter.proto` (full replacement)
2. `make proto` — regenerates Swift generated files; Rust regenerates at build time
3. Verify `make proto` exits 0
4. Add `uuid` to `Cargo.toml` dependencies
5. Add `tower` to `[dev-dependencies]`
6. Update `src/rust-core/src/ipc/server.rs`:
   - New imports (`uuid::Uuid`)
   - Replace `spike_trace_id()` with `new_trace_id()`
   - Add `StreamAudioStream` type + `stream_audio` stub
   - Update `session()` with reader task + updated event construction
   - Add `#[cfg(test)]` integration tests
7. Update `src/swift/Sources/Dexter/Bridge/DexterClient.swift`:
   - Add `sessionID` generation
   - Yield `SystemEvent(CONNECTED)` before session loop
   - Add `textResponse` and `actionRequest` cases to the response switch
8. `cargo build` — zero warnings, zero errors
9. `make test` — 5 existing + 2 new integration tests = 7 tests pass
10. `make run` + visual confirm: disc green + "ClientEvent received" in core log
    + "Text response: 'session ready'" in Swift log

---

## 7. Acceptance criteria

| # | Criterion | How to verify |
|---|-----------|---------------|
| 1 | `cargo build` clean — zero warnings, zero errors | `cargo build` |
| 2 | `make proto` exits 0; Swift artifacts updated | `make proto` |
| 3 | `swift build` passes with new proto artifacts | `swift build` in `src/swift/` |
| 4 | `make test` — 7/7 tests pass (5 existing + 2 new) | `make test` |
| 5 | Phase 1 regression: disc still turns green | `make run` |
| 6 | Client→server: core log shows "ClientEvent received" with `kind=system_event` | `make run`, read core log |
| 7 | Server→client echo: Swift log shows "Text response: 'session ready'" | `make run`, read Swift log |
| 8 | `StreamAudio` returns `UNIMPLEMENTED` in integration test | criterion 4 |
| 9 | All proto message types are complete; no bare strings where typed fields should be | code review of `.proto` |

---

## Notes

**Why `tokio_stream::iter` in tests:**
The test needs to send a finite list of `ClientEvent`s as a stream. `tokio_stream::iter`
wraps a `Vec` as a `Stream` — clean and idiomatic. The stream closes after the last item,
which is what we want for the test (server reader task exits cleanly after None).

**Why two tasks with a oneshot signal instead of `pending()`:**
The reader task terminates when the client closes its request stream or on error. The
hold-open task keeps the response stream alive so the server can push events after the
client stops sending — necessary in Phase 6 when async retrieval or background work
completes after the last ClientEvent.

`std::future::pending()` in the hold-open task leaks: the task and its `tx` clone live
forever regardless of session state. On session restart (Swift reconnects), old hold-open
tasks accumulate. The oneshot drop signal (`_signal_done` dropped by reader task →
`reader_done_rx.await` resolves in hold-open) ties hold-open lifetime to reader lifetime.
Drop IS the notification — no polling, no explicit `.send()` call, no new dependencies.

**Phase 6 replacement:**
CoreOrchestrator holds its own `tx` clone; reader task sends to orchestrator via typed
channel; orchestrator drops `tx` on session end. Hold-open task removed entirely.

**`description_p` field name:**
proto3 field `description` collides with Swift's `CustomStringConvertible.description`.
`protoc-gen-swift` renames it. The exact rename depends on the plugin version — check
the generated file after `make proto` and use whatever name appears there.

**Field tag stability:**
All field numbers in `ClientEvent.oneof` and `ServerEvent.oneof` are final. Tags 3–6
in `ClientEvent` and 2–5 in `ServerEvent` must not change after Phase 3 — proto wire
format compatibility depends on stable field numbers.
