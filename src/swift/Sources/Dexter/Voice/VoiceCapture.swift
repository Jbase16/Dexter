import AVFoundation

/// Captures microphone audio via AVCaptureSession, applies energy-threshold VAD,
/// and delivers complete utterances as `[Data]` arrays on the falling edge.
///
/// Threading model
/// ───────────────
/// `VoiceCapture` is `@unchecked Sendable`. ALL reads and writes to `vadState`,
/// `onsetFrames`, `silenceFrames`, and `utteranceBuffer` happen exclusively on
/// `callbackQueue` — the delegate queue passed to `AVCaptureAudioDataOutput`.
/// `onUtteranceComplete` and `onSpeechStart` are invoked only from `callbackQueue`.
///
/// Consumers (DexterClient) capture the callbacks in non-actor closures and must
/// not make assumptions about which thread they fire on, but may rely on serial
/// ordering — callbacks never interleave.
///
/// Audio format
/// ────────────
/// 16kHz, 16-bit, mono, little-endian interleaved PCM.
/// Matches the `AudioChunk` format expected by `stt_worker.py` and the proto
/// `AudioChunk` message exactly. The system resamples from the hardware rate
/// (typically 48kHz) to 16kHz internally.
final class VoiceCapture: NSObject, AVCaptureAudioDataOutputSampleBufferDelegate,
                           @unchecked Sendable {

    // MARK: - Callbacks (set before start())

    /// Called with all buffered PCM chunks for one complete utterance on the VAD
    /// falling edge (ACTIVE → SILENT transition). The array contains at least one
    /// element. Each `Data` is a raw int16 LE PCM buffer at 16kHz mono.
    var onUtteranceComplete: (([Data]) -> Void)?

    // MARK: - Constants

    private enum Constants {
        static let VOICE_SAMPLE_RATE:  Double = 16_000
        static let VOICE_BIT_DEPTH:    Int    = 16
        static let VOICE_CHANNELS:     Int    = 1
        /// RMS amplitude threshold (normalized [0, 1]) for speech onset detection.
        /// Lowered from 0.01 → 0.003: webcam/distance mics produce speech RMS in
        /// the 0.003–0.020 range; 0.003 is 4× above the measured ambient (~0.0007)
        /// so onset is clean while capturing the full leading consonant of each word.
        static let VAD_ENERGY_THRESHOLD: Float = 0.003
        /// Consecutive above-threshold frames required to declare speech start.
        /// Filters out brief transients (keyboard clicks, paper rustles).
        static let VAD_ONSET_FRAMES:   Int    = 2
        /// Consecutive below-threshold frames required to declare utterance end.
        /// At 16kHz with ~512-frame buffers ≈ 32ms/frame, 20 frames ≈ 640ms silence.
        static let VAD_SILENCE_FRAMES: Int    = 20
    }

    /// AVCaptureAudioDataOutput audio settings — requests 16kHz int16 mono PCM.
    /// The system's audio conversion pipeline resamples from the hardware rate.
    ///
    /// `nonisolated(unsafe)`: [String: Any] is not Sendable in Swift 6. This dict is
    /// a write-once compile-time constant and is never mutated — concurrent reads are safe.
    private nonisolated(unsafe) static let outputFormat: [String: Any] = [
        AVFormatIDKey:               kAudioFormatLinearPCM,
        AVSampleRateKey:             Constants.VOICE_SAMPLE_RATE,
        AVNumberOfChannelsKey:       Constants.VOICE_CHANNELS,
        AVLinearPCMBitDepthKey:      Constants.VOICE_BIT_DEPTH,
        AVLinearPCMIsFloatKey:       false,
        AVLinearPCMIsBigEndianKey:   false,
        AVLinearPCMIsNonInterleaved: false,
    ]

    // MARK: - VAD state
    // All fields below are accessed exclusively on callbackQueue.

    private enum VADState { case silent, active }

    private var vadState:        VADState = .silent
    private var onsetFrames:     Int      = 0   // consecutive above-threshold frames (SILENT state)
    private var silenceFrames:   Int      = 0   // consecutive below-threshold frames (ACTIVE state)
    private var utteranceBuffer: [Data]   = []  // PCM chunks accumulated during VAD-active

    // MARK: - Adaptive ambient calibration
    //
    // Measures the mic's noise floor over the first 100 audio frames (~3 s) and
    // sets the working threshold to max(floor, min(cap, ambient × multiplier)).
    //
    // Multiplier: 2× gives ~6 dB headroom — enough to reject steady-state noise
    // (fans, HVAC) while still capturing the soft attack of consonants at speech
    // onset.  4× was too aggressive: with a moderately noisy room (ambient 0.019)
    // the threshold hit 0.075, causing the VAD to miss word-initial consonants
    // and hand whisper a truncated payload that hallucinated completely different words.
    //
    // Cap: 0.035 is the maximum threshold regardless of ambient RMS.  Any real
    // speech from a mic at normal desktop distance will be well above 0.035.
    // The cap prevents a single loud startup transient from permanently raising
    // the threshold so high that quiet speech onsets are swallowed.
    //
    // Pre-roll buffer (preRollFrames below): 6 frames (~192 ms) are always kept
    // in a circular buffer.  On every VAD rising edge those frames are prepended
    // to utteranceBuffer so whisper receives audio that begins before the threshold
    // was crossed — recovering the leading consonant that triggered the edge.
    //
    // All fields are accessed exclusively on callbackQueue.
    private static let ambientCalibrationFrames = 100
    private static let ambientMultiplier:  Float = 2.0
    // Raised from 0.035 → 0.080: the original cap defeated adaptive calibration
    // in noisy environments (TV, HVAC). With 100-frame averaging the calibration
    // is already transient-resistant; the cap is only needed to catch pathological
    // ambient levels. 0.080 lets TV/fan noise be correctly rejected while real
    // speech (desktop mic at arm's length, RMS typically 0.05–0.25) remains detectable.
    private static let maxThresholdCap:    Float = 0.080
    private static let preRollFrames:      Int   = 6    // ~192 ms look-back at 32 ms/frame
    private var ambientSamples:  [Float] = []           // accumulates RMS during calibration
    private var preRollBuffer:   [Data]  = []           // circular pre-roll buffer (callbackQueue)
    /// Working threshold — starts at the compile-time floor, then set after
    /// calibration completes.  Clamped to [floor, cap].
    private var workingThreshold: Float = Constants.VAD_ENERGY_THRESHOLD

    /// Phase 24c: one-shot VAD silence-frame override.
    ///
    /// Set by `applyVadHint()` when Rust sends a `VadHint` during TTS streaming
    /// (e.g. 8 frames/256ms for a yes/no question vs the default 20/640ms).
    /// Consumed and reset to `nil` on the next falling edge so subsequent utterances
    /// use `Constants.VAD_SILENCE_FRAMES` again.
    private var silenceFrameOverride: Int? = nil

    /// Push-to-talk gate — two-stage armed/active design.
    ///
    /// `isArmed` is set by `activate()` (hotkey press confirmed by Rust).
    /// `isActive` is set by the VAD rising edge — ONLY when `isArmed` is true.
    ///
    /// This prevents a race where ambient noise triggers a VAD rising edge
    /// BEFORE `activate()` executes: without `isArmed`, the VAD could be in
    /// `.active` state when the hotkey fires, causing `isActive` to be set while
    /// the silence countdown is already running, delivering ambient-noise frames
    /// to Whisper and producing empty transcripts.
    ///
    /// `activate()` also resets all VAD state, cancelling any in-progress
    /// ambient-noise cycle so the next rising edge is guaranteed to be real speech.
    ///
    /// Lifecycle per hotkey press:
    ///   activate() → isArmed=true, VAD reset
    ///   rising edge (with isArmed) → isActive=true, isArmed=false
    ///   falling edge (with isActive) → deliver utterance, isActive=false
    ///
    /// Accessed exclusively on `callbackQueue` (same contract as all other VAD state).
    private var isArmed:  Bool = false   // set by activate(); gates the rising edge
    private var isActive: Bool = false   // set by rising edge; gates falling-edge delivery

    // MARK: - Capture session state (mutated on main thread in start/stop only)

    private var session:      AVCaptureSession?
    private var audioOutput:  AVCaptureAudioDataOutput?

    // Serial queue for AVCaptureAudioDataOutputSampleBufferDelegate callbacks.
    // All VAD state mutations happen here — satisfying the @unchecked Sendable contract.
    private let callbackQueue = DispatchQueue(label: "com.dexter.voicecapture",
                                              qos:   .userInteractive)

    // MARK: - Diagnostics counters (callbackQueue only)
    private var diagFrameCount:  Int = 0
    /// Frames since activate() was called — used to log RMS while waiting for speech onset.
    /// Reset to 0 on each activate(). Only logged for the first N frames to avoid floods.
    private var activeLogFrames: Int = 0

    // MARK: - Lifecycle

    /// Start the capture session. Explicitly requests microphone authorization,
    /// then configures AVCaptureSession with the default microphone, sets up
    /// 16kHz int16 mono output, and begins capturing.
    ///
    /// Safe to call from any thread. Returns immediately if the microphone is
    /// unavailable — `onUtteranceComplete` will never fire.
    func start() {
        let status = AVCaptureDevice.authorizationStatus(for: .audio)
        print("[VoiceCapture] Authorization status at start(): \(status.rawValue) " +
              "(0=notDetermined, 1=restricted, 2=denied, 3=authorized)")

        switch status {
        case .authorized:
            startSession()
        case .notDetermined:
            // Request permission; launch session on the grant callback.
            print("[VoiceCapture] Requesting microphone authorization…")
            AVCaptureDevice.requestAccess(for: .audio) { granted in
                print("[VoiceCapture] Authorization response: granted=\(granted)")
                if granted { self.startSession() }
            }
        case .denied, .restricted:
            print("[VoiceCapture] Microphone access denied/restricted — voice input disabled")
        @unknown default:
            print("[VoiceCapture] Unknown authorization status \(status.rawValue) — attempting session start")
            startSession()
        }
    }

    /// Internal: configure and start the AVCaptureSession.
    /// Only called after authorization is confirmed.
    private func startSession() {
        let s = AVCaptureSession()

        guard let device = AVCaptureDevice.default(for: .audio) else {
            print("[VoiceCapture] No audio capture device available — voice input disabled")
            return
        }
        print("[VoiceCapture] Using audio device: \(device.localizedName)")

        guard let input = try? AVCaptureDeviceInput(device: device) else {
            print("[VoiceCapture] Could not create audio device input — voice input disabled")
            return
        }

        guard s.canAddInput(input) else {
            print("[VoiceCapture] Cannot add audio input to session — voice input disabled")
            return
        }
        s.addInput(input)

        let output = AVCaptureAudioDataOutput()
        output.audioSettings = Self.outputFormat
        output.setSampleBufferDelegate(self, queue: callbackQueue)

        guard s.canAddOutput(output) else {
            print("[VoiceCapture] Cannot add audio output to session — voice input disabled")
            return
        }
        s.addOutput(output)

        audioOutput = output
        session     = s
        s.startRunning()
        print("[VoiceCapture] Session startRunning() called — isRunning=\(s.isRunning)")
    }

    /// Arm the push-to-talk gate for one utterance.
    ///
    /// Called by `DexterClient` when the Rust core sends `EntityState.listening`
    /// in response to a hotkey press. Dispatches async to `callbackQueue` so all
    /// VAD state mutations are serialized.
    ///
    /// Critically, this also RESETS the VAD state machine. If ambient noise drove
    /// a rising edge before activate() executes (a common race: the hotkey tap
    /// itself is audio), that in-progress cycle is cancelled. The next rising edge
    /// detected after isArmed=true is guaranteed to be real intentional speech.
    func activate() {
        callbackQueue.async {
            // Cancel any in-progress ambient-noise VAD cycle.
            self.vadState       = .silent
            self.utteranceBuffer.removeAll()
            self.silenceFrames  = 0
            self.onsetFrames    = 0
            self.isActive       = false
            // Arm the gate — the next rising edge above threshold will trigger delivery.
            self.isArmed        = true
            self.activeLogFrames = 0
            print(String(format: "[VoiceCapture] Armed — waiting for speech above threshold %.5f",
                         self.workingThreshold))
        }
    }

    /// Override the VAD silence threshold for the next utterance.
    ///
    /// Phase 24c: called by `DexterClient` when a `VadHint` arrives from Rust during
    /// TTS streaming.  The override is consumed on the next falling edge and then
    /// cleared — subsequent utterances return to `Constants.VAD_SILENCE_FRAMES`.
    ///
    /// Example: 8 frames (256ms) for yes/no questions saves 384ms vs the default
    /// 20 frames (640ms) because the operator's answer is short and confident.
    ///
    /// Thread-safe: dispatches to `callbackQueue` so the assignment is serialised
    /// with `processVAD()` which reads `silenceFrameOverride` on the same queue.
    func applyVadHint(_ frames: UInt32) {
        callbackQueue.async { self.silenceFrameOverride = Int(frames) }
    }

    /// Stop the capture session and release resources.
    func stop() {
        session?.stopRunning()
        session     = nil
        audioOutput = nil
    }

    // MARK: - AVCaptureAudioDataOutputSampleBufferDelegate

    func captureOutput(
        _ output: AVCaptureOutput,
        didOutput sampleBuffer: CMSampleBuffer,
        from connection: AVCaptureConnection
    ) {
        guard let blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer) else { return }
        let length = CMBlockBufferGetDataLength(blockBuffer)
        guard length > 0 else { return }

        // Copy the block buffer bytes into a Data value so we own the lifetime.
        var data = Data(count: length)
        let copyResult = data.withUnsafeMutableBytes { ptr -> OSStatus in
            guard let bytes = ptr.baseAddress else { return -1 }
            return CMBlockBufferCopyDataBytes(blockBuffer, atOffset: 0,
                                             dataLength: length, destination: bytes)
        }
        guard copyResult == noErr else { return }

        let rms = computeRMS(data)

        // Log the first frame to confirm audio is arriving, then one summary after 60 frames.
        diagFrameCount += 1
        if diagFrameCount == 1 {
            print(String(format: "[VoiceCapture] First frame received — rms=%.5f threshold=%.5f (calibrating…)",
                         rms, workingThreshold))
        } else if diagFrameCount == 60 {
            print(String(format: "[VoiceCapture] Audio running normally (60 frames) — rms=%.5f",
                         rms))
        }

        processVAD(data: data, rms: rms)
    }

    // MARK: - VAD

    /// Update the VAD state machine for one buffer.
    /// Called exclusively from `callbackQueue`.
    private func processVAD(data: Data, rms: Float) {

        // ── Ambient calibration ─────────────────────────────────────────────
        // Collect RMS samples for the first N frames (while everything is quiet
        // at startup) and raise workingThreshold if the measured ambient exceeds
        // the compile-time floor.  Calibration is read-only after it completes.
        if ambientSamples.count < Self.ambientCalibrationFrames {
            ambientSamples.append(rms)
            if ambientSamples.count == Self.ambientCalibrationFrames {
                let avgAmbient = ambientSamples.reduce(0, +) / Float(Self.ambientCalibrationFrames)
                let adaptive   = avgAmbient * Self.ambientMultiplier
                // Clamp to [floor, cap]: never go below the compile-time floor (avoids
                // silent-room over-sensitivity) and never above maxThresholdCap (avoids
                // swallowing word-initial consonants in a noisy calibration environment).
                workingThreshold = min(Self.maxThresholdCap,
                                       max(Constants.VAD_ENERGY_THRESHOLD, adaptive))
                print(String(format: "[VoiceCapture] Calibration done — ambient_avg=%.5f, adaptive=%.5f, threshold=%.5f (cap=%.3f floor=%.3f)",
                             avgAmbient, adaptive, workingThreshold,
                             Self.maxThresholdCap, Constants.VAD_ENERGY_THRESHOLD))
            }
        }

        // ── Pre-roll circular buffer ────────────────────────────────────────
        // Always keep the last preRollFrames audio frames regardless of VAD state.
        // On the rising edge these are prepended to utteranceBuffer so whisper
        // receives audio that begins before the threshold was crossed — recovering
        // the leading consonant (e.g. the "Wh" in "What up") that triggered the edge.
        preRollBuffer.append(data)
        if preRollBuffer.count > Self.preRollFrames {
            preRollBuffer.removeFirst()
        }

        switch vadState {
        case .silent:
            // Diagnostic: while armed, log RMS every 16 frames so we can see
            // if speech is reaching the threshold.  Capped at 120 frames (≈4 s)
            // to avoid flooding the console on a long silent pause.
            if isArmed {
                activeLogFrames += 1
                if activeLogFrames <= 120, activeLogFrames % 16 == 0 {
                    print(String(format: "[VoiceCapture] Waiting… rms=%.5f threshold=%.5f (frame %d)",
                                 rms, workingThreshold, activeLogFrames))
                }
            }

            if rms > workingThreshold {
                onsetFrames += 1
                if onsetFrames >= Constants.VAD_ONSET_FRAMES {
                    // Rising edge — speech started.
                    // Seed utteranceBuffer with the pre-roll so whisper sees the
                    // audio that preceded the threshold crossing (the leading consonant).
                    vadState        = .active
                    onsetFrames     = 0
                    silenceFrames   = 0
                    utteranceBuffer = preRollBuffer   // ~192 ms of pre-onset audio
                    // Only arm delivery if the operator pressed hotkey first.
                    // isArmed is consumed here (one rising edge per hotkey press).
                    let wasArmed = isArmed
                    if isArmed {
                        isActive = true
                        isArmed  = false
                    }
                    // Only log when the hotkey was pressed — unarmed rising edges are
                    // ambient noise correctly gated out; logging them is pure noise.
                    if wasArmed {
                        print(String(format: "[VoiceCapture] Rising edge — speech detected rms=%.5f isActive=%d",
                                     rms, isActive ? 1 : 0))
                    }
                }
            } else {
                onsetFrames = 0
            }

        case .active:
            // Always buffer the frame first, then check for silence.
            // This avoids cutting off the last audible frame at the trailing edge.
            utteranceBuffer.append(data)

            if rms < workingThreshold {
                silenceFrames += 1
                let threshold = silenceFrameOverride ?? Constants.VAD_SILENCE_FRAMES
                if silenceFrames >= threshold {
                    // Falling edge — utterance complete.
                    // Consume and clear the one-shot override so the next utterance
                    // falls back to Constants.VAD_SILENCE_FRAMES automatically.
                    vadState             = .silent
                    silenceFrameOverride = nil
                    silenceFrames        = 0
                    let utterance = utteranceBuffer
                    utteranceBuffer.removeAll()
                    // Gate: only deliver the utterance when the operator pressed hotkey.
                    // Reset isActive so the next utterance requires another hotkey press
                    // (one press = one utterance = one inference round-trip).
                    if isActive {
                        isActive = false
                        print(String(format: "[VoiceCapture] Falling edge — delivering %d chunks to STT",
                                     utterance.count))
                        onUtteranceComplete?(utterance)
                    }
                    // Unarmed falling edges (ambient noise cycle) are silently discarded —
                    // no log: they fire constantly from TV/HVAC and produce zero signal.
                }
            } else {
                silenceFrames = 0
            }
        }
    }

    // MARK: - RMS energy

    /// Compute RMS energy of a raw int16 PCM buffer, normalised to [0, 1].
    ///
    /// Normalisation: divide each int16 sample by 32768.0 (Int16.max + 1) so the
    /// result is independent of bit depth and directly comparable to the threshold.
    private func computeRMS(_ data: Data) -> Float {
        let frameCount = data.count / 2   // 2 bytes per int16 sample
        guard frameCount > 0 else { return 0 }

        var sumSquares: Float = 0
        data.withUnsafeBytes { raw in
            guard let samples = raw.baseAddress?.assumingMemoryBound(to: Int16.self) else { return }
            for i in 0..<frameCount {
                let s = Float(samples[i]) / 32768.0
                sumSquares += s * s
            }
        }
        return (sumSquares / Float(frameCount)).squareRoot()
    }
}
