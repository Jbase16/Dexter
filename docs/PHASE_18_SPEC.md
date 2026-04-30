# Phase 18 — Configurable Hotkey + Proactive Exclusions + Proactive IDLE Timing
## Spec version 1.1 — Session 018, 2026-03-13

> **Status:** CURRENT PHASE.
> This document is the authoritative implementation guide for Phase 18.
> All architectural decisions are locked. Implement exactly as written.
>
> **Spec version history:**
> v1.0 — Initial Phase 18 plan (configurable hotkey + proactive exclusions)
> v1.1 — Added Issue 3 fix (proactive IDLE timing) identified in Phase 17 retroactive review

---

## 1. What Phase 18 Delivers

Three deliverables: two operator-control-surface features and one correctness fix.

| Deliverable | Origin | Why Now |
|-------------|--------|---------|
| **Configurable hotkey** — `[hotkey]` section in `config.toml`; hotkey params pushed to Swift at session start via new `ConfigSync` proto message | Phase 16 deferred ("Phase 17+") | `BehaviorConfig` pattern from Phase 17 makes a new config section trivial. Proto already being updated this phase. |
| **Per-bundle proactive exclusions** — `proactive_excluded_bundles` in `[behavior]`; Gate 6 in `ProactiveEngine::should_fire()` | Phase 17 deferred ("Phase 18+ could add exclusion list") | Direct follow-on to Phase 17's `ProactiveEngine`. One field, one gate, three tests. |
| **Proactive IDLE timing fix** — proactive entity stays in THINKING state until audio *playback* finishes (not just until TTS synthesis finishes) | Phase 17 retroactive review (Issue 3) | Proto already being updated; `AudioPlayer` already has the exact completion-callback hook needed. Surgical fix with no scope creep. |

**Phase 17 retroactive fixes already applied (not in Phase 18 scope):**
- Issue 1 (`[SILENT]` refunding rate-limit slot): fixed — `collect_generation()` now returns
  `Option<String>` distinguishing inference errors from model silence; `undo_fire()` added
  to `ProactiveEngine`; 1 new test added (193 Rust tests total).
- Issue 2 (no timeout on `collect_generation()`): fixed — `tokio::time::timeout(30s)` wrapper
  added; timeout returns `Ok(None)` (same as inference error — burns the slot).

**What this does NOT include:**
- Proactive on significant AX element changes — deferred to Phase 19+ (requires a
  statistical significance classifier; architecturally uncertain).
- Wake phrase detection — deferred (Phase 19+).
- Regular-response SPEAKING→IDLE timing fix — the same synthesis-vs-playback timing issue
  exists in `generate_and_stream()` but is less visually jarring (longer audio = smaller
  relative mismatch). Deferred to Phase 19 where the full `is_final` mechanism can be
  extended to all TTS flows.

**Test count target:** 200 Rust passing (currently 193). 7 new tests.

---

## 2. What Already Exists (Do Not Rebuild)

| Component | Phase | Relevance to Phase 18 |
|-----------|-------|-----------------------|
| `BehaviorConfig` + `[behavior]` TOML section | 17 | Pattern for `HotkeyConfig` + `[hotkey]`. Same serde-defaults idiom. |
| `ProactiveEngine::should_fire()` + `undo_fire()` | 17/17-fix | Gate 6 slots in after Gate 3. `undo_fire()` already present from Issue 1 fix. |
| `handle_system_event()` `Connected` arm | 6/16 | Phase 18 adds `ConfigSync` emission here. |
| `ServerEvent` oneof (fields 2–5 used) | 3 | Field 6 for `ConfigSync`, field 7 reserved; `AudioResponse` adds field 3. |
| `EventBridge.isHotkeyEvent(_:)` | 16 | Parameterized via stored properties (Phase 18). |
| `DexterClient.onResponse` handler | 12/16 | Add `.configSync`, update `.audioResponse` with `isFinal`. |
| `AudioPlayer.enqueue(data:sequenceNumber:)` | 13 | Gains `isFinal:` parameter and `onPlaybackFinished` callback (Phase 18). |
| Buffer completion handler in `flushReadyBuffers()` | 13 | `pendingBufferCount == 0 && sequenceQueue.isEmpty` condition already present — extend it to fire `onPlaybackFinished` when armed. |
| `make proto` target | 3 | Regenerates Swift + Rust from updated proto. Phase 18 adds `HotkeyConfig`, `ConfigSync`, `is_final` on `AudioResponse`, two new `SystemEventType` values. |

---

## 3. Architecture

### 3.1 Configurable hotkey — data flow

```
~/.dexter/config.toml
  [hotkey]
  key_code = 49        ← macOS kVK_Space (default)
  ctrl     = true
  shift    = true
  cmd      = false
  option   = false
         │
         ▼
  Rust config.rs: HotkeyConfig (loaded at startup, part of Arc<DexterConfig>)
         │
         ▼  (on CONNECTED system event)
  Rust orchestrator.rs: handle_system_event(Connected)
    → sends ConfigSync { hotkey: HotkeyConfig { ... } } as ServerEvent
         │
         ▼  (gRPC ServerEvent stream)
  Swift DexterClient.onResponse → .configSync(let cs)
    → bridge.updateHotkeyConfig(cs.hotkey)    [MainActor]
         │
         ▼
  Swift EventBridge: stored properties updated
    hotkeyKeyCode, hotkeyRequiresCtrl, hotkeyRequiresShift, etc.
         │
         ▼
  hotkeyTapCallback → isHotkeyEvent(_:) reads stored properties
```

**Key decisions:**
- **Push, not pull.** Rust is the config owner. Swift does not read `~/.dexter/config.toml`
  directly — that would split config ownership and duplicate TOML parsing.
- **5 flat bool fields, not a combo string.** `ctrl = true / shift = true` maps directly
  onto CGEvent flag checks with zero parsing. No edge cases around key name aliases or
  ordering.
- **ConfigSync is ephemeral.** Not stored in conversation history or session state.

### 3.2 Per-bundle proactive exclusions — Gate 6

`ProactiveEngine::should_fire()` gains a sixth gate inserted after Gate 3 (app_name
required) and before Gate 4 (startup grace):

```
Gate 1: enabled == true
Gate 2: !snapshot.is_screen_locked
Gate 3: snapshot.app_name.is_some()
Gate 6: snapshot.app_bundle_id ∉ excluded_bundles  ← NEW
Gate 4: session age ≥ startup_grace_secs
Gate 5: time since last fire ≥ min_interval_secs
```

**Why bundle ID, not app name?** Bundle IDs are locale-invariant. App names are
locale-dependent and user-editable. `com.agilebits.onepassword-osx` is unambiguous;
`1Password` could vary.

**Why Gate 6 position?** Gate 3 already confirms a non-trivial context exists. Gate 6
is a cheap list lookup that short-circuits before the `elapsed()` syscalls in Gates 4/5.

### 3.3 Proactive IDLE timing fix — the synthesis vs. playback gap

**The bug (Phase 17 Issue 3):**

```
Rust flow:
  1. THINKING state
  2. TTS synthesis → Python worker sends TTS_DONE
  3. handle.await (Tokio task) completes — all PCM sent to gRPC stream
  4. Rust sends EntityState::Idle   ← happens here

Swift reality:
  1. Receives PCM chunks → buffers in AVAudioPlayerNode queue
  2. Starts playing                  ← might still be here when Rust sends IDLE
  3. Finishes playing audio
```

Rust sends IDLE after **synthesis** completion. Swift transitions the entity while
audio is **still playing**. For a one-sentence observation, the gap is the full
playback duration (typically 2–4 seconds on the FAST model).

**The fix:**

A sentinel `AudioResponse { data: [], is_final: true }` is sent from Rust after all
PCM chunks. This arms a callback in Swift's `AudioPlayer`. When the last scheduled
buffer finishes playing (the exact moment `pendingBufferCount == 0 && sequenceQueue.isEmpty`
inside the existing completion handler), `AudioPlayer.onPlaybackFinished()` fires.
`DexterClient` sends `SYSTEM_EVENT_TYPE_AUDIO_PLAYBACK_COMPLETE` back to Rust.
Rust handles it → `EntityState::Idle`.

```
Rust (do_proactive_response):
  1. THINKING
  2. TTS synthesis → PCM chunks sent as AudioResponse
  3. Sentinel: AudioResponse { data: [], is_final: true } sent
  4. No IDLE sent from Rust

Swift (AudioPlayer):
  5. Receives PCM chunks → schedules buffers
  6. Receives is_final sentinel → sets awaitingFinalCallback = true
  7. Last buffer finishes playing (pendingBufferCount == 0 && awaitingFinalCallback)
  8. onPlaybackFinished() fires → DexterClient sends AUDIO_PLAYBACK_COMPLETE

Rust (handle_system_event):
  9. AudioPlaybackComplete → EntityState::Idle
```

**Why a zero-byte sentinel rather than marking the last PCM chunk?** The TTS loop
reads frames from the Python worker one at a time and forwards them immediately. We
don't know a frame is the "last" until `TTS_DONE` arrives from the worker — after the
last PCM frame has already been sent. A post-loop sentinel (empty data, `is_final =
true`) accurately marks the end of the audio stream without requiring lookahead.

**Why this fix is scoped to proactive only in Phase 18:** The regular streaming path
(SentenceSplitter → per-sentence TTS) would require marking `is_final` on the last
sentence of a full response — that involves the sentence boundary detection layer. The
gap is also less visible for multi-sentence responses (the entity is in SPEAKING state
which has shorter duration relative to audio). Deferred to Phase 19.

**No-TTS fallback:** When `voice.is_tts_available()` is false, no audio is sent. Rust
sends `EntityState::Idle` directly — there is no audio playback to wait for.

### 3.4 Proto changes — all four updates in one `make proto` pass

```protobuf
// AudioResponse: add is_final field (Phase 18 — proactive IDLE timing fix)
message AudioResponse {
  bytes  data            = 1;
  uint32 sequence_number = 2;
  bool   is_final        = 3;  // Phase 18: final audio sentinel for proactive TTS
                                // Rust sets true with empty data after all PCM chunks.
                                // Swift uses this to arm the playback-complete callback.
}

// New messages for configurable hotkey
message HotkeyConfig {
  uint32 key_code = 1;  // macOS virtual key code. kVK_Space = 49.
  bool   ctrl     = 2;
  bool   shift    = 3;
  bool   cmd      = 4;
  bool   option   = 5;
}

message ConfigSync {
  HotkeyConfig hotkey = 1;
}

// ServerEvent: add config_sync at field 6
message ServerEvent {
  string trace_id = 1;
  oneof event {
    TextResponse      text_response  = 2;
    EntityStateChange entity_state   = 3;
    AudioResponse     audio_response = 4;
    ActionRequest     action_request = 5;
    ConfigSync        config_sync    = 6;  // Phase 18: session config pushed on open
  }
}

// SystemEventType: two new values
enum SystemEventType {
  // ... existing values 0–7 unchanged ...
  SYSTEM_EVENT_TYPE_AUDIO_PLAYBACK_COMPLETE = 8;  // Phase 18: Swift signals audio done
                                                   // payload: "{}" (no data needed)
}
```

---

## 4. Files Changed

| File | Change |
|------|--------|
| `src/shared/proto/dexter.proto` | `AudioResponse.is_final`, `HotkeyConfig`, `ConfigSync`, `ServerEvent.config_sync = 6`, `SystemEventType.AUDIO_PLAYBACK_COMPLETE = 8` |
| `src/rust-core/src/config.rs` | Add `HotkeyConfig` struct with serde defaults; `hotkey: HotkeyConfig` in `DexterConfig`; add `proactive_excluded_bundles: Vec<String>` to `BehaviorConfig`; 2 new tests |
| `src/rust-core/src/proactive/engine.rs` | Add Gate 6 (exclusion list) to `should_fire()`; `excluded_bundles` field in struct; update `new()` and `new_backdated()`; 3 new tests |
| `src/rust-core/src/orchestrator.rs` | `Connected` arm sends `ConfigSync`; `AudioPlaybackComplete` arm sends IDLE; `do_proactive_response` sends `is_final` sentinel, skips direct IDLE; no-TTS case keeps direct IDLE; 2 new tests |
| `src/swift/Sources/Dexter/Bridge/EventBridge.swift` | Replace hardcoded constants in `isHotkeyEvent(_:)` with stored properties; add `updateHotkeyConfig(_:)` |
| `src/swift/Sources/Dexter/Bridge/DexterClient.swift` | Handle `.configSync`; update `.audioResponse` to pass `audio.isFinal`; set `audioPlayer.onPlaybackFinished` callback |
| `src/swift/Sources/Dexter/Voice/AudioPlayer.swift` | Add `onPlaybackFinished: (@Sendable () -> Void)?`; add `awaitingFinalCallback: Bool`; update `enqueue` signature with `isFinal:` param; arm callback in completion handler; reset in `stop()` |
| `docs/SESSION_STATE.json` | Phase 18 complete, test counts updated to 200 |

---

## 5. New Tests (7 total)

### config.rs (2 new tests)

| Test | Validates |
|------|-----------|
| `hotkey_config_defaults_are_correct` | Default `HotkeyConfig`: key_code=49, ctrl=true, shift=true, cmd=false, option=false |
| `hotkey_config_partial_override_preserves_defaults` | `[hotkey]\ncmd = true` overrides cmd; key_code/ctrl/shift/option stay at defaults |

### proactive/engine.rs (3 new tests)

| Test | Validates |
|------|-----------|
| `proactive_engine_excluded_bundle_does_not_fire` | Gate 6: bundle in exclusion list → `should_fire()` false |
| `proactive_engine_non_excluded_bundle_fires` | Gate 6: different bundle → all gates clear → fires |
| `proactive_engine_empty_exclusion_list_fires_all_bundles` | Empty list (default) → no apps blocked (Phase 17 parity) |

### orchestrator.rs (2 new tests)

| Test | Validates |
|------|-----------|
| `handle_connected_sends_config_sync_after_idle` | `CONNECTED` → first event is `EntityState(Idle)`, second is `ConfigSync` with correct default hotkey values |
| `handle_audio_playback_complete_transitions_to_idle` | `AUDIO_PLAYBACK_COMPLETE` → orchestrator sends `EntityState(Idle)` |

---

## 6. Implementation Guide

Implement in exactly this order. Run `cargo test` after each Rust step.

---

### Step 1: `HotkeyConfig` + `proactive_excluded_bundles` in `config.rs`

**File:** `src/rust-core/src/config.rs`

Add `HotkeyConfig` after `BehaviorConfig`:

```rust
/// Global activation hotkey parameters.
///
/// Defaults to Ctrl+Shift+Space (keyCode 49) — identical to the Phase 16
/// hardcoded value. Operators who add no `[hotkey]` section get unchanged behavior.
/// Changes take effect at next session start — pushed to Swift via `ConfigSync`.
#[derive(Debug, Deserialize, Clone)]
pub struct HotkeyConfig {
    /// macOS virtual key code. Default 49 = kVK_Space.
    #[serde(default = "default_hotkey_key_code")]
    pub key_code: u32,
    #[serde(default = "default_hotkey_ctrl")]
    pub ctrl: bool,
    #[serde(default = "default_hotkey_shift")]
    pub shift: bool,
    #[serde(default = "default_hotkey_cmd")]
    pub cmd: bool,
    #[serde(default = "default_hotkey_option")]
    pub option: bool,
}

fn default_hotkey_key_code() -> u32  { 49 }
fn default_hotkey_ctrl()     -> bool { true }
fn default_hotkey_shift()    -> bool { true }
fn default_hotkey_cmd()      -> bool { false }
fn default_hotkey_option()   -> bool { false }

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key_code: default_hotkey_key_code(),
            ctrl:     default_hotkey_ctrl(),
            shift:    default_hotkey_shift(),
            cmd:      default_hotkey_cmd(),
            option:   default_hotkey_option(),
        }
    }
}
```

Add `hotkey: HotkeyConfig` to `DexterConfig` (with `#[serde(default)]`) and to
`DexterConfig::default()`.

Add `proactive_excluded_bundles: Vec<String>` to the existing `BehaviorConfig` with
`#[serde(default)]` (deserializes to `vec![]` when absent). Update `BehaviorConfig::default()`
to include `proactive_excluded_bundles: vec![]`.

**Add 2 unit tests:**

```rust
#[test]
fn hotkey_config_defaults_are_correct() {
    let cfg = HotkeyConfig::default();
    assert_eq!(cfg.key_code, 49);
    assert!(cfg.ctrl);
    assert!(cfg.shift);
    assert!(!cfg.cmd);
    assert!(!cfg.option);
}

#[test]
fn hotkey_config_partial_override_preserves_defaults() {
    let toml = "[hotkey]\ncmd = true\n";
    let cfg: DexterConfig = toml::from_str(toml).expect("valid TOML");
    assert_eq!(cfg.hotkey.key_code, 49);
    assert!(cfg.hotkey.ctrl);
    assert!(cfg.hotkey.shift);
    assert!(cfg.hotkey.cmd,  "cmd was overridden to true");
    assert!(!cfg.hotkey.option);
}
```

**After Step 1: `cargo test` → 195 passing, 0 warnings.**

---

### Step 2: Gate 6 in `ProactiveEngine`

**File:** `src/rust-core/src/proactive/engine.rs`

Add `excluded_bundles: Vec<String>` to the struct. Update `new()` and `new_backdated()`
to read `cfg.proactive_excluded_bundles.clone()`. Insert Gate 6 in `should_fire()` after
Gate 3 (app_name check), before Gate 4 (startup grace):

```rust
// Gate 6: [Phase 18] per-bundle exclusion list.
//
// Bundle IDs are locale-invariant stable identifiers; app names are not.
// If app_bundle_id is absent (should not occur after Gate 3, but be defensive),
// allow through — we cannot match against an unknown bundle.
if let Some(ref bundle_id) = snapshot.app_bundle_id {
    if self.excluded_bundles.iter().any(|ex| ex == bundle_id) {
        return false;
    }
}
```

**Add 3 unit tests** (see §5).

**After Step 2: `cargo test` → 198 passing, 0 warnings.**

---

### Step 3: Proto updates

**File:** `src/shared/proto/dexter.proto`

Apply all four proto changes in a single edit:

1. Add `bool is_final = 3` to `AudioResponse`:
   ```protobuf
   message AudioResponse {
     bytes  data            = 1;
     uint32 sequence_number = 2;
     bool   is_final        = 3;  // Phase 18: final sentinel for proactive TTS
   }
   ```

2. Add `SYSTEM_EVENT_TYPE_AUDIO_PLAYBACK_COMPLETE = 8` to `SystemEventType`:
   ```protobuf
   SYSTEM_EVENT_TYPE_AUDIO_PLAYBACK_COMPLETE = 8;  // Phase 18: Swift signals TTS done
   ```

3. Add `HotkeyConfig` and `ConfigSync` messages (after `TranscriptChunk`):
   ```protobuf
   message HotkeyConfig {
     uint32 key_code = 1;
     bool   ctrl     = 2;
     bool   shift    = 3;
     bool   cmd      = 4;
     bool   option   = 5;
   }

   message ConfigSync {
     HotkeyConfig hotkey = 1;
   }
   ```

4. Add `config_sync = 6` to `ServerEvent.oneof`.

Regenerate:
```bash
make proto
```

After `make proto`, Rust will emit a non-exhaustive match warning/error for
`SystemEventType::AudioPlaybackComplete` in `handle_system_event` — add the arm in
Step 4. Swift will require a new case in any exhaustive switch on `SystemEventType` —
add it in Step 7.

---

### Step 4: Orchestrator — `ConfigSync` + `AudioPlaybackComplete` + proactive IDLE

**File:** `src/rust-core/src/orchestrator.rs`

#### 4a. `Connected` arm — send `ConfigSync`

After the existing `send_state(EntityState::Idle, &trace_id).await?` in the `Connected`
arm, send the `ConfigSync`:

```rust
// Phase 18: push session config so Swift can configure OS-level observers.
let hk = self.cfg.hotkey.clone();
let sync_event = ServerEvent {
    trace_id: trace_id.clone(),
    event: Some(server_event::Event::ConfigSync(
        crate::ipc::proto::ConfigSync {
            hotkey: Some(crate::ipc::proto::HotkeyConfig {
                key_code: hk.key_code,
                ctrl:     hk.ctrl,
                shift:    hk.shift,
                cmd:      hk.cmd,
                option:   hk.option,
            }),
        }
    )),
};
self.tx.send(Ok(sync_event))
    .map_err(|_| anyhow::anyhow!("ConfigSync send failed — receiver dropped"))?;
debug!(
    session   = %self.session_id,
    key_code  = hk.key_code,
    ctrl      = hk.ctrl,
    shift     = hk.shift,
    "ConfigSync pushed to Swift shell"
);
```

#### 4b. New `AudioPlaybackComplete` arm

Add after `ScreenUnlocked`:

```rust
// Phase 18: Swift signals that proactive TTS audio has finished playing.
// Transition entity to IDLE now that the operator has heard the observation.
SystemEventType::AudioPlaybackComplete => {
    info!(
        session  = %self.session_id,
        trace_id = %trace_id,
        "Proactive TTS playback complete — transitioning to IDLE"
    );
    self.send_state(EntityState::Idle, &trace_id).await?;
}
```

#### 4c. `do_proactive_response` — send `is_final` sentinel, skip direct IDLE

Replace the TTS block's closing `send_state(EntityState::Idle)` with the sentinel
approach. The full updated TTS section:

```rust
if self.voice.is_tts_available() {
    let tts_arc    = self.voice.tts_arc();
    let text_bytes = response.trim().as_bytes().to_vec();
    let session_tx = self.tx.clone();
    let trace_id_clone = trace_id.to_string();

    let handle = tokio::spawn(async move {
        use crate::ipc::proto::{server_event, AudioResponse};
        let mut guard = tts_arc.lock().await;
        if let Some(client) = guard.as_mut() {
            if client.write_frame(msg::TEXT_INPUT, &text_bytes).await.is_ok() {
                let mut seq = 0u32;
                loop {
                    match client.read_frame().await {
                        Ok(Some((msg::TTS_AUDIO, pcm))) => {
                            let evt = ServerEvent {
                                trace_id: String::new(),
                                event: Some(server_event::Event::AudioResponse(
                                    AudioResponse {
                                        data:            pcm,
                                        sequence_number: seq,
                                        is_final:        false,
                                    },
                                )),
                            };
                            let _ = session_tx.send(Ok(evt)).await;
                            seq += 1;
                        }
                        Ok(Some((msg::TTS_DONE, _))) => {
                            // Phase 18: send the is_final sentinel (empty data).
                            // Swift arms its playback-complete callback on receipt.
                            // When the last scheduled buffer finishes playing, Swift
                            // sends AUDIO_PLAYBACK_COMPLETE, and the orchestrator
                            // transitions to IDLE then.
                            let sentinel = ServerEvent {
                                trace_id: trace_id_clone,
                                event: Some(server_event::Event::AudioResponse(
                                    AudioResponse {
                                        data:            vec![],
                                        sequence_number: seq,
                                        is_final:        true,
                                    },
                                )),
                            };
                            let _ = session_tx.send(Ok(sentinel)).await;
                            break;
                        }
                        Ok(Some(_))  => {}   // discard unexpected frames
                        _            => break,
                    }
                }
            }
        }
    });
    let _ = handle.await;
    // Do NOT send EntityState::Idle here — Swift sends AUDIO_PLAYBACK_COMPLETE
    // after the last buffer finishes playing, which the orchestrator handles.
} else {
    // No TTS — no audio will play. Transition to IDLE directly.
    self.send_state(EntityState::Idle, trace_id).await?;
}
```

Remove the `self.send_state(EntityState::Idle, trace_id).await?;` line that currently
appears unconditionally after the TTS block.

**Add 2 unit tests:**

```rust
#[tokio::test]
async fn handle_connected_sends_config_sync_after_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, mut rx) = make_orchestrator(tmp.path());
    while rx.try_recv().is_ok() {}

    let evt = SystemEvent {
        r#type:  crate::ipc::proto::SystemEventType::Connected.into(),
        payload: "{}".to_string(),
    };
    orch.handle_system_event(evt, new_trace()).await.unwrap();

    // First: EntityState(Idle)
    let first = rx.try_recv().unwrap().unwrap();
    assert!(matches!(
        first.event,
        Some(crate::ipc::proto::server_event::Event::EntityState(_))
    ), "first event must be EntityStateChange");

    // Second: ConfigSync with default hotkey values
    let second = rx.try_recv().unwrap().unwrap();
    match second.event {
        Some(crate::ipc::proto::server_event::Event::ConfigSync(ref cs)) => {
            let hk = cs.hotkey.as_ref().expect("ConfigSync must carry HotkeyConfig");
            assert_eq!(hk.key_code, 49);
            assert!(hk.ctrl);
            assert!(hk.shift);
            assert!(!hk.cmd);
            assert!(!hk.option);
        }
        other => panic!("Expected ConfigSync, got {:?}", other),
    }

    assert!(rx.try_recv().is_err(), "CONNECTED must produce exactly two events");
}

#[tokio::test]
async fn handle_audio_playback_complete_transitions_to_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut orch, mut rx) = make_orchestrator(tmp.path());
    while rx.try_recv().is_ok() {}

    let evt = SystemEvent {
        r#type:  crate::ipc::proto::SystemEventType::AudioPlaybackComplete.into(),
        payload: "{}".to_string(),
    };
    orch.handle_system_event(evt, new_trace()).await.unwrap();

    let event = rx.try_recv().unwrap().unwrap();
    match event.event {
        Some(crate::ipc::proto::server_event::Event::EntityState(ref change)) => {
            assert_eq!(
                change.state,
                crate::ipc::proto::EntityState::Idle as i32,
                "AUDIO_PLAYBACK_COMPLETE must transition entity to IDLE"
            );
        }
        other => panic!("Expected EntityStateChange(Idle), got {:?}", other),
    }
}
```

**After Step 4: `cargo test` → 200 passing, 0 warnings.**

---

### Step 5: `AudioPlayer.swift` — playback-complete callback

**File:** `src/swift/Sources/Dexter/Voice/AudioPlayer.swift`

#### 5a. Add stored properties

After `private var nextExpectedSeq: UInt32 = 0`:

```swift
/// Callback fired on `self.queue` when the last scheduled buffer in a proactive
/// TTS sequence finishes playing. Set by `DexterClient` to send
/// `AUDIO_PLAYBACK_COMPLETE` back to Rust. Swift 6: `@Sendable` required because
/// this closure will be called from `self.queue` (a serial DispatchQueue), which
/// is not actor-isolated.
var onPlaybackFinished: (@Sendable () -> Void)?

/// Set to true when an `AudioResponse` with `is_final = true` is enqueued.
/// When `pendingBufferCount` reaches zero while this flag is set, `onPlaybackFinished`
/// is called and the flag is reset. Protected by `self.queue`.
private var awaitingFinalCallback: Bool = false
```

#### 5b. Update `enqueue` signature

Replace `func enqueue(data: Data, sequenceNumber: UInt32)` with:

```swift
/// Enqueue a PCM chunk for sequenced playback.
///
/// - Parameters:
///   - data:           Raw PCM bytes (int16, 16kHz, mono). May be empty for the
///                     `is_final` sentinel — empty buffers are not scheduled.
///   - sequenceNumber: Position in the ordered stream; out-of-order chunks are
///                     held until the gap is filled.
///   - isFinal:        When `true`, arms `onPlaybackFinished`. When all previously
///                     scheduled buffers have played (or immediately if none were
///                     scheduled), `onPlaybackFinished` fires.
func enqueue(data: Data, sequenceNumber: UInt32, isFinal: Bool = false) {
    queue.async { [self] in
        if isFinal {
            awaitingFinalCallback = true
            // Empty sentinel: don't add to queue, but check if already drained.
            if !data.isEmpty {
                sequenceQueue.append(Item(data: data, sequenceNumber: sequenceNumber))
            }
        } else {
            sequenceQueue.append(Item(data: data, sequenceNumber: sequenceNumber))
        }
        flushReadyBuffers()
        // Fire immediately if the queue is already empty when is_final arrives
        // (all buffers played before the sentinel was received).
        checkFinalCallback()
    }
}
```

Add the private helper:

```swift
/// Fire `onPlaybackFinished` if armed and the queue is fully drained.
/// Must only be called from `self.queue`.
private func checkFinalCallback() {
    guard awaitingFinalCallback,
          pendingBufferCount == 0,
          sequenceQueue.isEmpty else { return }
    awaitingFinalCallback = false
    onPlaybackFinished?()
}
```

#### 5c. Call `checkFinalCallback()` in the buffer completion handler

In `flushReadyBuffers()`, the existing completion handler body is:

```swift
this.pendingBufferCount -= 1
if this.pendingBufferCount == 0 && this.sequenceQueue.isEmpty {
    this._isPlaying = false
}
this.flushReadyBuffers()
```

Add `this.checkFinalCallback()` after the `_isPlaying = false` block:

```swift
this.pendingBufferCount -= 1
if this.pendingBufferCount == 0 && this.sequenceQueue.isEmpty {
    this._isPlaying = false
}
this.flushReadyBuffers()
this.checkFinalCallback()   // Phase 18: fire if proactive TTS sequence is complete
```

#### 5d. Reset `awaitingFinalCallback` in `stop()`

In `stop()`, add after `_isPlaying = false`:

```swift
awaitingFinalCallback = false   // discard any pending callback if barge-in fires
```

---

### Step 6: `DexterClient.swift` — wire `ConfigSync` + `is_final` + playback callback

**File:** `src/swift/Sources/Dexter/Bridge/DexterClient.swift`

#### 6a. Set `onPlaybackFinished` at session start

After `audioPlayer.start()`, set the playback-complete callback. This must be done
once per session (before any audio arrives). The `weak self` / `Task` pattern is the
established Swift 6 approach for calling actor-isolated methods from a DispatchQueue:

```swift
audioPlayer.onPlaybackFinished = { [weak self, sessionID] in
    guard let client = self else { return }
    Task {
        let event = Dexter_V1_ClientEvent.with {
            $0.traceID   = UUID().uuidString
            $0.sessionID = sessionID
            $0.systemEvent = Dexter_V1_SystemEvent.with {
                $0.type    = .audioPlaybackComplete
                $0.payload = "{}"
            }
        }
        await client.send(event)
    }
}
```

`audioPlayer` is a `let` on `DexterClient`, so the capture is straightforward. `self`
is the `DexterClient` actor (which is `Sendable`). `sessionID` is a `String`.

#### 6b. Update `audioResponse` case

Replace the existing `.audioResponse(let audio)` case:

```swift
case .audioResponse(let audio):
    // Route PCM chunks to AVAudioEngine for sequenced playback.
    // isFinal arms the playback-complete callback in AudioPlayer (Phase 18).
    audioPlayer.enqueue(
        data:           Data(audio.data),
        sequenceNumber: audio.sequenceNumber,
        isFinal:        audio.isFinal
    )
```

#### 6c. Add `.configSync` case

```swift
case .configSync(let cs):
    // Rust pushed session configuration — update EventBridge hotkey parameters.
    await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
        Task { @MainActor in
            bridge.updateHotkeyConfig(cs.hotkey)
            continuation.resume()
        }
    }
```

`bridge` is the `EventBridge` instance in scope in `runSession`. Capture it from the
`onResponse` closure's capture list (add `bridge` alongside `window` and `sessionID`).

#### 6d. `AudioPlaybackComplete` exhaustiveness (if needed)

After `make proto`, if any Swift `switch` on `SystemEventType` becomes non-exhaustive,
add:

```swift
case .audioPlaybackComplete:
    break  // handled via the onPlaybackFinished → send path; no direct client action
```

---

### Step 7: `EventBridge.swift` — parameterized `isHotkeyEvent(_:)`

**File:** `src/swift/Sources/Dexter/Bridge/EventBridge.swift`

#### 7a. Add stored properties (after `hotkeyRunLoopSource`)

```swift
// Hotkey detection parameters — initialized to Phase 16 hardcoded defaults.
// Updated from ConfigSync (Rust → Swift) at session open via updateHotkeyConfig(_:).
// All accesses on main thread: hotkeyTapCallback runs on main run loop.
private var hotkeyKeyCode:        Int64 = 49    // kVK_Space
private var hotkeyRequiresCtrl:   Bool  = true
private var hotkeyRequiresShift:  Bool  = true
private var hotkeyRequiresCmd:    Bool  = false
private var hotkeyRequiresOption: Bool  = false
```

#### 7b. Replace hardcoded constants in `isHotkeyEvent(_:)`

```swift
private func isHotkeyEvent(_ event: CGEvent) -> Bool {
    let keyCode = event.getIntegerValueField(.keyboardEventKeycode)
    let flags   = event.flags
    return keyCode == hotkeyKeyCode
        && flags.contains(.maskControl)   == hotkeyRequiresCtrl
        && flags.contains(.maskShift)     == hotkeyRequiresShift
        && flags.contains(.maskCommand)   == hotkeyRequiresCmd
        && flags.contains(.maskAlternate) == hotkeyRequiresOption
}
```

The `== Bool` form handles both required and excluded modifiers uniformly. When
`hotkeyRequiresCmd = false`, `flags.contains(.maskCommand) == false` is equivalent
to the original `!flags.contains(.maskCommand)`.

#### 7c. Add `updateHotkeyConfig(_:)`

```swift
/// Update hotkey detection parameters from a ConfigSync proto message.
/// Called on MainActor by DexterClient.onResponse.
/// isHotkeyEvent(_:) is also called on the main thread, so no data race.
func updateHotkeyConfig(_ config: Dexter_V1_HotkeyConfig) {
    hotkeyKeyCode        = Int64(config.keyCode)
    hotkeyRequiresCtrl   = config.ctrl
    hotkeyRequiresShift  = config.shift
    hotkeyRequiresCmd    = config.cmd
    hotkeyRequiresOption = config.option
    logger.debug("Hotkey config updated: keyCode=\(config.keyCode)")
}
```

**After Steps 5–7: `swift build` → 0 project-code warnings.**

---

### Step 8: Full regression

```bash
cargo test            # 200 passing, 0 failed, 0 warnings
cd src/swift && swift build  # 0 project-code warnings
uv run pytest -q      # 19 passed
make smoke
```

**Manual validation (requires `make run`):**

1. Press Ctrl+Shift+Space → entity LISTENING *(existing behavior, now config-driven)*
2. Add `[hotkey]\nshift = false` to config, restart core
3. Press Ctrl+Space (no Shift) → LISTENING ✓
4. Press Ctrl+Shift+Space → no transition ✓ *(Shift no longer required)*
5. Add `com.apple.dt.Xcode` to `[behavior] proactive_excluded_bundles`, restart, focus Xcode → no proactive fires
6. Focus Safari → proactive fires after interval ✓
7. Focus any app (no exclusion), wait for proactive to fire and speak:
   - Entity stays THINKING throughout playback ✓
   - Entity transitions to IDLE only after audio finishes ✓
   - No IDLE flash while voice is still audible ✓

---

## 7. Acceptance Checklist

- [x] AC-1  `HotkeyConfig` in config.rs: 5 fields, serde defaults match Phase 16 hardcoded values
- [x] AC-2  `[hotkey]` TOML section overrides each field independently
- [x] AC-3  `DexterConfig` includes `hotkey: HotkeyConfig`
- [x] AC-4  `BehaviorConfig` includes `proactive_excluded_bundles: Vec<String>` (default empty)
- [x] AC-5  `ProactiveEngine::should_fire()` returns false when bundle in exclusion list
- [x] AC-6  Empty exclusion list → Phase 17 parity (no apps blocked)
- [x] AC-7  `dexter.proto` contains `HotkeyConfig`, `ConfigSync`, `AudioResponse.is_final`, `AUDIO_PLAYBACK_COMPLETE`
- [x] AC-8  `ServerEvent` oneof field 6 is `config_sync`
- [x] AC-9  `SYSTEM_EVENT_TYPE_AUDIO_PLAYBACK_COMPLETE = 8` in `SystemEventType`
- [x] AC-10 `CONNECTED` → `EntityState(Idle)` then `ConfigSync` (two events, correct order)
- [x] AC-11 `ConfigSync.hotkey` values match operator's `[hotkey]` config section
- [x] AC-12 `AUDIO_PLAYBACK_COMPLETE` → `EntityState(Idle)` in orchestrator
- [x] AC-13 Proactive path sends `is_final = true` sentinel; does NOT call `send_state(Idle)` directly (TTS-available path)
- [x] AC-14 No-TTS path still sends `EntityState(Idle)` directly from Rust
- [x] AC-15 `AudioPlayer.onPlaybackFinished` fires after last buffer completes
- [x] AC-16 `AudioPlayer.stop()` clears `awaitingFinalCallback` (no stale callback on barge-in)
- [x] AC-17 `DexterClient` sets `onPlaybackFinished` callback before any audio arrives
- [x] AC-18 `EventBridge.isHotkeyEvent(_:)` reads stored properties (no hardcoded constants)
- [x] AC-19 `EventBridge.updateHotkeyConfig(_:)` updates all 5 stored properties
- [x] AC-20 Entity stays THINKING during proactive TTS playback (manual verification)
- [x] AC-21 Entity transitions to IDLE only when audio finishes (manual verification)
- [x] AC-22 `cargo test` ≥ 200 passing, 0 failed
- [x] AC-23 `cargo test` 0 warnings
- [x] AC-24 `swift build` 0 project-code warnings
- [x] AC-25 `uv run pytest` 19/19
- [x] AC-26 `make proto` succeeds cleanly

---

## 8. Known Pitfalls

**Pitfall: `is_final = true` with empty data — `makeBuffer` returns nil**

`AudioPlayer.makeBuffer(from:)` checks `frameCount > 0`. For the empty sentinel
(`data: []`), `frameCount == 0` → returns `nil`. The sentinel is therefore never
scheduled as a buffer. This is intentional — the sentinel only arms
`awaitingFinalCallback`. The `flushReadyBuffers()` call immediately after
`awaitingFinalCallback = true` + `checkFinalCallback()` correctly handles the case
where all buffers have already played before the sentinel arrives.

**Pitfall: `checkFinalCallback()` called twice for the sentinel path**

After setting `awaitingFinalCallback = true` in `enqueue`, two calls to
`checkFinalCallback()` happen: one at the end of `enqueue`'s dispatch block, and
potentially one from the completion handler of the last in-flight buffer (if it
finishes between those two moments). The flag reset `awaitingFinalCallback = false`
inside `checkFinalCallback()` prevents double-firing: the second call sees `false`
and returns immediately. Both calls are on `self.queue` (serial), so there's no race.

**Pitfall: `onPlaybackFinished` must be set before `enqueue` is called**

`onPlaybackFinished` is set in `runSession` immediately after `audioPlayer.start()`.
The `onResponse` closure that calls `audioPlayer.enqueue` is set up later in the same
`session.bidirectionalStreaming` call. Since both happen before the gRPC stream is live
(no events arrive until the stream is open), the callback is always set before any
`AudioResponse` events arrive.

**Pitfall: Barge-in fires while proactive TTS is playing**

`VoiceCapture.onSpeechStart` calls `audioPlayer.stop()` synchronously. `stop()` now
also clears `awaitingFinalCallback = false`. So if the operator speaks during a
proactive observation, the callback is disarmed, and `AUDIO_PLAYBACK_COMPLETE` is never
sent. The entity stays in THINKING state until... Rust never gets AUDIO_PLAYBACK_COMPLETE.
Dexter is stuck in THINKING.

**Mitigation for barge-in:** In `stop()`, after clearing `awaitingFinalCallback`, fire
`onPlaybackFinished` immediately if it was armed — the observation was interrupted, not
completed, but the entity must still return to IDLE. Updated `stop()`:

```swift
func stop() {
    queue.sync { [self] in
        let wasFinal = awaitingFinalCallback
        player.stop()
        player.play()
        sequenceQueue.removeAll()
        pendingBufferCount    = 0
        nextExpectedSeq       = 0
        _isPlaying            = false
        awaitingFinalCallback = false
        // If a proactive observation was interrupted, fire immediately so Rust
        // receives AUDIO_PLAYBACK_COMPLETE and can transition to IDLE.
        if wasFinal { onPlaybackFinished?() }
    }
}
```

**Pitfall: `flags.contains(.maskX) == Bool` — semantics are exact**

When `hotkeyRequiresCmd = false`, `flags.contains(.maskCommand) == false` correctly
rejects any keypress that includes Cmd. When `hotkeyRequiresCmd = true`, it correctly
requires Cmd. The `== Bool` form handles both inclusion and exclusion uniformly. Do not
mix `contains` and `!contains` — the `== Bool` form covers both cases.

**Pitfall: `ConfigSync` must use `Dexter_V1_HotkeyConfig`, not a local Swift struct**

The generated type is `Dexter_V1_HotkeyConfig`. `EventBridge.updateHotkeyConfig(_:)`
accepts it directly. No intermediate Swift struct needed — `HotkeyConfig` touches only
one call site (unlike `EntityState` which drives rendering across the entire entity layer).

**Pitfall: `make proto` must precede Step 4**

Steps 4–7 reference `ConfigSync`, `HotkeyConfig`, `AudioPlaybackComplete`, and
`AudioResponse.is_final` from the generated proto files. Running `make proto` before
Step 4 is mandatory.

---

## 9. Known Constraints / Phase 19 Deferred Items

- **Regular streaming SPEAKING→IDLE timing**: The same synthesis-vs-playback gap exists
  in `generate_and_stream()`'s TTS path. Extending the `is_final` mechanism requires
  marking the last sentence's audio at the `SentenceSplitter` level — deferred to Phase 19.
- **Proactive on significant AX element changes**: Deferred again. `AxElementChanged`
  fires on every cursor move; a significance classifier is needed.
- **`HotkeyConfig.enabled: bool`**: Currently there's no way to disable the hotkey
  entirely via config. Adding `enabled = false` to `[hotkey]` is Phase 19+.
- **Per-bundle exclusion UX**: Operator must know bundle IDs. A voice command
  "don't comment on this app" could auto-populate the list. Phase 19+.
