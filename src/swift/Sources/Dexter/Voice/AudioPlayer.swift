import AVFoundation

/// Streams TTS PCM chunks received as `AudioResponse` gRPC events through AVAudioEngine.
///
/// Threading model
/// ───────────────
/// `AudioPlayer` is `@unchecked Sendable`. ALL mutations of `_isPlaying`,
/// `pendingBufferCount`, `sequenceQueue`, and `nextExpectedSeq` happen exclusively
/// on `self.queue` (a serial DispatchQueue). No external caller may access those
/// fields directly.
///
/// `enqueue()` — dispatches async to `self.queue`.
/// `stop()`    — dispatches sync to `self.queue`, so callers on any thread (e.g.
///               VoiceCapture's callbackQueue during barge-in) can rely on the
///               player being fully idle before `stop()` returns.
/// Completion  — AVFoundation fires completion handlers on its internal thread;
///               handlers dispatch async back to `self.queue` to avoid re-entrancy
///               on a steady audio stream (each buffer's handler scheduling the next).
///
/// `isPlaying` — computed via `queue.sync`, readable from any thread.
final class AudioPlayer: @unchecked Sendable {

    // MARK: - Threading

    private let queue = DispatchQueue(label: "com.dexter.audioplayer", qos: .userInteractive)

    // MARK: - Audio graph

    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()

    /// 16kHz int16 mono — matches tts_worker.py output format exactly.
    /// AVAudioEngine resamples to the hardware output rate internally.
    private static let pcmFormat = AVAudioFormat(
        commonFormat: .pcmFormatInt16,
        sampleRate:   16_000,
        channels:     1,
        interleaved:  true
    )!

    // MARK: - Queue-protected state

    /// True when at least one buffer is scheduled or actively playing.
    /// Safe to read from any thread — dispatches sync to self.queue.
    var isPlaying: Bool { queue.sync { _isPlaying } }

    private var _isPlaying:         Bool   = false  // mutated only on self.queue
    private var pendingBufferCount:  Int    = 0      // buffers currently scheduled or playing
    private var sequenceQueue:       [Item] = []     // ordered pending PCM chunks
    private var nextExpectedSeq:     UInt32 = 0      // next sequence number to drain
    private var activeTraceID:       String?
    private var retiredTraceIDs:     [String] = []
    private var activeStreamID:      String?
    private var retiredStreamIDs:    [String] = []

    /// Callback fired on `self.queue` when the last scheduled buffer in a proactive
    /// TTS sequence finishes playing. Set by `DexterClient` to send
    /// `AUDIO_PLAYBACK_COMPLETE` back to Rust. Swift 6: `@Sendable` required because
    /// this closure is called from `self.queue` (a serial DispatchQueue), which is
    /// not actor-isolated.
    var onPlaybackFinished: (@Sendable (_ traceID: String?) -> Void)?

    /// Set to true when an `AudioResponse` with `is_final = true` is enqueued.
    /// When `pendingBufferCount` reaches zero while this flag is set,
    /// `onPlaybackFinished` is called and the flag is reset. Protected by `self.queue`.
    private var awaitingFinalCallback: Bool = false
    private var awaitingFinalTraceID: String?

    private struct Item {
        let data:           Data
        let sequenceNumber: UInt32
    }

    // MARK: - Lifecycle

    /// Attach the player node to the engine and start the audio graph. Idempotent —
    /// safe to call again on session reconnect without stopping an already-running engine.
    ///
    /// Must be called before the first `enqueue()`. Does not need to be called from
    /// any specific thread.
    func start() {
        guard !engine.isRunning else { return }
        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: Self.pcmFormat)
        engine.prepare()
        do {
            try engine.start()
            player.play()   // arm the node; it will play buffers as they are scheduled
        } catch {
            // Non-fatal — audio silently dropped until next session start attempt.
            // text-only fallback remains functional.
            print("[AudioPlayer] Engine start failed: \(error)")
        }
    }

    // MARK: - Playback

    /// Register the current response trace before audio arrives. TextResponse
    /// chunks normally precede AudioResponse chunks for the same turn, so this lets
    /// `stop()` retire a cancelled turn even if no PCM frame reached Swift yet.
    func prepareForResponseTrace(_ traceID: String?) {
        guard let traceID, !traceID.isEmpty else { return }
        queue.async { [self] in
            guard !retiredTraceIDs.contains(traceID) else { return }
            if activeStreamID == nil || activeTraceID == nil {
                activeTraceID = traceID
            }
        }
    }

    /// Enqueue a PCM chunk for sequenced playback. Dispatches asynchronously to
    /// `self.queue` — returns immediately.
    ///
    /// - Parameters:
    ///   - data:           Raw PCM bytes (int16, 16kHz, mono). May be empty for the
    ///                     `is_final` sentinel — empty buffers are not scheduled.
    ///   - sequenceNumber: Position in the ordered stream; out-of-order chunks are
    ///                     held until the gap is filled.
    ///   - isFinal:        When `true`, arms `onPlaybackFinished`. When all previously
    ///                     scheduled buffers have played (or immediately if none were
    ///                     scheduled), `onPlaybackFinished` fires.
    ///   - traceID:         Response trace shared by the text and audio events for one
    ///                     model turn. Retired traces are ignored after cancel.
    ///   - streamID:        Fresh per Rust TTS stream. Frames from retired or non-active
    ///                     streams are ignored so stale post-cancel audio cannot replay
    ///                     as the next stream's sequence 0.
    func enqueue(
        data: Data,
        sequenceNumber: UInt32,
        isFinal: Bool = false,
        traceID: String? = nil,
        streamID: String? = nil
    ) {
        queue.async { [self] in
            guard acceptPlaybackIdentity(traceID: traceID, streamID: streamID) else { return }
            if isFinal {
                awaitingFinalCallback = true
                awaitingFinalTraceID = traceID
                // Empty sentinel: don't add to the sequence queue, but allow
                // non-empty is_final frames to be scheduled normally (future-proofing).
                if !data.isEmpty {
                    sequenceQueue.append(Item(data: data, sequenceNumber: sequenceNumber))
                }
            } else {
                sequenceQueue.append(Item(data: data, sequenceNumber: sequenceNumber))
            }
            flushReadyBuffers()
            // Fire immediately if the queue is already drained when is_final arrives —
            // e.g. all buffers finished playing before the sentinel was received.
            checkFinalCallback()
        }
    }

    /// Stop playback immediately, clear all pending audio, and re-arm the player node
    /// so the next `enqueue()` call begins a fresh playback sequence.
    ///
    /// Dispatches synchronously to `self.queue`. Callers on any thread (including
    /// VoiceCapture's callbackQueue for barge-in) can assume the player is idle
    /// when `stop()` returns.
    func stop() {
        queue.sync { [self] in
            let wasFinal = awaitingFinalCallback
            player.stop()
            // Phase 27: do NOT call player.play() here — lazy re-arm in flushReadyBuffers()
            // prevents stale TTS frames from the cancelled generation from playing.
            // player.play() is called in flushReadyBuffers() only when the first new buffer
            // matching nextExpectedSeq (=0 after reset) is ready to schedule.
            sequenceQueue.removeAll()
            pendingBufferCount    = 0
            nextExpectedSeq       = 0
            _isPlaying            = false
            awaitingFinalCallback = false
            let traceID = awaitingFinalTraceID
            awaitingFinalTraceID  = nil
            retireActivePlayback()
            // Phase 18: if a proactive observation was interrupted by barge-in,
            // fire the callback immediately so Rust receives AUDIO_PLAYBACK_COMPLETE
            // and can transition the entity to IDLE. Without this, the entity would
            // stay stuck in THINKING because playback was aborted before the last
            // buffer completion handler ever fired.
            if wasFinal { onPlaybackFinished?(traceID) }
        }
    }

    // MARK: - Private

    /// Drain contiguous ready chunks from `sequenceQueue` and schedule them on the
    /// player node. Must only be called from `self.queue`.
    private func flushReadyBuffers() {
        // Phase 27: lazy re-arm — start the player node only when a new buffer is
        // about to be scheduled. After stop() the node is stopped; this ensures only
        // frames that match nextExpectedSeq (reset to 0 in stop()) can re-arm the
        // player. Stale in-flight frames from a cancelled generation have seq > 0
        // and do not match, so they never trigger a play() call.
        if !player.isPlaying { player.play() }

        while let idx = sequenceQueue.firstIndex(where: { $0.sequenceNumber == nextExpectedSeq }) {
            let item = sequenceQueue.remove(at: idx)
            nextExpectedSeq += 1
            guard let buffer = makeBuffer(from: item.data) else { continue }

            _isPlaying = true
            pendingBufferCount += 1

            // Completion fires on AVFoundation's internal thread — dispatch async
            // back to self.queue. The async hop breaks the synchronous call chain that
            // would otherwise recurse unboundedly on a steady audio stream.
            //
            // Bind to `this` (not `self`) to avoid capturing the `var self` weak
            // reference into a @Sendable closure — Swift 6 rejects `guard let self`
            // in @Sendable closures when `self` is a captured var.
            player.scheduleBuffer(buffer) { [weak self] in
                guard let this = self else { return }
                this.queue.async { [this] in
                    this.pendingBufferCount -= 1
                    if this.pendingBufferCount == 0 && this.sequenceQueue.isEmpty {
                        this._isPlaying = false
                    }
                    this.flushReadyBuffers()
                    this.checkFinalCallback()   // Phase 18: fire if proactive TTS sequence complete
                }
            }
        }
    }

    /// Fire `onPlaybackFinished` if armed and the queue is fully drained.
    ///
    /// Called after every buffer completion and after the `is_final` sentinel is
    /// enqueued, so the callback fires as soon as both conditions are true —
    /// regardless of order.
    ///
    /// The double-call guard (`awaitingFinalCallback = false` before the call)
    /// prevents re-entrancy: the second call (if any) sees `false` and returns
    /// immediately. Both calls happen on `self.queue` (serial), so no data race.
    ///
    /// Must only be called from `self.queue`.
    private func checkFinalCallback() {
        guard awaitingFinalCallback,
              pendingBufferCount == 0 else { return }
        // Discard any remaining items in sequenceQueue. These can accumulate when a
        // barge-in calls stop() mid-stream: stop() resets nextExpectedSeq to 0 but
        // subsequent TTS frames (seq > 0) still arrive from the gRPC stream and pile
        // up in sequenceQueue without ever matching nextExpectedSeq. Once
        // pendingBufferCount reaches zero there is nothing more to play —
        // clearing the stale entries is correct and unblocks the callback.
        sequenceQueue.removeAll()
        awaitingFinalCallback = false
        let traceID = awaitingFinalTraceID
        awaitingFinalTraceID = nil
        retireActivePlayback()
        onPlaybackFinished?(traceID)
    }

    private func acceptPlaybackIdentity(traceID: String?, streamID: String?) -> Bool {
        guard acceptTraceID(traceID) else { return false }
        guard acceptStreamID(streamID) else { return false }
        return true
    }

    private func acceptTraceID(_ traceID: String?) -> Bool {
        guard let traceID, !traceID.isEmpty else {
            // Backward compatibility for older callers and audio-only responses.
            return true
        }
        if retiredTraceIDs.contains(traceID) {
            return false
        }
        if let activeTraceID, !activeTraceID.isEmpty {
            if activeTraceID == traceID { return true }
            if activeStreamID == nil {
                self.activeTraceID = traceID
                return true
            }
            return false
        }
        activeTraceID = traceID
        return true
    }

    private func acceptStreamID(_ streamID: String?) -> Bool {
        guard let streamID, !streamID.isEmpty else {
            // Backward compatibility for older Rust builds before AudioResponse.stream_id.
            return activeStreamID == nil || activeStreamID == ""
        }
        if retiredStreamIDs.contains(streamID) {
            return false
        }
        if let activeStreamID {
            return activeStreamID == streamID
        }
        activeStreamID = streamID
        return true
    }

    private func retireActivePlayback() {
        retireActiveTrace()
        retireActiveStream()
    }

    private func retireActiveTrace() {
        guard let traceID = activeTraceID, !traceID.isEmpty else {
            activeTraceID = nil
            return
        }
        retiredTraceIDs.append(traceID)
        if retiredTraceIDs.count > 16 {
            retiredTraceIDs.removeFirst(retiredTraceIDs.count - 16)
        }
        activeTraceID = nil
    }

    private func retireActiveStream() {
        guard let streamID = activeStreamID, !streamID.isEmpty else {
            activeStreamID = nil
            return
        }
        retiredStreamIDs.append(streamID)
        if retiredStreamIDs.count > 16 {
            retiredStreamIDs.removeFirst(retiredStreamIDs.count - 16)
        }
        activeStreamID = nil
    }

    /// Convert raw int16 LE PCM bytes to an `AVAudioPCMBuffer` in `pcmFormat`.
    private func makeBuffer(from data: Data) -> AVAudioPCMBuffer? {
        let frameCount = AVAudioFrameCount(data.count / 2)  // 2 bytes per int16 sample
        guard frameCount > 0,
              let buffer = AVAudioPCMBuffer(
                  pcmFormat: Self.pcmFormat,
                  frameCapacity: frameCount
              ) else { return nil }

        buffer.frameLength = frameCount
        data.withUnsafeBytes { raw in
            guard let src = raw.baseAddress?.assumingMemoryBound(to: Int16.self) else { return }
            buffer.int16ChannelData![0].update(from: src, count: Int(frameCount))
        }
        return buffer
    }
}
