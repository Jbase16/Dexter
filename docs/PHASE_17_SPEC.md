# Phase 17 — Proactive Context-Triggered Observations
## Spec version 1.0 — Session 017, 2026-03-13

> **Status:** COMPLETE.
> 192 Rust tests passing, 19 Python tests passing, 0 warnings (Rust + Swift).

---

## 1. What Phase 17 Delivers

Phase 16 made Dexter context-aware *when responding*. Phase 17 makes Dexter
context-aware *when initiating* — the behavioural leap from chatbot to persistent
entity.

| Deliverable | Why |
|-------------|-----|
| **ProactiveEngine** — rate-limited proactive observation initiator | Dexter can now comment on what the operator is doing without being asked |
| **BehaviorConfig** — `[behavior]` section in `config.toml` | Operator controls proactive interval, startup grace, and on/off toggle |
| **collect_generation()** — non-streaming generation path | Enables `[SILENT]` inspection before any output reaches the UI |

**What this does NOT include:**
- Configurable hotkey combination (deferred to Phase 18 — requires proto changes)
- Wake phrase detection ("Dexter, listen") — Phase 18+
- Cross-session context persistence — deferred (memory is per-session, Phase 9 retrieval covers semantic recall)

---

## 2. Architecture

### 2.1 ProactiveEngine (`src/rust-core/src/proactive/engine.rs`)

Rate-limited gate that governs when Dexter initiates a proactive observation.

```
ProactiveEngine::should_fire(snapshot) — gates:
  [1] enabled == true                    (config toggle)
  [2] !snapshot.is_screen_locked         (respect privacy)
  [3] snapshot.app_name.is_some()        (context must be non-trivial)
  [4] session age ≥ startup_grace_secs   (let operator settle, default 30s)
  [5] time since last fire ≥ interval    (rate limit, default 90s)
```

`record_fire()` resets the rate-limit clock. It is called BEFORE generation
(not after) so a failed inference still burns the slot — preventing rapid
re-fire loops on Ollama connectivity issues.

### 2.2 collect_generation() vs. generate_and_stream()

Two distinct generation paths now exist in `CoreOrchestrator`:

| Path | Streaming? | Use case |
|------|-----------|---------|
| `generate_and_stream()` | Yes — tokens sent to UI as they arrive | All user-initiated turns |
| `collect_generation()` | No — tokens accumulated, then inspect | Proactive observations only |

The non-streaming path is mandatory for proactive because the model can respond
with `[SILENT]` — a silent opt-out that must be intercepted before the operator
sees it. Streaming would display "[SILENT]" briefly before we could suppress it.

### 2.3 [SILENT] opt-out

The proactive prompt explicitly tells the model:

> "…or respond with exactly [SILENT] to stay quiet if you have nothing useful to add."

`ProactiveEngine::is_silent_response()` checks:
- Empty/whitespace-only response
- Case-insensitive `[SILENT]` (handles `[silent]`, `[Silent]`, etc.)

When silent: entity transitions immediately to IDLE, no text or TTS output.

### 2.4 Proactive messages layout

```
[0] system: personality ("You are Dexter...")
[1] system: [Context: Xcode — Source Editor: func parseVmStat]
[2] user:   [Proactive] You noticed the operator is now working in: ...
```

No conversation history is included. Proactive observations are context-driven
(what is the operator doing right now?), not dialogue-driven.

### 2.5 Conversation history: ephemeral

Proactive responses are NOT added to `ConversationContext` or `SessionStateManager`.
They are ambient observations — like a colleague glancing at your screen and
commenting. The model does not need to remember its proactive observations.

If the operator wants to reference a proactive observation, they speak — and the
model reconstructs based on current context.

### 2.6 TTS delivery

Proactive responses use TTS if available. Unlike regular streaming (which uses
`SentenceSplitter` to pipeline synthesis), proactive sends the complete response
as a single TTS unit — appropriate because observations are 1 sentence.

Entity state stays THINKING until TTS finishes, then transitions to IDLE.

### 2.7 BehaviorConfig (`[behavior]` in config.toml)

```toml
[behavior]
proactive_enabled = true
proactive_interval_secs = 90
proactive_startup_grace_secs = 30
```

All fields default to the above values when absent. `DexterConfig` gained a new
`behavior: BehaviorConfig` field; all existing tests pass without modification
because `BehaviorConfig::default()` is used by `DexterConfig::default()`.

---

## 3. Files Changed

### New files
| File | Purpose |
|------|---------|
| `src/rust-core/src/proactive/mod.rs` | Module entry — re-exports `ProactiveEngine` |
| `src/rust-core/src/proactive/engine.rs` | `ProactiveEngine` struct + 9 unit tests |
| `docs/PHASE_17_SPEC.md` | This document |

### Modified files
| File | Change |
|------|--------|
| `src/rust-core/src/config.rs` | Added `BehaviorConfig`, `DexterConfig.behavior`, 2 new tests |
| `src/rust-core/src/orchestrator.rs` | Added `proactive_engine` field, `collect_generation()`, `do_proactive_response()`, proactive trigger in `handle_system_event` |
| `src/rust-core/src/main.rs` | Added `mod proactive;` |
| `docs/SESSION_STATE.json` | Phase 17 complete, test counts updated |

---

## 4. New Tests

### proactive/engine.rs (9 new tests)

| Test | Validates |
|------|-----------|
| `proactive_engine_disabled_never_fires` | Gate 1: enabled flag |
| `proactive_engine_does_not_fire_when_screen_locked` | Gate 2: screen lock |
| `proactive_engine_does_not_fire_with_no_app` | Gate 3: app context required |
| `proactive_engine_does_not_fire_during_startup_grace` | Gate 4: startup grace |
| `proactive_engine_does_not_fire_before_min_interval` | Gate 5: rate limiting |
| `proactive_engine_fires_when_all_gates_pass` | Happy path: all gates clear |
| `proactive_engine_record_fire_blocks_immediate_repeat` | record_fire() resets clock |
| `is_silent_response_detects_variants` | [SILENT] case-insensitive + empty |
| `is_silent_response_does_not_suppress_real_responses` | No false positives |
| `build_proactive_prompt_contains_context_summary` | Prompt embeds context + [SILENT] hint |

### config.rs (2 new tests)

| Test | Validates |
|------|-----------|
| `behavior_defaults_are_correct` | Default values for all BehaviorConfig fields |
| `behavior_partial_override_preserves_defaults` | TOML partial override leaves absent fields at defaults |

---

## 5. Acceptance Checklist

- [x] AC-1  `ProactiveEngine::should_fire()` returns `false` when `enabled = false`
- [x] AC-2  `should_fire()` returns `false` when screen is locked
- [x] AC-3  `should_fire()` returns `false` when no app is focused
- [x] AC-4  `should_fire()` returns `false` within startup grace period
- [x] AC-5  `should_fire()` returns `false` within min interval after `record_fire()`
- [x] AC-6  `should_fire()` returns `true` when all gates pass
- [x] AC-7  `[SILENT]` response suppresses all UI output (text + TTS + entity state reverts to IDLE)
- [x] AC-8  Non-silent response sends full text as `is_final=true` TextResponse
- [x] AC-9  Proactive response is NOT added to `ConversationContext` or `SessionStateManager`
- [x] AC-10 Proactive uses FAST model (`model_config.fast`)
- [x] AC-11 `[behavior]` section in `config.toml` controls proactive parameters
- [x] AC-12 `cargo test` ≥ 192 passing, 0 failed
- [x] AC-13 `cargo build` 0 warnings
- [x] AC-14 `swift build` 0 project-code warnings (no Swift changes this phase)
- [x] AC-15 `uv run pytest` 19/19 (no Python changes this phase)

---

## 6. Known Constraints / Next Phase Deferred Items

- **Hotkey configurability**: Deferred to Phase 18. Requires a new `HotkeyConfig` proto
  message so Rust can push the config to Swift's EventBridge at session start.
- **Proactive on element change**: Currently only fires on `AppFocused` changes.
  `AxElementChanged` events fire too frequently (every cursor move in Xcode). Phase 18
  could add a "significant element change" classifier to trigger proactive selectively.
- **Operator feedback loop**: No mechanism for operator to say "don't observe this app".
  Phase 18+ could add a per-bundle exclusion list to `BehaviorConfig`.
