# Phase 39 Live Verification Checkpoint

## Context

Phase 39 is a checkpoint phase, not a feature phase. The goal is to record the
first broad live-verification pass after the action policy, CLI smoke harness,
Humor Engine, Contacts message path, HUD approval path, action cancellation, and
barge-in regressions were all exercised together.

This document is intentionally evidence-oriented. It captures what was run, what
passed, and what remains outside the checkpoint.

Checkpoint date: 2026-05-16.

---

## Outcome

The live stack passed.

The following checks completed successfully:

```bash
bash -n scripts/live-cli-smoke.sh
cd src/rust-core && cargo fmt --check
cd src/rust-core && cargo test --bin dexter-core --no-fail-fast
make live-smoke-action-matrix
make live-smoke-cli
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact
make live-smoke-hud-approval
make live-smoke-hud
make live-smoke-action-cancel
make live-smoke-barge-in
make live-smoke-all
make smoke
```

Observed results:

| Check | Result | Notes |
|---|---:|---|
| `bash -n scripts/live-cli-smoke.sh` | PASS | Shell smoke harness syntax valid. |
| `cargo fmt --check` | PASS | Rust formatting clean. |
| `cargo test --bin dexter-core --no-fail-fast` | PASS | 584 passed, 7 ignored. |
| `make live-smoke-action-matrix` | PASS | Deterministic action matrix green. |
| `make live-smoke-cli` | PASS | Natural-language CLI live smoke green. |
| `make live-smoke-message-contact` | PASS | Contacts-backed message path reached approval and auto-denied. |
| `make live-smoke-hud-approval` | PASS | Real Swift HUD approval-required action smoke green. |
| `make live-smoke-hud` | PASS | Real Swift HUD baseline lifecycle smoke green. |
| `make live-smoke-action-cancel` | PASS | Long-running action subprocess killed on hotkey cancel. |
| `make live-smoke-barge-in` | PASS | Swift/TTS barge-in smoke green. |
| `make live-smoke-all` | PASS | Full live suite green, each target with its own daemon. |
| `make smoke` | PASS | Rust check, Swift build, and Python worker tests green. |

After the live smoke runs, `/tmp/dexter.sock` was checked and no Dexter daemon
was left accepting connections.

---

## Action Consolidation Addendum — 2026-05-22

The action regression surface gained one more deterministic failure-path smoke:

```bash
make live-smoke-external-failures
```

This target starts its own release core with test-only failure knobs and verifies:

- generic `message_send` actions fail closed unless the orchestrator resolves
  the recipient through Contacts first;
- AppleScript runtime errors are visible to the operator;
- AppleScript timeouts are bounded and visible in action receipts;
- failed `screencapture` demotes Vision to PRIMARY instead of sending a fake
  text-only vision request.

The action smoke scripts that start their own daemon now stop only that daemon
and assert `/tmp/dexter.sock` plus `/tmp/dexter-shell.sock` are gone on
successful runs. Their daemon warmup window is now 300s by default
(`DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS` overrides it) to avoid false failures
when Ollama cold-loads slowly under macOS memory pressure.

Observed consolidation results:

| Check | Result | Notes |
|---|---:|---|
| `cargo test --bin dexter-core` | PASS | 614 passed, 7 ignored. |
| `cargo test --bin dexter-cli` | PASS | 33 passed. |
| `make live-smoke-external-failures` | PASS | External failure paths green. |
| `make smoke` | PASS | Rust check, Swift build, and Python worker tests green. |

---

## Operator Status Addendum — 2026-05-22

`dexter-cli --status` and `make status` provide the single operator summary
view for local troubleshooting. The command reuses the doctor health checks,
prints worker recovery suggestions when applicable, and includes the latest
local action receipts from `audit.jsonl`.

The status smoke:

```bash
make live-smoke-operator-status
```

starts a fresh release core, writes one safe synthetic action receipt, runs
`dexter-cli --status`, and verifies the report contains:

- health checks, including daemon ping;
- recent action receipt source;
- the seeded action receipt;
- a final result line.

The Swift HUD status surface now mirrors the same operator shape in-app. The
HUD status button fetches `Health` and recent `ActionHistory`, renders both in
one `Dexter Status` document, and keeps restart buttons for degraded
daemon-lifetime workers.

```bash
make live-smoke-hud-health
```

verifies the real Swift app requests Health plus ActionHistory from the daemon,
surfaces the status document in the HUD, restarts a worker through
`RestartComponent`, and receives post-restart health.

`dexter-cli --why` and `make why` provide a local evidence report for "why did
or didn't Dexter act?" When the daemon is running, the CLI asks the Rust-owned
`ActionDiagnostic` RPC for the report; if the daemon is unavailable, it falls
back to the same offline audit/session evidence it used before. The report
combines current health, recent `audit.jsonl` receipts, and persisted or
in-flight session text for Rust-side refusals that never reached the action
engine, such as off-host command surfacing or Contacts-backed message refusal.

```bash
make live-smoke-action-diagnostic
```

starts a fresh release core, drives a synthetic `message_send` that must fail
closed before bypassing Contacts resolution, then verifies `dexter-cli --why`
explains the failure from the audit/session evidence.

The Swift HUD now exposes the same "why did/didn't that action run?" evidence
without Terminal. The HUD Why button calls the daemon `ActionDiagnostic` RPC and
renders a compact `Action Diagnostic` document in-app, so the classification
rules live in Rust rather than being duplicated in Swift.

```bash
make live-smoke-hud-action-diagnostic
```

starts a fresh release core, seeds a blocked raw `message_send` receipt through
`dexter-cli`, then verifies the real Swift HUD explains that block using local
evidence only.

Denied, expired, abandoned, and failed live action receipts now auto-surface
that same diagnostic after the immediate receipt is shown. `make
live-smoke-hud-approval` verifies this with a destructive action denial: the
HUD shows the action receipt, then automatically renders the fuller diagnostic
without requiring the operator to click the Why button.

Final text responses that look like no-receipt action refusals are also probed
through `ActionDiagnostic` after a short delay. The HUD only renders the report
when the daemon finds a concrete refusal clue, and turn/action guards prevent
that delayed report from overwriting a newer operator turn or a real action
receipt.

---

## Contacts Diagnostic Refresh Addendum — 2026-05-27

The action diagnostic and Contacts message path gained a targeted hardening pass
after the first operator-status checkpoint. The pass fixed two concrete drifts:

- no-receipt Contacts refusals now classify exact-recipient misses and
  handle/contact mismatches in both the daemon `ActionDiagnostic` path and the
  offline `dexter-cli --why` fallback;
- message-recipient extraction now treats `message Jason Phillips can you call
  me` as `Jason Phillips` instead of confusing the body pronoun `me` with a
  self-send request.

Two stale live-smoke assertions were refreshed to match the current operator
surface:

- `scripts/live-action-diagnostic-smoke.sh` now checks for the daemon-backed
  `Action Diagnostic` markdown shape and the current `Send iMessage to: ...`
  target text;
- `scripts/live-message-contact-smoke.sh` now checks the current approval
  wording (`review=approval required`) instead of an old destructive-category
  field.

Observed refresh results:

| Check | Result | Notes |
|---|---:|---|
| `cargo test --bin dexter-core` | PASS | 619 passed, 7 ignored. |
| `cargo test --bin dexter-cli` | PASS | 45 passed. |
| `cargo fmt --check` | PASS | Rust formatting clean. |
| `cargo build --release --bin dexter-core --bin dexter-cli` | PASS | Release core and CLI build clean. |
| `make live-smoke-action-receipts` | PASS | Safe, denied, and approved receipts inspect correctly. |
| `make live-smoke-approval-lifecycle` | PASS | Typed approval, cancel, denial, and timeout lifecycle green. |
| `make live-smoke-external-failures` | PASS | Raw `message_send`, AppleScript failures, and Vision demotion green. |
| `make live-smoke-action-diagnostic` | PASS | CLI `--why` explains blocked raw message-send evidence. |
| `make live-smoke-operator-status` | PASS | Status view combines health and recent action context. |
| `DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact` | PASS | Contacts resolved and approval auto-denied; no real iMessage sent. |
| `make live-smoke-action-matrix` | PASS | Deterministic shell/file/browser/AppleScript policy matrix green. |
| `make live-smoke-cli` | PASS | Humor Engine, natural-language actions, off-host, browser, clipboard, and shell context green. |
| `make live-smoke-recovery` | PASS | Browser, TTS, and STT worker restarts recover cleanly. |
| `make live-smoke-degraded-mode` | PASS | Bad config, Ollama outage, and missing worker diagnostics fail visibly. |
| `make live-smoke-hud` | PASS | Real Swift HUD typed lifecycle green. |
| `make live-smoke-hud-health` | PASS | HUD health/status and worker restart surface green. |
| `make live-smoke-hud-action-history` | PASS | HUD recent-actions RPC surface green. |
| `make live-smoke-hud-action-diagnostic` | PASS | HUD Why surface explains blocked action evidence. |
| `make live-smoke-hud-approval` | PASS | HUD approval denial and auto-diagnostic path green. |
| `make live-smoke-action-cancel` | PASS | Long-running subprocess is killed on hotkey cancel. |
| `make live-smoke-barge-in` | PASS | Swift/TTS barge-in smoke green. |
| `make smoke` | PASS | Rust check, Swift build, and 20 Python worker tests green. |

This refresh still deliberately does not include
`make live-smoke-message-contact-approve`, because that target sends a real
iMessage and should remain opt-in per run.

---

## Shared Action Evidence Addendum — 2026-05-27

The daemon `ActionDiagnostic` path and `dexter-cli` offline/operator paths now
share one action-evidence formatter for failed and successful audited actions.
This keeps the operator-facing diagnosis, evidence, target, and next-step copy
from drifting between:

- `dexter-cli --why`;
- `dexter-cli --status`;
- the daemon-owned `ActionDiagnostic` RPC used by the Swift HUD Why surface.

Observed helper consolidation results:

| Check | Result | Notes |
|---|---:|---|
| `cargo test --bin dexter-core action_evidence` | PASS | Shared evidence helper copy green. |
| `cargo test --bin dexter-core diagnostic_prefers_failed_receipt` | PASS | Daemon diagnostic prefers failed audit evidence. |
| `cargo test --bin dexter-cli format_operator_status_report` | PASS | CLI status latest-action summary green. |
| `cargo test --bin dexter-cli format_why_no_action_report_prefers_failed_action_receipt` | PASS | CLI why-report failed-receipt path green. |
| `cargo test --bin dexter-cli` | PASS | 48 passed. |
| `cargo test --bin dexter-core` | PASS | 621 passed, 7 ignored. |
| `DEXTER_SMOKE_SUMMARY_TARGETS="live-smoke-action-diagnostic live-smoke-operator-status live-smoke-hud-action-diagnostic" make live-smoke-summary` | PASS | 3 focused live smokes passed; summary at `docs/live-smoke-results/live-smoke-20260527_202232.md`. |
| `make smoke` | PASS | Rust check, Swift build, and 20 Python worker tests green. |

After the focused live smoke and `make smoke`, `/tmp/dexter.sock` was checked
and no Dexter daemon was left accepting connections.

The Swift HUD status surface was then brought into parity with `dexter-cli
--status`: it now renders a `Latest Action Summary` before `Recent Actions`.
That summary is daemon-owned: `ActionHistoryResponse` carries
`latest_action_summary_markdown`, produced from the same Rust action-evidence
formatter used by daemon diagnostics. Swift renders that field instead of
duplicating the success/failure, evidence, target, and next-step rules.

The HUD health smoke seeds a deterministic safe action receipt before opening
the status surface, so the live test verifies the actual latest-action summary
rather than relying on stale local audit history.

Observed HUD status parity results:

| Check | Result | Notes |
|---|---:|---|
| `bash -n scripts/live-hud-health-smoke.sh` | PASS | HUD health smoke syntax valid. |
| `make proto` | PASS | Swift proto artifacts regenerated with `latest_action_summary_markdown`. |
| `cargo test --bin dexter-core latest_action_summary_markdown` | PASS | Daemon-owned latest-action markdown copy green. |
| `cargo test --bin dexter-core` | PASS | 624 passed, 7 ignored. |
| `cargo test --bin dexter-cli` | PASS | 48 passed. |
| `cd src/swift && swift build` | PASS | Swift app build clean aside from existing warnings. |
| `make live-smoke-hud-health` | PASS | HUD status preview included `Latest Action Summary`, success evidence, and the seeded `HUD_STATUS_...` receipt. |
| `DEXTER_SMOKE_SUMMARY_TARGETS="live-smoke-operator-status live-smoke-hud-health live-smoke-hud-action-history" make live-smoke-summary` | PASS | 3 focused live smokes passed; summary at `docs/live-smoke-results/live-smoke-20260527_204928.md`. |
| `make smoke` | PASS | Rust check, Swift build, and 20 Python worker tests green. |

---

## What This Proves

### Model routing and Humor Engine

`make live-smoke-cli` verifies:

- Dirty joke follow-ups remain in the Humor Engine.
- Identity joke variation follow-ups remain in the Humor Engine.
- `step-dad joke` is treated as a literal dad-joke request, while explicit
  NSFW dad-joke phrasing routes as dirty humor.
- Normal chat still uses the standard router and does not get captured by the
  Humor Engine.

### Natural-language action emission

`make live-smoke-cli` still includes model-emission coverage for:

- Safe shell action result surfacing.
- Destructive shell action approval and denial.
- Destructive shell action approval and execution.
- Browser action health.
- Routine browser navigation/type/extract.
- Destructive browser click approval and denial.
- Destructive browser click approval and execution.

This is intentionally retained because deterministic action tests cannot prove
that the local model can still emit usable action blocks.

### Deterministic Rust-side action boundary

`make live-smoke-action-matrix` now drives exact `ActionSpec` JSON through
`dexter-cli --action-json`.

This verifies the Rust policy/approval/execution boundary without relying on a
local model to generate the correct JSON shape.

Covered action lanes:

- Shell:
  - safe shell executes immediately and surfaces output
  - destructive shell auto-denies without execution
  - destructive shell auto-approves and executes only the reviewed command
- File:
  - `file_read` executes immediately and surfaces content
  - cautious temp `file_write` executes immediately
  - protected `file_write` auto-denies without mutation
  - destructive temp `file_write` auto-approves and mutates only the fixture
- Browser:
  - navigate/type/extract execute without approval
  - destructive click auto-denies without DOM mutation
  - destructive click auto-approves and mutates only the local fixture
- AppleScript:
  - benign script executes without approval
  - destructive script auto-denies without mutation
  - destructive script auto-approves and mutates only the fixture

### Contacts-backed message send

`DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact`
verifies:

- Dexter uses Contacts-backed recipient resolution.
- A known contact reaches the message approval path.
- Auto-deny prevents sending.

This is deliberately not implemented as synthetic `ActionSpec` JSON. Directly
injecting a `message_send` action would bypass the orchestrator's Contacts
resolution and recipient-integrity path, which is the security-sensitive part
being tested.

### Swift HUD lifecycle and approval parity

`make live-smoke-hud` verifies the real Swift HUD can launch, connect to the
release Rust core, submit a typed turn through the HUD path, and observe the
expected client/core lifecycle.

`make live-smoke-hud-approval` verifies the real Swift HUD receives and handles
an approval-required action request without executing it in deny mode.

### Cancellation and barge-in

`make live-smoke-action-cancel` verifies a long-running action subprocess is
actually killed after hotkey cancellation. This is stronger than checking that
Dexter merely changes UI state.

`make live-smoke-barge-in` verifies the Swift/TTS interruption path does not
leave audio buffers or entity state stuck after barge-in.

### Broad build/runtime sanity

`make smoke` verifies:

- Rust core type-checks with `cargo check`.
- Swift builds.
- Python worker tests pass.

---

## What This Does Not Prove

This checkpoint does not claim:

- A real approved iMessage was sent during this exact checkpoint. The Contacts
  smoke was run in deny mode. A real-send path had been exercised previously,
  but this document records only this checkpoint's commands.
- Long-duration soak reliability. The suite starts and stops multiple daemons,
  but it is not an all-day memory, worker, or thermal stability test.
- Audio input correctness through the real microphone. Barge-in and TTS
  interruption are covered; full STT recognition quality is not.
- External website reliability. Browser tests use controlled fixtures where
  appropriate to avoid confusing external-network flakiness with Dexter bugs.
- Security review completeness. The action boundary is regression-tested, but
  this is not a full threat-model audit.

---

## Why `--action-json` Exists

The old action smoke path asked the model to emit an action, then checked whether
the Rust core handled it correctly. That mixed two different questions:

1. Can the model emit a usable action block?
2. Does Rust correctly classify, gate, execute, approve, deny, and report that
   action?

`dexter-cli --action-json` separates those questions. It sends an exact
`ActionSpec` through a dev-only synthetic `UIAction` envelope:

```json
{
  "source": "dexter-cli",
  "kind": "action_json",
  "action_json": {
    "type": "shell",
    "args": ["echo", "hi"]
  }
}
```

The orchestrator parses that envelope, validates the inner `ActionSpec`, then
submits it to the same `ActionEngine` policy and execution path used for model
actions.

This path is intentionally not exposed through Swift UI. It exists to make live
regression tests deterministic.

Natural-language action smokes remain in `make live-smoke-cli` so model action
emission is still covered separately.

---

## Verification Commands For Future Re-Runs

Fast local confidence:

```bash
make smoke
```

Rust-only confidence:

```bash
cd src/rust-core && cargo test --bin dexter-core --no-fail-fast
```

Deterministic action boundary:

```bash
make live-smoke-action-matrix
make live-smoke-external-failures
make live-smoke-operator-status
make live-smoke-action-diagnostic
make live-smoke-hud-action-diagnostic
make live-smoke-hud-health
make live-smoke-action-receipts
make live-smoke-approval-lifecycle
```

Main live CLI behavior:

```bash
make live-smoke-cli
```

Contacts-backed message approval without sending:

```bash
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact
```

Real Swift HUD approval:

```bash
make live-smoke-hud-approval
```

Full live suite:

```bash
make live-smoke-all
```

Full live suite with a markdown receipt:

```bash
make live-smoke-summary
```

Smaller multi-target receipt:

```bash
DEXTER_SMOKE_SUMMARY_TARGETS="live-smoke-action-diagnostic live-smoke-operator-status" make live-smoke-summary
```

The latest receipt is copied to:

```text
docs/live-smoke-results/latest.md
```

---

## Next Phase Candidate

The next useful phase should continue observability around local evidence:
summary artifacts and clearer refusal/action-failure explanations rather than
more policy surface area.

Recommended focus:

- Operator-facing diagnostic copy for "why didn't Dexter act?" scenarios.
- Clearer local logs for action refusal causes, Contacts resolution failures,
  worker degradation, and off-host refusal.
- Smoke summary artifacts after live suites.

The summary artifact item is implemented by `make live-smoke-summary`. It wraps
the existing live-smoke target sequence, captures per-target logs under
`docs/live-smoke-results/logs/<timestamp>/`, and writes both a timestamped
markdown receipt and `docs/live-smoke-results/latest.md`.

The operator-facing diagnostic copy item is partially implemented for audited
action failures. `Action Diagnostic` now includes a concrete `Next step` line
for failed, denied, expired, abandoned, timed-out, shell, browser,
AppleScript, and raw `message_send` receipts. Session-only refusal clues already
had next-step copy; audited receipts now have the same operator shape.

The audited-receipt cause and next-step copy now lives in
`src/rust-core/src/action_evidence.rs`. The daemon `ActionDiagnostic` path and
the `dexter-cli --why` offline fallback both call the same helper, so operator
copy cannot silently diverge between HUD and Terminal diagnostics.

`make live-smoke-summary` was run for the full live suite on 2026-05-27 and
passed:

```text
Result: PASS
Passed: 16
Failed: 0
Duration: 14m 37s
Receipt: docs/live-smoke-results/live-smoke-20260527_163303.md
```

The receipt-backed checkpoint covered recovery, degraded-mode diagnostics,
external failures, operator status, action diagnostics, natural-language CLI,
deterministic action matrix, receipts, approval lifecycle, HUD lifecycle, HUD
health, HUD action history, HUD action diagnostics, HUD approval, action
cancellation, and barge-in.

After extracting shared action-evidence copy, a focused receipt-backed pass also
passed:

```text
Result: PASS
Passed: 3
Failed: 0
Receipt: docs/live-smoke-results/live-smoke-20260527_165126.md
```

That focused pass covered CLI action diagnostics, operator status, and the real
Swift HUD action-diagnostic surface.

`dexter-cli --status` now includes a `Latest Action Summary` section before the
raw recent-action list. For successful actions it reports the target and
success evidence; for failed, denied, expired, abandoned, timed-out, shell,
browser, AppleScript, or raw `message_send` receipts it uses the shared
`action_evidence` cause and next-step wording. `make live-smoke-operator-status`
was updated to verify the summary section, and the CLI unit suite covers
successful, failed, and empty status reports.

The policy gate and primary live workflows are now covered well enough to move
from "can this path survive?" to "can the operator understand what happened?"
