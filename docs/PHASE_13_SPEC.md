# Phase 13 — End-to-End Voice
## Spec version 1.0 — Session 015, 2026-03-09

> **Status:** Current phase.
> This document is the authoritative implementation guide for Phase 13.
> All architectural decisions are locked. Implement exactly as written.

---

## 1. What Phase 13 Delivers

A full spoken-language loop:

```
Operator speaks
    │
    ▼ AVCaptureSession (16kHz PCM)
VoiceCapture.swift
    │ Energy-threshold VAD
    ▼ utterance complete
DexterClient.streamAudio() ─────────► Rust stream_audio() RPC
                                            │ per-call WorkerClient(Stt)
                                            ▼ faster-whisper base.en
                                       TranscriptChunk stream
    ◄──────────────────────────────────────┘
DexterClient sends TextInput to session stream
    │
    ▼ Rust CoreOrchestrator
  THINKING state ──► inference ──► SentenceSplitter
    │                                    │
    │               ◄─── AudioResponse ──┘  (TTS worker)
    ▼
AudioPlayer.swift (AVAudioEngine)
    │
    ▼ SPEAKING state during playback ──► IDLE when done
Operator hears response
```

Barge-in (half-duplex, Phase 13):
- VAD fires during TTS playback → `AudioPlayer.stop()` immediately
- New STT utterance starts cleanly
- New transcript → TextInput sent → Rust handles sequentially

---

## 2. What Already Exists (Do Not Rebuild)

From previous phases — verified working before Phase 13 begins:

| Component | Phase | Status |
|-----------|-------|--------|
| `VoiceCoordinator` — TTS worker lifecycle (start/stop/health) | 10 | ✅ |
| `stream_audio()` RPC — per-call STT WorkerClient, TranscriptChunk stream | 10 | ✅ |
| `stt_worker.py` — faster-whisper base.en, AUDIO_CHUNK/AUDIO_END/TRANSCRIPT protocol | 10 | ✅ |
| `tts_worker.py` — kokoro-82M synthesis → 16kHz int16 PCM, TTS_AUDIO/TTS_DONE | 10 | ✅ |
| `SentenceSplitter` — sentence detection in token stream | 10 | ✅ |
| `orchestrator.generate_and_stream()` — TTS synthesis task, `AudioResponse` events | 10 | ✅ |
| `DexterClient.audioResponse` handler stub — prints chunk info | 12 | ✅ (replace print) |
| Proto: `AudioResponse`, `AudioChunk`, `TranscriptChunk` | 3 | ✅ |
| `VoiceCoordinator.health_check_and_restart()` — `#[allow(dead_code)]` pending this phase | 10 | ✅ (wire up) |

---

## 3. Prerequisites

Before implementing, run:
```bash
make setup-python   # installs kokoro, faster-whisper into the uv venv
```

The TTS worker failure seen before Phase 13 (`Handshake JSON parse error: EOF`) is caused by
`kokoro` not being installed. `setup-python` resolves this. Confirm with:
```bash
cd src/python-workers && uv run python workers/tts_worker.py &
# should print the handshake JSON and wait, not exit immediately
```

---

## 4. New Files to Create

### 4.1 `src/swift/Sources/Dexter/Voice/VoiceCapture.swift`

**Purpose:** Capture 16kHz PCM from the microphone, apply energy-threshold VAD, and
deliver complete utterances to `DexterClient` as `AsyncStream<Data>` chunks.

**Why `AVCaptureSession` (not `AVAudioEngine.inputNode.installTap`):**
The implementation plan specifies `AVCaptureSession` with `AVAudioInput` as the capture
path. It gives explicit device selection and is the platform-standard capture API for
dedicated audio recording (vs `AVAudioEngine` which is primarily a playback/processing graph).
`AVCaptureAudioDataOutput.audioSettings` controls the output format — the system resamples
hardware rate (typically 48kHz) to 16kHz internally.

**Architecture:**
```swift
final class VoiceCapture: NSObject, AVCaptureAudioDataOutputSampleBufferDelegate,
                           @unchecked Sendable {

    // MARK: - Callbacks (set before start())
    var onUtteranceComplete: (([Data]) -> Void)?  // called with all PCM buffers for one utterance on falling edge
    var onSpeechStart: (() -> Void)?              // called on VAD rising edge (for barge-in)

    // MARK: - State
    // Threading invariant: vadState, silenceFrames, and utteranceBuffer are
    // exclusively accessed on callbackQueue (the AVCaptureAudioDataOutput delegate
    // queue). onUtteranceComplete and onSpeechStart are invoked only from callbackQueue.
    private var session:          AVCaptureSession?
    private var audioOutput:      AVCaptureAudioDataOutput?
    private var callbackQueue:    DispatchQueue
    private var vadState:         VADState = .silent
    private var silenceFrames:    Int = 0
    private var utteranceBuffer:  [Data] = []    // accumulates PCM chunks during VAD-active

    // MARK: - Constants (all named, no magic numbers)
    private static let outputFormat: [String: Any] = [
        AVFormatIDKey:              kAudioFormatLinearPCM,
        AVSampleRateKey:            16000.0,
        AVNumberOfChannelsKey:      1,
        AVLinearPCMBitDepthKey:     16,
        AVLinearPCMIsFloatKey:      false,
        AVLinearPCMIsBigEndianKey:  false,
        AVLinearPCMIsNonInterleaved: false,
    ]

    enum VADState { case silent, active }
}
```

**VAD implementation** (energy threshold):
- Per callback: compute RMS of the int16 buffer: `sqrt(sum(sample² for sample in buf) / n)`
- Rising edge (SILENT → ACTIVE): RMS > `Constants.VAD_ENERGY_THRESHOLD` for
  `Constants.VAD_ONSET_FRAMES` consecutive frames.
- Falling edge (ACTIVE → SILENT): RMS < `Constants.VAD_ENERGY_THRESHOLD` for
  `Constants.VAD_SILENCE_FRAMES` consecutive frames.
- On rising edge: `utteranceBuffer.removeAll()`, call `onSpeechStart?()`.
- In ACTIVE state: append raw PCM `Data` to `utteranceBuffer` on every callback.
- On falling edge: call `onUtteranceComplete?(utteranceBuffer)`, then `utteranceBuffer.removeAll()`.

**Constants to add to `constants.rs` (Rust) and mirror in `VoiceCapture.swift`:**

In `VoiceCapture.swift` (Swift constants — not in Rust, these are UI-layer parameters):
```swift
// In VoiceCapture.swift private enum Constants:
static let VOICE_SAMPLE_RATE: Double = 16_000
static let VOICE_BIT_DEPTH:   Int    = 16
static let VOICE_CHANNELS:    Int    = 1
static let VAD_ENERGY_THRESHOLD: Float = 0.01   // RMS threshold for speech onset
static let VAD_ONSET_FRAMES:  Int    = 2        // consecutive frames above threshold
static let VAD_SILENCE_FRAMES: Int   = 20       // consecutive silence frames to end utterance
```

**Lifecycle:**
- `start()`: create `AVCaptureSession`, add `AVCaptureDeviceInput` (default mic),
  configure `AVCaptureAudioDataOutput` with `outputFormat`, set `self` as delegate,
  call `session.startRunning()`.
- `stop()`: `session.stopRunning()`, set session to nil.
- `captureOutput(_:didOutput:from:)`: runs on `callbackQueue`, not main thread.

**Why `@unchecked Sendable`:**
Same rationale as `EventBridge` — bridging a C-callback-based API (`AVCaptureAudioDataOutputSampleBufferDelegate`). The delegate callback fires exclusively on `callbackQueue`. ALL reads and writes to `vadState`, `silenceFrames`, and `utteranceBuffer` happen only from that queue. `onUtteranceComplete` and `onSpeechStart` are invoked only from that queue. `@unchecked Sendable` with these documented invariants is correct (see MEMORY.md Swift 6 patterns).

---

### 4.2 `src/swift/Sources/Dexter/Voice/AudioPlayer.swift`

**Purpose:** Receive `AudioResponse` PCM chunks from Rust over gRPC and play them
through `AVAudioEngine`. Stop immediately on barge-in.

**Why `AVAudioEngine` + `AVAudioPlayerNode` (not `AVAudioPlayer`):**
`AVAudioPlayer` requires a file; it doesn't stream. `AVAudioPlayerNode` can schedule
`AVAudioPCMBuffer` objects incrementally — the correct API for streaming TTS output.
`AVAudioEngine` provides the graph (`inputNode → mixerNode → outputNode`) with
hardware-accelerated sample-rate conversion built in.

**Architecture:**
```swift
final class AudioPlayer: @unchecked Sendable {

    // Threading invariant: ALL reads and writes to `_isPlaying`, `sequenceQueue`,
    // and `nextExpectedSeq` happen exclusively on `self.queue`. No external caller
    // may access these fields directly. `@unchecked Sendable` is safe because
    // `self.queue` (a serial DispatchQueue) enforces mutual exclusion.
    // `isPlaying` (the public computed property) dispatches synchronously to
    // `self.queue` so it is safe to call from any thread or queue.
    private let queue = DispatchQueue(label: "com.dexter.audioplayer", qos: .userInteractive)

    private let engine: AVAudioEngine
    private let player: AVAudioPlayerNode
    // 16kHz int16 mono — matches tts_worker.py output format exactly.
    // AVAudioEngine converts to hardware format internally.
    private static let pcmFormat = AVAudioFormat(
        commonFormat: .pcmFormatInt16,
        sampleRate:   16_000,
        channels:     1,
        interleaved:  true
    )!

    // Read from any thread via queue.sync. Never access _isPlaying directly.
    var isPlaying: Bool { queue.sync { _isPlaying } }
    private var _isPlaying:       Bool = false         // mutated only on self.queue
    private var sequenceQueue:    [AudioResponseItem] = []  // mutated only on self.queue
    private var nextExpectedSeq:  UInt32 = 0               // mutated only on self.queue

    struct AudioResponseItem {
        let data: Data
        let sequenceNumber: UInt32
    }
}
```

**Key methods:**
- `start()`: attach `player` to `engine`, connect `player → engine.mainMixerNode`,
  `engine.prepare()`, `try engine.start()`. Call once at init, before any concurrent access.
- `enqueue(data: Data, sequenceNumber: UInt32)`: dispatches **async** to `self.queue`,
  appends to `sequenceQueue`, then calls `flushReadyBuffers()`.
- `flushReadyBuffers()`: **must only be called from `self.queue`**. Consumes contiguous
  chunks in sequence order, converts each to `AVAudioPCMBuffer`, calls
  `player.scheduleBuffer(_:completionHandler:)`. The completion handler fires on
  AVFoundation's internal thread — it dispatches **async** back to `self.queue` and
  calls `flushReadyBuffers()` again, chaining buffers without gaps and without
  re-entrancy. When `sequenceQueue` is empty after draining, sets `_isPlaying = false`.
- `stop()`: dispatches **sync** to `self.queue` — calls `player.stop()`,
  `sequenceQueue.removeAll()`, `nextExpectedSeq = 0`, `_isPlaying = false`.
  `sync` guarantees the stop is fully complete before returning, so callers on any
  thread (including VoiceCapture's callbackQueue) can safely assume the player is
  idle after `stop()` returns.

**`AVAudioPCMBuffer` construction from raw int16 PCM:**
```swift
// data is raw int16 LE PCM at 16kHz mono
let frameCount = AVAudioFrameCount(data.count / 2)  // 2 bytes per int16 sample
guard let buffer = AVAudioPCMBuffer(pcmFormat: Self.pcmFormat,
                                     frameCapacity: frameCount) else { return }
buffer.frameLength = frameCount
data.withUnsafeBytes { ptr in
    guard let int16Ptr = ptr.baseAddress?.assumingMemoryBound(to: Int16.self) else { return }
    // int16ChannelData![0] is the pointer to channel 0 of interleaved int16 format
    buffer.int16ChannelData![0].assign(from: int16Ptr, count: Int(frameCount))
}
```

**Sequence ordering rationale:**
gRPC streams are ordered, but the orchestrator's TTS task sends `AudioResponse` chunks
as TTS sentences complete. Sentence boundaries produce natural chunk order. The sequence
queue handles any edge case where chunks arrive out of order (should not occur in practice
over UDS, but the protocol defines `sequence_number` for a reason).

---

## 5. Files to Modify

### 5.1 `src/swift/Sources/Dexter/Bridge/DexterClient.swift`

**Three changes:**

**A. Add `audioPlayer` and `voiceCapture` fields:**
```swift
actor DexterClient {
    // ... existing fields ...
    private let audioPlayer = AudioPlayer()
    private var voiceCapture: VoiceCapture?
}
```

**B. Replace the `audioResponse` print stub with real playback:**
```swift
case .audioResponse(let audio):
    // Route PCM data to AVAudioEngine for playback.
    // AudioPlayer is @unchecked Sendable — safe to call from this task.
    audioPlayer.enqueue(data: audio.data, sequenceNumber: audio.sequenceNumber)
```

**C. Wire `VoiceCapture` into `runSession`:**

After EventBridge setup (`bridge.start()` / `defer bridge.stop()`), start VoiceCapture:

```swift
// ── VoiceCapture ──────────────────────────────────────────────────────
//
// VoiceCapture buffers PCM internally during each VAD-active period and
// delivers complete utterances via onUtteranceComplete on the falling edge.
// One utterance → one stub.streamAudio() call — matching the per-call
// WorkerClient(Stt) design in ipc/server.rs (stateless per utterance).
//
// Barge-in:
//   onSpeechStart fires while TTS is playing.
//   audioPlayer.stop() is called unconditionally — stop() is idempotent
//   and dispatches sync to its queue, so the player is fully idle before
//   onSpeechStart returns. audioPlayer is @unchecked Sendable — safe
//   cross-queue call from VoiceCapture's callbackQueue.
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
capture.onSpeechStart = { [audioPlayer = self.audioPlayer] in
    // Barge-in: stop TTS unconditionally when new speech is detected.
    // stop() is idempotent — no isPlaying check needed; avoids TOCTOU race.
    // Non-actor context — audioPlayer is @unchecked Sendable.
    audioPlayer.stop()
}
capture.start()

// Drive one stub.streamAudio() call per complete utterance.
// Runs concurrently with the session stream (which handles inference responses).
Task {
    for await utterance in utteranceStream {
        var seqNum: UInt32 = 0
        try? await stub.streamAudio(
            requestProducer: { writer in
                for data in utterance {
                    let chunk = Dexter_V1_AudioChunk.with {
                        $0.data           = data
                        $0.sequenceNumber = seqNum
                        $0.sampleRate     = 16_000
                    }
                    seqNum += 1
                    try await writer.write(chunk)
                }
            },
            onResponse: { [weak self, sessionID] response in
                var transcript = ""
                for try await chunk in response.messages {
                    if chunk.isFinal { transcript = chunk.text }
                }
                guard !transcript.isEmpty else { return }
                let event = Dexter_V1_ClientEvent.with {
                    $0.traceID   = UUID().uuidString
                    $0.sessionID = sessionID
                    $0.textInput = Dexter_V1_TextInput.with { $0.content = transcript }
                }
                await self?.send(event)
            }
        )
    }
}

---

### 5.2 `src/rust-core/src/ipc/server.rs`

**One change: TTS health-check timer in the reader task.**

Add `tokio::select!` to the reader task loop to interleave health checks:

```rust
// In session() reader task, after orchestrator.start_voice().await:
let mut health_interval = tokio::time::interval(
    std::time::Duration::from_secs(VOICE_WORKER_HEALTH_INTERVAL_SECS)
);

loop {
    tokio::select! {
        msg = inbound.message() => {
            match msg {
                Ok(Some(event)) => { /* existing handling */ }
                Ok(None)  => { info!(...); break; }
                Err(e)    => { error!(...); break; }
            }
        }
        _ = health_interval.tick() => {
            // health_check_and_restart takes &mut self on voice — access via orchestrator field
            orchestrator.voice_health_check().await;
        }
    }
}
```

Add a public method to `CoreOrchestrator`:
```rust
pub async fn voice_health_check(&mut self) {
    self.voice.health_check_and_restart().await;
}
```

This removes the `#[allow(dead_code)]` on `health_check_and_restart` in `VoiceCoordinator`.

---

### 5.3 `src/swift/Sources/Dexter/App.swift`

**One change: microphone permission check at startup.**

Before starting `DexterClient`, request microphone access:

```swift
// In applicationDidFinishLaunching, before Task { await c.connect(to: window) }:
AVCaptureDevice.requestAccess(for: .audio) { granted in
    if !granted {
        DispatchQueue.main.async {
            let alert = NSAlert()
            alert.messageText     = "Microphone Access Required"
            alert.informativeText = "Dexter needs microphone access for voice interaction. " +
                                    "Grant access in System Settings → Privacy & Security → Microphone."
            alert.runModal()
        }
    }
}
```

Also add `NSMicrophoneUsageDescription` to `Info.plist` (create if not present):
```xml
<key>NSMicrophoneUsageDescription</key>
<string>Dexter uses the microphone for voice interaction and speech recognition.</string>
```

**Note on `LSUIElement` apps and `Info.plist`:**
SwiftPM does not auto-generate `Info.plist`. It must be created at
`src/swift/Sources/Dexter/Info.plist` and referenced in `Package.swift` under the
`executableTarget` as `infoPlistPath`. If it already exists (from Phase 11), add the key.

---

### 5.4 `src/swift/Package.swift`

If `Info.plist` doesn't exist yet, add it:
```swift
.executableTarget(
    name: "Dexter",
    // ...existing...
    infoPlistPath: "Sources/Dexter/Info.plist"   // ← ADD
)
```

The `Info.plist` must contain at minimum:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>LSUIElement</key><true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>Dexter uses the microphone for voice interaction and speech recognition.</string>
</dict>
</plist>
```

(`LSUIElement = true` was already established in Phase 11 — include it.)

---

## 6. Implementation Order

Phase 13 must be implemented strictly in this order. Each step must build clean before
the next begins.

### Step 1: `make setup-python` + TTS smoke test

Run `make setup-python` to install kokoro + faster-whisper deps.
Manually test the TTS worker:
```bash
cd src/python-workers
uv run python workers/tts_worker.py
# Should print handshake JSON, then wait for input. Ctrl-C to exit.
```

Then `make run` — TTS worker should now start without the handshake error.
The `[DexterClient] Audio chunk seq=N size=NB` prints should appear when you send text.

### Step 2: `AudioPlayer.swift` + DexterClient audioResponse wiring

Create `AudioPlayer.swift`. Wire `audioResponse` in `DexterClient` to call `audioPlayer.enqueue`.
Build + run. Send a TextInput manually (via the existing channel — Phase 12 already has
text input wiring). Entity should go SPEAKING, you should hear audio through speakers.

Verify:
- `swift build` 0 errors, 0 project-code warnings
- Running `make run` + sending a text input → audio plays through speakers
- Entity goes IDLE after last AudioResponse chunk is played

### Step 3: `VoiceCapture.swift` (capture + VAD only, no gRPC yet)

Implement `VoiceCapture.swift` with `onUtteranceComplete` and `onSpeechStart` callbacks.
Add test-only wiring in `App.swift` (not DexterClient) that just `print`s when VAD fires:
```swift
let testCapture = VoiceCapture()
testCapture.onSpeechStart       = { print("[VoiceCapture] Speech start") }
testCapture.onUtteranceComplete = { chunks in
    print("[VoiceCapture] Utterance complete: \(chunks.count) chunk(s), \(chunks.reduce(0) { $0 + $1.count }) bytes")
}
testCapture.start()
```

Verify VAD is working before wiring it to gRPC.

### Step 4: Wire `VoiceCapture` into `DexterClient` (`streamAudio` + `TextInput`)

Remove test wiring from App.swift. Implement the per-utterance `AsyncStream<[Data]>`
architecture in `VoiceCapture`, and the `stub.streamAudio()` loop in `DexterClient`.
Wire `TranscriptChunk` final → `TextInput` → `send(_:)`.

Verify:
- Speak a short utterance
- See `[DexterClient] Text response: 'hello'` (or equivalent) appear
- Entity goes: LISTENING → THINKING → (SPEAKING if TTS working) → IDLE

### Step 5: Barge-in

Wire `onSpeechStart` in DexterClient to call `audioPlayer.stop()`.
Test: start speaking while entity is in SPEAKING state. Audio should stop immediately.
Entity will transition to LISTENING (the next StateChange from Rust when the new utterance
arrives).

### Step 6: Rust health-check timer

Add `voice_health_check()` to `CoreOrchestrator`. Add `tokio::select!` to the reader task
in `ipc/server.rs`. Remove `#[allow(dead_code)]` from `VoiceCoordinator.health_check_and_restart`.

Verify with `cargo test` — all tests must still pass.

### Step 7: `Info.plist` + microphone permission

Create/update `Info.plist`. Update `Package.swift` if needed. Add the permission request
to `App.swift`. Build and run — first launch should show a microphone permission dialog.

### Step 8: Full regression

```bash
cargo test        # must show ≥ 159 tests pass, 0 failures
swift build       # must show 0 errors, 0 project-code warnings
make run          # full system up
```

Manually exercise the complete loop (acceptance criteria below).

---

## 7. Acceptance Criteria

Phase 13 is complete when ALL of the following pass:

| ID | Criterion |
|----|-----------|
| AC-1 | `make setup-python` + `make run` → TTS worker starts, no handshake error in log |
| AC-2 | Sending a text input → audio plays through speaker, entity goes SPEAKING → IDLE |
| AC-3 | `AudioPlayer` respects sequence ordering — no audio gaps or ordering glitches |
| AC-4 | Microphone permission dialog appears on first launch (with `NSMicrophoneUsageDescription`) |
| AC-5 | `VoiceCapture` VAD: speaking into mic → `onSpeechStart` fires within 100ms |
| AC-6 | Full voice loop: speak → LISTENING → THINKING → SPEAKING → hear response |
| AC-7 | Barge-in: speak during SPEAKING → audio stops, new utterance starts |
| AC-8 | TTS health-check timer fires without error in Rust log (interval: `VOICE_WORKER_HEALTH_INTERVAL_SECS`) |
| AC-9 | `cargo test` ≥ 159 tests pass, 0 failures |
| AC-10 | `swift build` 0 errors, 0 project-code warnings |

---

## 8. Known Limitations (Deferred to Phase 15)

These are intentional Phase 13 constraints, not bugs:

1. **Inference cancellation on barge-in:** The Swift AudioPlayer stops, but Rust's ongoing
   inference is NOT cancelled — it completes and sends unused AudioResponse chunks. A
   cancellation token through the session stream is Phase 15 work.

2. **Multiple simultaneous utterances:** If the operator speaks faster than Rust can respond,
   TextInput events queue and execute serially. True duplex requires cancel/interrupt
   propagation through the orchestrator.

3. **TTS latency (first sentence):** Kokoro synthesis starts after the SentenceSplitter detects
   the first sentence boundary (`'. '`, `'! '`, `'? '`, `'\n\n'`). Short responses (single
   sentence) may have higher perceived latency than streamed token output.

4. **VAD threshold calibration:** The initial threshold (`0.01` RMS) is a starting point.
   It will need tuning against real ambient noise conditions. A future improvement is
   adaptive threshold based on recent ambient levels.

5. **STT per-utterance only:** Partial transcripts (`is_final=false`) from faster-whisper are
   not yet used for streaming recognition. Only `is_final=true` chunks produce a TextInput.
   Real-time display of partial transcripts is a future phase improvement.

---

## 9. Session State Update

When Phase 13 is complete, update `SESSION_STATE.json`:
```json
"current": "Phase 14",
"completed_phases": [
  "... (all prior phases)",
  "Phase 13 End-to-End Voice — AC-1 through AC-10 PASS"
],
```

And update `fresh_session_bootstrap_instructions` to:
```
"Phases 1–13 are fully complete — Begin Phase 14 immediately without re-running earlier
phase work. Run 'make test' (verify ≥ 159 Rust tests pass) and 'swift build' (0 warnings)
before starting Phase 14 work."
```
