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
/// Shared mute flag bridging DexterClient actor isolation to the @Sendable gRPC closure.
///
/// Phase 38 / Codex finding [27]: previously this was a bare `var muted: Bool`
/// inside an `@unchecked Sendable` class — the doc claimed staleness was acceptable,
/// but that argument doesn't justify the data race itself. The actor wrote on its
/// executor while the gRPC transport executor read concurrently with no
/// synchronization, which is undefined behavior in Swift's memory model regardless
/// of how harmless the resulting staleness might be in practice.
///
/// NSLock-protected access removes the race without measurable cost (one lock
/// acquire per audio chunk, ~50 ns). `@unchecked Sendable` remains because the
/// class itself is reference-shared and we vouch for its safety via the lock.
private final class TTSGate: @unchecked Sendable {
    private let lock = NSLock()
    private var _muted: Bool = false

    var muted: Bool {
        get {
            lock.lock()
            defer { lock.unlock() }
            return _muted
        }
        set {
            lock.lock()
            _muted = newValue
            lock.unlock()
        }
    }
}

private func audioPlaybackCompletePayload(traceID: String?) -> String {
    guard let traceID, !traceID.isEmpty else { return "{}" }
    guard JSONSerialization.isValidJSONObject(["audio_trace_id": traceID]),
          let data = try? JSONSerialization.data(withJSONObject: ["audio_trace_id": traceID]),
          let payload = String(data: data, encoding: .utf8) else {
        return "{}"
    }
    return payload
}

actor DexterClient {

    private static let socketPath = "/tmp/dexter.sock"
    private static let retryDelay = Duration.milliseconds(500)

    // AudioPlayer persists across session reconnects — the engine is started once
    // and stays running for the process lifetime. Declared as a `let` constant so
    // the reference is stable and can be safely captured in @Sendable closures.
    private let audioPlayer = AudioPlayer()

    // VoiceCapture is created and destroyed within each session in runSession().
    // Stored here so it is reachable for barge-in tests if needed; lifetime is
    // managed entirely by the runSession() defer block.
    private var voiceCapture: VoiceCapture?

    /// Live continuation for the client-event channel.
    ///
    /// Non-nil only while a session is established. The requestProducer in
    /// `runSession` iterates this stream — keeping it open holds the writer
    /// alive without any sleep. `send(_:)` yields into it from Phase 6 onward.
    private var eventContinuation: AsyncStream<Dexter_V1_ClientEvent>.Continuation?

    /// The session ID for the currently active session.
    ///
    /// Phase 25: set at the start of `runSession`, cleared on exit. Allows
    /// `sendTypedInput` to construct a `ClientEvent` with the correct session ID
    /// without coupling the public API to `runSession`'s local scope.
    private var currentSessionID: String? = nil

    /// Shared mute flag — readable from the @Sendable gRPC onResponse closure.
    /// Actor writes it; gRPC closure reads it. See TTSGate for safety rationale.
    private let ttsGate = TTSGate()

    // MARK: - Connection lifecycle

    func connect(to window: FloatingWindow) async {
        // AnimatedEntity starts in .idle — the correct pre-connection visual.
        // Rust drives the first EntityStateChange when the session is established;
        // no explicit "connecting" indicator state is needed.

        // Retry loop — Rust core may start after the Swift shell.
        // runSession() returns normally when the server closes the stream,
        // and throws on connection failure. Either way, retry fires immediately.
        while !Task.isCancelled {
            do {
                try await runSession(window: window)
            } catch {
                // Log every failure so connection problems are visible during development.
                // Phase 2 replaces this with the structured logging layer.
                print("[DexterClient] Session error (retrying in \(Self.retryDelay)): \(error)")
                // Return entity to idle on session drop — Rust will send a new
                // EntityStateChange once the reconnected session is ready.
                await MainActor.run { window.animatedEntity.entityState = .idle }
                try? await Task.sleep(for: Self.retryDelay)
            }
        }
    }

    // MARK: - Session

    private func runSession(window: FloatingWindow) async throws {
        // .http2NIOPosix() is the ClientTransport protocol extension factory — it
        // avoids referencing the deprecated HTTP2ClientTransport namespace directly.
        // transportSecurity: .plaintext = h2c (cleartext HTTP/2), matching tonic's
        // serve_with_incoming which does not add TLS to the Unix domain socket stream.
        //
        // Note: withGRPCClient itself is marked deprecated in grpc-swift 2.2.3
        // (https://forums.swift.org/t/80177) as a signal of a planned API redesign.
        // It is still the correct and only available API in this release.
        try await withGRPCClient(
            transport: .http2NIOPosix(
                // authority: "localhost" is the conventional synthetic authority for gRPC-over-UDS.
                // Without it, grpc-swift falls back to the raw socket path ("/tmp/dexter.sock"),
                // which gets percent-encoded as "%2Ftmp%2Fdexter.sock" — an invalid HTTP/2
                // :authority value that the h2 crate rejects with RST_STREAM PROTOCOL_ERROR.
                // See: grpc-swift-nio-transport NameResolver+UDS.swift line 76.
                target: .unixDomainSocket(path: Self.socketPath, authority: "localhost"),
                transportSecurity: .plaintext
            )
        ) { client in
            let stub = Dexter_V1_DexterService.Client(wrapping: client)

            // UUID v4 trace ID for the ping — stable identifier for this connection attempt.
            let pingTraceID = UUID().uuidString

            // Confirm liveness before opening the session stream.
            let pong = try await stub.ping(
                Dexter_V1_PingRequest.with { $0.traceID = pingTraceID }
            )
            print("[DexterClient] Ping OK — core version: \(pong.coreVersion)")
            // Entity visual state is driven entirely by EntityStateChange server events.
            // Rust sends the first state transition (typically IDLE) when the
            // session stream opens; no explicit "connected" indicator is needed here.

            // Stable session ID for the lifetime of this Session call.
            // All ClientEvents in this session carry this ID for correlation.
            let sessionID = UUID().uuidString
            // Phase 25: expose sessionID to sendTypedInput() without coupling it
            // to the local scope. Cleared in the defer block below alongside
            // eventContinuation so the two are always in sync.
            self.currentSessionID = sessionID

            // Start AVAudioEngine for TTS playback. Idempotent — engine.isRunning
            // guard in start() makes repeated calls on reconnect a no-op.
            audioPlayer.start()

            // Phase 18: wire playback-complete callback so Rust knows when TTS audio
            // finishes playing (not just synthesising). Set before the gRPC stream opens
            // so it is always armed before any AudioResponse events can arrive.
            // Task { await client.send } hops from self.queue (DispatchQueue) to the
            // DexterClient actor executor — the established Swift 6 pattern.
            audioPlayer.onPlaybackFinished = { [weak self, sessionID] audioTraceID in
                guard let client = self else { return }
                Task {
                    let event = Dexter_V1_ClientEvent.with {
                        $0.traceID   = UUID().uuidString
                        $0.sessionID = sessionID
                        $0.systemEvent = Dexter_V1_SystemEvent.with {
                            $0.type    = .audioPlaybackComplete
                            $0.payload = audioPlaybackCompletePayload(traceID: audioTraceID)
                        }
                    }
                    await client.send(event)
                }
            }

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
                // Safety net: cleans up if stub.session throws before onResponse
                // ever runs (e.g. connection drops before the stream is opened).
                // continuation.finish() is a no-op if already called by onResponse.
                continuation.finish()
                self.eventContinuation = nil
                self.currentSessionID  = nil
            }

            // ── EventBridge lifecycle ─────────────────────────────────────────
            //
            // EventBridge is an @unchecked Sendable class (not @MainActor) that
            // bridges macOS AX/NSWorkspace/DistributedNC APIs to the session stream.
            // All callbacks are guaranteed to fire on the main thread by design
            // (NSWorkspace: operationQueue: .main, AX: CFRunLoopGetMain()).
            //
            // The sendEvent closure uses Task { await } to hop from the main thread
            // to DexterClient's actor executor before calling the actor-isolated
            // send(_:) method — required by Swift 6 strict concurrency.
            //
            // stop() dispatches cleanup to the main thread via DispatchQueue.main.async;
            // it returns immediately, making it safe for `defer`.
            let bridge = EventBridge { [weak self] event in
                Task { [weak self] in await self?.send(event) }
            }
            bridge.start()
            defer { bridge.stop() }

            // ── VoiceCapture ──────────────────────────────────────────────────
            //
            // Phase 34: push-to-talk only model. VoiceCapture buffers PCM during
            // each VAD-active period and delivers complete utterances via
            // onUtteranceComplete on the falling edge.
            // One utterance → one stub.streamAudio() call — matching the per-call
            // WorkerClient(Stt) design in ipc/server.rs (stateless per utterance).
            //
            // Activation is driven by the hotkey → Rust LISTENING state round-trip:
            // capture.activate() is called when the entityState handler receives
            // .listening below. This ensures VoiceCapture arms only when Rust
            // confirms it is ready to receive audio.
            let (utteranceStream, utteranceContinuation) = AsyncStream<[Data]>.makeStream()
            let capture = VoiceCapture()
            self.voiceCapture = capture
            defer {
                capture.stop()
                utteranceContinuation.finish()
                self.voiceCapture = nil
            }

            capture.onUtteranceComplete = { utterance in
                utteranceContinuation.yield(utterance)
            }
            capture.start()

            // Drive one stub.streamAudio() call per complete utterance.
            // Runs concurrently with the session stream (which handles inference responses).
            // `sessionID` captured for TextInput event construction.
            let streamAudioTask = Task { [stub, sessionID] in
                for await utterance in utteranceStream {
                    try? await stub.streamAudio(
                        requestProducer: { writer in
                            // Use enumerated() to avoid capturing a mutable `var seqNum`
                            // in a @Sendable closure — Swift 6 forbids that pattern.
                            for (idx, data) in utterance.enumerated() {
                                let chunk = Dexter_V1_AudioChunk.with {
                                    $0.data           = data
                                    $0.sequenceNumber = UInt32(idx)
                                    $0.sampleRate     = 16_000
                                }
                                try await writer.write(chunk)
                            }
                        },
                        onResponse: { [weak self, sessionID, window] response in
                            var transcript = ""
                            var fastPath   = false
                            for try await chunk in response.messages {
                                if chunk.isFinal {
                                    transcript = chunk.text
                                    // Phase 24c: fast_path=true means Rust already delivered
                                    // this transcript to the orchestrator directly — echoing
                                    // it back as TextInput would trigger duplicate inference.
                                    fastPath = chunk.fastPath
                                }
                            }
                            // Log every STT result so we can diagnose empty-transcript failures.
                            print("[DexterClient] STT result: \"\(transcript)\" (fast_path: \(fastPath), \(transcript.count) chars)")
                            // Forward final transcript to the inference pipeline.
                            guard !transcript.isEmpty else {
                                print("[DexterClient] Empty transcript — resetting entity to idle")
                                // Without this, Rust stays in EntityState::Listening forever —
                                // the hotkey handler sends Listening but only a TextInput or
                                // AudioPlaybackComplete drives a transition away from it.
                                // AudioPlaybackComplete → Idle (or stays Alert if action pending).
                                let resetEvent = Dexter_V1_ClientEvent.with {
                                    $0.traceID   = UUID().uuidString
                                    $0.sessionID = sessionID
                                    $0.systemEvent = Dexter_V1_SystemEvent.with {
                                        $0.type = .audioPlaybackComplete
                                    }
                                }
                                await self?.send(resetEvent)
                                return
                            }
                            // Phase 25: show what the operator said in the HUD before
                            // inference starts. Fires before the fast-path guard so both
                            // code paths (echo + fast-path) display the transcript.
                            // window is FloatingWindow (@unchecked Sendable) — same capture
                            // pattern used throughout the session onResponse closure.
                            await MainActor.run { window.hud.showOperatorInput(transcript) }

                            // Phase 24c: suppress the echo when Rust already has the transcript.
                            // This eliminates the duplicate inference that would otherwise occur
                            // when both the fast-path delivery AND the TextInput echo reach the
                            // orchestrator for the same utterance.
                            guard !fastPath else {
                                print("[DexterClient] Fast-path transcript — Rust received directly, echo suppressed")
                                return
                            }
                            let event = Dexter_V1_ClientEvent.with {
                                $0.traceID   = UUID().uuidString
                                $0.sessionID = sessionID
                                $0.textInput = Dexter_V1_TextInput.with {
                                    $0.content   = transcript
                                    $0.fromVoice = true  // Phase 34: voice input → TTS output enabled
                                }
                            }
                            await self?.send(event)
                        }
                    )
                }
            }
            defer { streamAudioTask.cancel() }

            // ── Bidirectional session stream ──────────────────────────────────
            //
            // Lifetime contract:
            //   1. requestProducer yields CONNECTED then blocks on clientEvents.
            //   2. Rust sends IDLE + TextResponse("session ready") in response.
            //   3. onResponse's for-await exits when Rust closes its side (END_STREAM).
            //   4. onResponse's defer calls continuation.finish().
            //   5. continuation.finish() unblocks requestProducer's for-await.
            //   6. requestProducer exits cleanly → grpc-swift sends END_STREAM.
            //   7. stub.session() returns; outer defer clears eventContinuation.
            //
            // Without step 4, continuation.finish() would only run in the outer
            // defer AFTER stub.session returns — but stub.session can't return
            // until requestProducer exits — deadlock broken only by grpc-swift
            // force-cancelling requestProducer, which sends RST_STREAM instead
            // of END_STREAM and causes the "stream unexpectedly closed" error.
            try await stub.session(requestProducer: { [clientEvents, sessionID] writer in
                // CONNECTED is the first event in every session — signals to Rust that
                // Swift is live and the bidirectional stream is open.
                let connectedEvent = Dexter_V1_ClientEvent.with {
                    $0.traceID   = UUID().uuidString
                    $0.sessionID = sessionID
                    $0.systemEvent = Dexter_V1_SystemEvent.with {
                        $0.type = .connected
                    }
                }
                try await writer.write(connectedEvent)

                // Block on the async stream — events sent via send(_:) flow through here.
                // The loop exits when continuation.finish() is called by onResponse.
                for await event in clientEvents {
                    print("[DexterClient] requestProducer → writing event traceID=\(event.traceID.prefix(8)) to gRPC stream")
                    try await writer.write(event)
                    print("[DexterClient] requestProducer → write OK")
                }
            }, onResponse: { [continuation, window, sessionID, audioPlayer, bridge, capture, ttsGate] response in
                // continuation is Sendable — safe to capture in @Sendable closure.
                // window and sessionID are local lets in runSession; captured for
                // entity state updates and ActionApproval correlation respectively.
                //
                // finish() here is the primary trigger that unblocks requestProducer.
                defer { continuation.finish() }
                for try await event in response.messages {
                    switch event.event {
                    case .entityState(let change):
                        let state = EntityState(from: change.state)
                        print("[DexterClient] onResponse ← entityState: \(state)")

                        // Round 3 / behavioral fix: flush queued TTS audio when the
                        // entity transitions to IDLE or LISTENING. Without this,
                        // cancellation ("stop") kills the generation but already-
                        // dispatched PCM buffers continue playing for up to ~60s.
                        // AudioPlayer.stop() is queue.sync — safe from any thread.
                        if state == .idle || state == .listening {
                            audioPlayer.stop()
                        }

                        await MainActor.run {
                            window.animatedEntity.entityState = state

                            // Phase 25: drive HUD visibility from entity state.
                            // THINKING → response incoming, arm HUD for tokens.
                            // IDLE / LISTENING → response done, schedule dismiss if
                            // responseComplete() hasn't already armed the timer.
                            switch state {
                            case .thinking:           window.hud.beginResponseStreaming()
                            case .idle, .listening:   window.hud.scheduleAutoDismiss()
                            default: break
                            }
                        }

                        // Push-to-talk gate: arm VoiceCapture for exactly one utterance
                        // when Rust confirms LISTENING state. The hotkey press → Rust
                        // LISTENING round-trip is the intentional activation signal.
                        // `capture` is captured directly (VoiceCapture is @unchecked Sendable)
                        // rather than going through the actor to avoid the isolation error.
                        // activate() dispatches to callbackQueue internally — thread-safe.
                        if state == .listening {
                            capture.activate()
                        }

                    case .textResponse(let resp):
                        print("[DexterClient] onResponse ← textResponse: isFinal=\(resp.isFinal), \(resp.content.prefix(40))...")
                        audioPlayer.prepareForResponseTrace(event.traceID)
                        await MainActor.run {
                            window.hud.appendToken(resp.content)
                            if resp.isFinal { window.hud.responseComplete() }
                        }

                    case .actionRequest(let req):
                        // Proto contract: SAFE actions are executed by Rust immediately
                        // without waiting for an ActionApproval. Sending one would be
                        // a protocol violation — the Rust side has already moved on.
                        guard req.category != .safe else {
                            print("[DexterClient] Action [\(req.actionID)] SAFE — auto-executed by core")
                            break
                        }

                        // CAUTIOUS / DESTRUCTIVE: present a confirmation sheet and
                        // await the operator's decision.
                        //
                        // Swift 6 removed the async-body overload of MainActor.run
                        // to prevent accidental main-actor-executor starvation.
                        // The replacement pattern is: withCheckedContinuation +
                        // Task { @MainActor in }. The Task runs on the main actor,
                        // allowing `await alert.beginSheetModal(for:)` to suspend
                        // cooperatively without holding the gRPC receive executor.
                        //
                        // The entity stays in whatever state Rust last set (typically
                        // ALERT) for the duration of the modal — the intended Phase 8
                        // behavior: Dexter remains visually active during confirmation.
                        let approved: Bool = await withCheckedContinuation { continuation in
                            Task { @MainActor in
                                let alert = NSAlert()
                                alert.messageText     = req.category == .destructive
                                                        ? "⚠️ Destructive Action"
                                                        : "Action Request"
                                // description_p: protoc-gen-swift appends _p to field names
                                // that collide with Swift reserved words. `description` is
                                // used by CustomStringConvertible, hence the suffix.
                                alert.informativeText = req.description_p
                                alert.addButton(withTitle: "Approve")
                                alert.addButton(withTitle: "Deny")
                                let response = await alert.beginSheetModal(for: window)
                                continuation.resume(returning: response == .alertFirstButtonReturn)
                            }
                        }

                        let approval = Dexter_V1_ClientEvent.with {
                            $0.traceID   = UUID().uuidString
                            $0.sessionID = sessionID
                            $0.actionApproval = Dexter_V1_ActionApproval.with {
                                $0.actionID = req.actionID
                                $0.approved = approved
                                // operator_note left empty; Phase 13+ may add a text field
                            }
                        }
                        await self.send(approval)

                    case .audioResponse(let audio):
                        // When TTS is muted, drop PCM chunks — text path still works.
                        // A is_final chunk with no preceding data means we must still
                        // synthesise a playback-complete signal so Rust's IDLE gate
                        // isn't stuck waiting for audio that will never play.
                        // We do that by letting AudioPlayer receive a data-less is_final.
                        if ttsGate.muted && !audio.isFinal { break }
                        // Route PCM chunks to AVAudioEngine for sequenced playback.
                        // isFinal arms the playback-complete callback in AudioPlayer (Phase 18).
                        // audioPlayer is @unchecked Sendable — safe to call from this
                        // @Sendable onResponse closure without actor-hopping.
                        audioPlayer.enqueue(
                            data:           ttsGate.muted ? Data() : Data(audio.data),
                            sequenceNumber: audio.sequenceNumber,
                            isFinal:        audio.isFinal,
                            traceID:        event.traceID,
                            streamID:       audio.streamID
                        )

                    case .configSync(let cs):
                        // Phase 18: Rust pushed session config — update EventBridge hotkey params.
                        // updateHotkeyConfig(_:) must run on the main thread (EventBridge's
                        // threading contract: all state accessed exclusively on main thread).
                        // Use withCheckedContinuation + Task @MainActor — the established
                        // Swift 6 pattern for main-thread work from an async context.
                        await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
                            Task { @MainActor in
                                bridge.updateHotkeyConfig(cs.hotkey)
                                cont.resume()
                            }
                        }

                    case .vadHint(let hint):
                        // Phase 24c: Rust detected a yes/no question in the last sentence
                        // of TTS output and is asking us to shorten the silence timeout for
                        // the next utterance.  applyVadHint dispatches to callbackQueue
                        // internally — thread-safe from this @Sendable onResponse closure.
                        capture.applyVadHint(hint.silenceFrames)

                    case .none:
                        break
                    }
                }
            })
        }
    }

    // MARK: - Public send API (used from Phase 6 onward)

    /// Inject an operator event into the active session stream.
    /// Silently dropped if no session is currently established.
    func send(_ event: Dexter_V1_ClientEvent) {
        eventContinuation?.yield(event)
    }

    /// Enable or disable TTS playback. Safe to call at any time — actor-isolated.
    func setTtsMuted(_ muted: Bool) {
        ttsGate.muted = muted
        print("[DexterClient] TTS \(muted ? "muted" : "unmuted")")
    }

    /// Send typed text from the HUD input field into the inference pipeline.
    ///
    /// Phase 25: called from App.swift's `hud.onTextSubmit` closure via
    /// `Task { await c?.sendTypedInput(text) }` — already hopped to the actor executor.
    /// Async so it can call `await send(event)` correctly within the actor.
    /// No-ops silently if no session is active (submit fires faster than session opens,
    /// or fires during reconnect gap).
    func sendTypedInput(_ text: String) async {
        print("[DexterClient] sendTypedInput called: '\(text)' | sessionID=\(currentSessionID ?? "NIL") | continuation=\(eventContinuation != nil ? "live" : "NIL")")
        guard let sessionID = currentSessionID else {
            print("[DexterClient] sendTypedInput DROPPED — no active session")
            return
        }
        let event = Dexter_V1_ClientEvent.with {
            $0.traceID   = UUID().uuidString
            $0.sessionID = sessionID
            $0.textInput = Dexter_V1_TextInput.with { $0.content = text }
        }
        send(event)
        print("[DexterClient] sendTypedInput enqueued to stream")
    }
}
