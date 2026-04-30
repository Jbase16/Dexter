# Phase 20 — Vision Integration: Screen Capture + llama3.2-vision Queries
## Spec version 1.0 — Session 021, 2026-03-14

> **Status:** COMPLETE.
> This document is the retroactive implementation record for Phase 20.
> All decisions reflect what was actually built and tested.

---

## 1. What Phase 20 Delivers

`llama3.2-vision:11b` has been configured in `SESSION_STATE.json` since Phase 0 and
routed via `ModelId::Vision` since Phase 5. What it has never had is an activation path —
no mechanism to attach an actual image to the inference request, and no screen capture
capability to provide one. Phase 20 wires that path end-to-end.

| Deliverable | What It Does |
|-------------|--------------|
| **`Message.images` field** | `Option<Vec<String>>` added to the public `Message` struct with `skip_serializing_if`. Normal text messages are unaffected (field absent from JSON). Vision messages serialize to `{"role":"user","content":"...","images":["<base64>"]}` — exactly the Ollama multimodal API contract. |
| **`Message::user_with_image()` constructor** | Convenience constructor for creating a user message with a single base64-encoded image. Part of the public `Message` API for callers that build vision messages from scratch. |
| **`capture_screen()` method on `CoreOrchestrator`** | Captures the main display using `screencapture -x -m -t png`. Returns `Option<String>` (base64-encoded PNG). Returns `None` on any failure: process spawn error, non-zero exit, read error, or timeout. Never blocks or panics. Uses a per-invocation UUID path (`/tmp/dexter_screen_<uuid>.png`) to prevent race conditions under concurrent calls. |
| **Vision image attachment in `handle_text_input` (step 4d)** | When the router selects `ModelId::Vision`, `capture_screen()` is called and the base64 result is attached to the last user message in the ephemeral `messages` vec before generation. Images are never stored in `ConversationContext`. |
| **`ModelId::Vision.unload_after_use()` → `true`** | Vision (~8GB) joins Heavy (~18GB) in the unload-after-use policy. Both models must be mutually exclusive residents on 36GB hardware. Unloading Vision after each query ensures Heavy can load on-demand without an explicit pre-unload step. |
| **Screen capture constants** | `SCREEN_CAPTURE_PATH_PREFIX = "/tmp/dexter_screen"` and `SCREEN_CAPTURE_TIMEOUT_SECS = 5` added to `constants.rs`. Full path is `<PREFIX>_<uuid>.png` per invocation. |
| **Extended vision routing keywords** | Router gains `"see my screen"`, `"look at the screen"`, `"look at my screen"` — natural operator phrasings that were not covered by the existing keyword set. |

**What this does NOT include:**
- Vision-triggered proactive observations (watching the screen unprompted) — deferred.
- OCR pipeline for apps without AX text access — deferred (Phase 23 per Phase 19 spec).
- Multi-image or video frame sequences — single-screenshot-per-query only.
- Image persistence in conversation history — explicitly excluded by design (see §3.2).

**Test count target:** 225 Rust passing (previously 214). 11 new tests (10 original + 1 Issue 2 regression test added during review).

---

## 2. What Already Exists (Do Not Rebuild)

| Component | Phase | Relevance to Phase 20 |
|-----------|-------|-----------------------|
| `ModelId::Vision` + `ModelRouter` vision routing | 5 | Vision category and keyword detection exist. Phase 20 extends keywords and activates the model path. |
| `llama3.2-vision:11b` configured in `ModelConfig` | 0 | Already present. Phase 20 is the first phase to actually invoke it. |
| `GenerationRequest` + `generate_stream()` | 4 | Accepts `Vec<Message>`. Adding `images` to `Message` automatically flows through to Ollama serialization — no changes to `generate_stream` itself. |
| `OllamaChatRequest` serialization | 4 | Uses `messages: &'a [Message]` directly. `Message`'s new `images` field with `skip_serializing_if` propagates without changes to the request struct. |
| `prepare_messages_for_inference()` | 19 | Returns the ephemeral `Vec<Message>` that Phase 20 mutates to attach vision images. Images never reach `ConversationContext`. |
| `handle_text_input` routing decision | 5/16 | `decision.model == ModelId::Vision` is the trigger condition for step 4d. |

---

## 3. Architecture

### 3.1 The image attachment boundary

The critical design decision in Phase 20 is **where** images live relative to the
conversation lifecycle.

```
ConversationContext (persistent, disk-serialized)
  └── messages: Vec<Message>   ← images NEVER stored here

prepare_messages_for_inference() → Vec<Message>  (ephemeral clone)
  └── step 4d: last_user.images = Some(vec![b64])   ← attached here

generate_primary() / generate_and_stream()  ← sees image
  └── GenerationRequest.messages  ← ephemeral, dropped after generation

Session state on disk  ← no images
```

**Why images are not stored in `ConversationContext`:**

1. **Size.** A single 2.4MB PNG base64-encodes to ~3.2MB. Three conversation turns with
   screen captures would add ~10MB to the session state file — and the captures are
   stale within seconds of being taken.

2. **Semantic staleness.** A screenshot from two turns ago is not context for the current
   query — the screen has changed. Persisting it implies a continuity that doesn't exist.

3. **Model confusion.** Sending old images in conversation history alongside new ones would
   confuse the vision model about which image is the current subject.

4. **Privacy.** Session state is persisted to `~/.dexter/state/`. Screen captures of
   sensitive content (passwords, private documents) should not be written there permanently.

The correct lifetime is: captured → encoded → attached to one request → discarded.

### 3.2 The ephemeral mutation pattern

```rust
// In handle_text_input, AFTER prepare_messages_for_inference():
let mut messages = self.prepare_messages_for_inference();

// Step 4d — mutate the ephemeral vec, not self.context:
if decision.model == ModelId::Vision {
    if let Some(image_b64) = self.capture_screen().await {
        if let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") {
            last_user.images = Some(vec![image_b64]);
        }
    }
}
```

`prepare_messages_for_inference()` returns an owned `Vec<Message>` (not references into
`self.context`). Mutating that vec is safe and isolated — `self.context.messages` is never
touched.

### 3.3 Screen capture implementation

```
screencapture -x -m -t png /tmp/dexter_screen.png
  -x    suppress camera shutter sound (silent capture)
  -m    main display only (no secondary monitors — avoids ambiguity)
  -t    png format (lossless; Ollama vision API accepts PNG and JPEG)
```

The file is written to `SCREEN_CAPTURE_PATH`, read via `tokio::fs::read`, base64-encoded
with `base64::engine::general_purpose::STANDARD.encode()`, then deleted. The file exists
only during the call to `capture_screen()` — typically ~100–200ms on Apple Silicon.

**Failure modes and handling:**

| Failure mode | Cause | Handling |
|---|---|---|
| `spawn failed` | `screencapture` not in PATH | `warn!` → `None` |
| non-zero exit | Screen Recording permission denied, no display | `warn!` → `None` |
| file read error | Disk full, race condition | `warn!` → `None` |
| timeout (>5s) | Quartz compositor stall, display sleep | `warn!` → `None` |

All paths return `None`. The caller (step 4d in `handle_text_input`) treats `None` as
"proceed text-only" — the vision model still responds, just without a screenshot.

### 3.4 VRAM mutual exclusion policy

`ModelId::unload_after_use()` returns `true` for both `Heavy` and `Vision`:

```
Heavy  (deepseek-r1:32b)  ~18GB  — unload always
Vision (llama3.2-vision:11b) ~8GB — unload always (Phase 20 change)

Headroom budget (36GB):
  Fast    (qwen3:8b)       ~5GB  ┐
  Primary (mistral-small:24b) ~14GB ┘  ~19GB warm resident
  Heavy   loaded on-demand:  +18GB → 37GB → requires Fast+Primary eviction (Ollama handles)
  Vision  loaded on-demand:  +8GB  → 27GB → safe margin
```

If Vision were not unloaded after use, a subsequent Heavy request would find
both models resident (~27GB + 18GB = 45GB > 36GB), forcing a contested eviction
that could cause performance instability or OOM pressure during the transition.

The `keep_alive: 0` parameter on the final streaming request handles the eviction via
Ollama's existing mechanism — no separate unload API call needed.

### 3.5 macOS TCC requirement

Screen capture requires the **Screen Recording** permission in:
```
System Settings → Privacy & Security → Screen Recording
```

The permission is checked against the executable path in the **user** TCC database:
```
~/Library/Application Support/com.apple.TCC/TCC.db
```

Query to verify (SIP disabled):
```sql
SELECT client, auth_value FROM access
WHERE service = 'kTCCServiceScreenCapture'
  AND client = '<bundle_id_or_executable_path>';
-- auth_value = 2 → allowed
```

`setup.sh` must be extended to open the Screen Recording privacy pane alongside the
existing Accessibility and Microphone checks, and to verify `auth_value = 2` before
allowing Dexter to start.

The `cargo test` binary path changes on every build (hash-suffixed in `target/debug/deps/`)
and cannot accumulate a persistent TCC grant. Test behavior when Screen Recording is
absent: `screencapture` prints "could not create image from display" and exits non-zero;
`capture_screen()` returns `None`; the test asserts non-panic and passes. This is correct
— tests should exercise the degradation path, not require a display.

---

## 4. Files Changed

| File | Change |
|------|--------|
| `Cargo.toml` | Added `base64 = "0.22"` |
| `src/constants.rs` | Added `SCREEN_CAPTURE_PATH_PREFIX`, `SCREEN_CAPTURE_TIMEOUT_SECS` |
| `src/inference/engine.rs` | `Message.images: Option<Vec<String>>`, updated constructors, `Message::user_with_image()`, 3 new tests |
| `src/inference/models.rs` | `unload_after_use()` returns `true` for `Vision`, updated existing test, 1 new test |
| `src/inference/router.rs` | Added `"see my screen"`, `"look at the screen"`, `"look at my screen"` keywords; `push_tool_result` struct literal updated; 3 new tests |
| `src/orchestrator.rs` | `capture_screen()` method, step 4d vision image attachment, all `Message { }` struct literals migrated to constructors, 3 new tests |

---

## 5. New Tests (10 total → 224 passing)

### `src/inference/engine.rs` (3 tests)

| Test | What It Verifies |
|------|-----------------|
| `message_with_images_serializes_images_array_field` | `Message { images: Some([...]) }` → JSON contains `"images"` key with the base64 payload. Verifies Ollama multimodal API contract. |
| `message_without_images_skips_images_field_in_json` | `Message::user("hello")` → JSON has no `"images"` key at all (not `"images":null`). Ensures text-only messages don't confuse non-vision models. |
| `message_user_with_image_constructor_creates_single_image_vec` | `Message::user_with_image(content, b64)` → correct role, content, and single-element images vec. |

### `src/inference/models.rs` (1 new test + 1 updated)

| Test | What It Verifies |
|------|-----------------|
| `model_vision_unloads_after_use_for_vram_safety` *(new)* | `ModelId::Vision.unload_after_use()` is `true`. Dedicated test with VRAM mutual exclusion rationale in assertion message. |
| `heavy_and_vision_unload_after_use` *(renamed from `only_heavy_unloads_after_use`)* | Both Heavy and Vision return `true`; Fast, Primary, Code, Embed return `false`. |

### `src/inference/router.rs` (3 tests)

| Test | What It Verifies |
|------|-----------------|
| `route_see_my_screen_returns_vision` | `"can you see my screen?"` → `Category::Vision`, `ModelId::Vision`. New Phase 20 keyword. |
| `route_look_at_the_screen_returns_vision` | `"look at the screen and tell me what app is open"` → `Category::Vision`. New Phase 20 keyword. |
| `route_picture_keyword_returns_vision` | `"take a picture of what's on screen"` → `Category::Vision`. Documents pre-existing `"picture"` keyword coverage. |

### `src/orchestrator.rs` (3 tests)

| Test | What It Verifies |
|------|-----------------|
| `screen_capture_constants_are_valid` | `SCREEN_CAPTURE_PATH_PREFIX` starts with `/tmp`, does not end with `/` or `.png`; `SCREEN_CAPTURE_TIMEOUT_SECS > 0`. Guards against accidental constant drift and malformed path construction. |
| `vision_messages_have_no_images_before_capture_attachment` | All messages from `prepare_messages_for_inference()` have `images = None`. Guards against accidental image persistence in context. |
| `vision_image_attaches_to_user_message_when_tool_result_follows` | **Issue 2 regression test.** Simulates Phase 19 + Phase 20 combined path: message list with user message followed by `role="retrieval"` tool_result. Verifies `rev().find(role == "user")` skips the tool_result and attaches the image to the correct message. Also verifies tool_result is unchanged. |
| `capture_screen_returns_none_gracefully_when_screencapture_fails` | `capture_screen()` does not panic regardless of display/permission availability. Accepts both `Some` and `None` — verifies the non-panic contract, not a specific result. |

---

## 6. Acceptance Criteria

| # | Criterion | Verified |
|---|-----------|----------|
| AC-1 | `cargo build` produces 0 warnings from project code | ✓ |
| AC-2 | `cargo test` passes 225 tests, 0 failures | ✓ |
| AC-3 | `swift build` clean, 0 warnings | ✓ |
| AC-4 | `uv run pytest -q` passes 19 tests | ✓ |
| AC-5 | `Message` with `images: None` serializes without `"images"` key | ✓ |
| AC-6 | `Message` with `images: Some([...])` serializes with correct `"images"` array | ✓ |
| AC-7 | `ModelId::Vision.unload_after_use()` returns `true` | ✓ |
| AC-8 | Vision routing keywords include natural-language screen-reference phrases | ✓ |
| AC-9 | `capture_screen()` returns `None` without panicking when Screen Recording is denied | ✓ (demonstrated in test run) |
| AC-10 | `capture_screen()` returns `Some(base64_png)` when Screen Recording is granted | ✓ (demonstrated: 0.17s vs 0.06s baseline, no error output) |
| AC-11 | No image data written to `ConversationContext` or session state | ✓ (enforced by `vision_messages_have_no_images_before_capture_attachment` test) |

---

## 7. Known Limitations and Deferred Work

**`setup.sh` not updated (deferred):**
Screen Recording permission verification should be added to `setup.sh` alongside the
existing Accessibility and Microphone checks. The TCC query pattern is documented in §3.5.
Deferred because `setup.sh` is a maintenance task that does not block functionality.

**Single-display only:**
The `-m` flag captures the main display. Multi-monitor setups will not capture secondary
displays. This is the correct default — the operator's primary work surface is the main
display. A future enhancement could allow specifying a display index.

**Vision queries do not feed back into retrieval:**
If the vision model identifies something in the screenshot that could benefit from web
lookup (e.g., an error message), there is no Phase 19 uncertainty sentinel path for the
vision response. Combining vision + retrieval in a single query is future work.

**No vision in proactive observations:**
`handle_proactive_observation()` does not use the Vision tier. Proactive observations are
text-context-driven (AX events) and do not capture the screen autonomously. This is
intentional — unprompted screen captures raise privacy concerns that require explicit
operator consent patterns not yet designed.

---

*Spec written retroactively: 2026-03-14, Session 021*
*Implementation completed: 2026-03-14, Session 021*
