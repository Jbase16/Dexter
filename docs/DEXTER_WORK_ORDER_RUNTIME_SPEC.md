# Dexter Work Order Runtime — Design Specification

## Status

Proposed design checkpoint for **DEX-03: language-to-outcome reliability**.

Revised after pre-implementation review on 2026-08-06. Slice A remains on hold
until the two independent lifecycle repairs described below are completed and
the revised design is accepted.

This document does not authorize implementation and does not change production
behavior. It defines the concrete system that must be reviewed before code is
added. Daily-driver v1 remains blocked and the current manual checklist must not
be attested.

## Problem

Dexter already has capable, policy-gated action executors. The missing layer is
the reliable conversion of ordinary operator language into a completed,
verified job.

The 2026-08-06 manual acceptance run demonstrated the gap:

1. A browser request routed to PRIMARY but emitted AppleScript, required an
   unnecessary approval, opened the page, omitted the requested title, and
   announced completion.
2. Two Finder-focus requests produced success claims without dispatching an
   action or creating a receipt.
3. A local missing-control request was treated as factual uncertainty and
   turned into an irrelevant outbound-retrieval refusal.
4. Browser-worker restart succeeded in the daemon but produced no perceptible
   operator feedback.
5. New Session remained indefinitely on `Starting a fresh session...` because
   the HUD has no completion transition tied to a newly established session.

Failures 1-3 require DEX-03 because they occur before or outside the typed
action system. Failures 4-5 are ordinary LLM-free lifecycle defects. They are
release blockers, but they do not need the work-order runtime and must be fixed
independently before Slice A. They remain acceptance cases because the later
HUD projection should reuse their proven terminal-state pattern.

The current automated action smokes start with an already-correct `ActionSpec`.
They prove execution, policy, receipts, and selected HUD surfaces, but they do
not prove that natural language reliably reaches those systems. Synthetic
action coverage remains useful component coverage; it is not sufficient
operator-path acceptance evidence.

## Plain-Language Design Decision

Every operator turn is handled as either:

- a **question**, which requires an evidence-grounded answer; or
- a **work order**, which remains open until the requested real-world result is
  verified, delivered, failed, or cancelled.

The language model may propose the meaning and steps of a work order. It may
not mark obligations complete. Only Rust-owned evidence can change obligation
state or authorize a completion claim.

The defining rule is:

> Dexter does not finish an actionable turn when the model stops generating or
> when one action returns. Dexter finishes when every required outcome has
> current evidence and every requested result has been delivered.

## Goals

1. Route ordinary language to Dexter's existing capabilities without relying
   on exact trigger phrases.
2. Preserve compound requests across multiple actions and model turns.
3. Prevent claims about actions or machine state without matching evidence.
4. Verify the relevant environment after actions whose success is observable.
5. Keep local UI requests local; uncertainty must not silently change reach.
6. Give the HUD one authoritative pending/success/failure state for each job.
7. Treat operator corrections as evidence that reopens disputed work.
8. Retain the existing action policy as the final authority over side effects.
9. Create a safe foundation for later operator-specific learned routines.

## Non-Goals

- Replacing Ollama or selecting a new foundation model.
- Making every open-ended conversational answer perfect.
- Hard-coding the five failed prompts or application names.
- Letting the model write Rust, change policy, or silently install capabilities.
- Replacing `ActionSpec`, the action engine, the audit log, or DEX-01 policy.
- Treating an action receipt by itself as proof of every requested outcome.
- Building self-learning before the work-order lifecycle is proven.

## Existing Foundations to Reuse

The design wraps existing systems rather than rebuilding them:

- `ActionSpec` remains the typed action vocabulary.
- `ActionOutcome` and the audit log remain execution evidence.
- `PolicyEngine::evaluate` remains the sole production policy decision.
- `ContextSnapshot` remains an input for current app, focused element, visible
  windows, clipboard, and recent shell state.
- Browser-worker results remain authoritative for browser URL, page state, and
  extracted content.
- Health and RestartComponent RPC responses remain authoritative for worker
  lifecycle state.
- The Swift HUD remains a view and input surface; Rust owns work-order truth.

## Module Boundary

The work-order runtime gets its own module alongside `action/`:

```text
src/rust-core/src/work_order/
  mod.rs
  types.rs
  evidence.rs
  matcher.rs
  tracker.rs
  render.rs
```

`orchestrator.rs` owns only the integration seam: create or delegate a turn,
forward action and observation events, and emit the resulting operator message.
Work-order types, transition rules, matching, evidence evaluation, and rendering
must not be implemented in the existing 20,000-line orchestrator.

## Core Records

The following shapes describe required information, not final Rust syntax.

### WorkOrder

```text
WorkOrder
  id
  session_id
  source_turn_id
  source_text_fingerprint
  created_at
  deadline
  kind: question | action | lifecycle
  goal
  scope: local_ui | browser | filesystem | process | external | internal
  status: proposed | active | awaiting_approval | verifying |
          succeeded | failed | cancelled | timed_out
  obligations[]
  attempts[]
  correction_generation
  final_delivery_evidence
```

The source text itself stays in the existing live conversation. Persistent
work-order records use the existing secret-safe fingerprinting and redaction
rules.

### Obligation

```text
Obligation
  id
  description
  kind: effect | observation | requested_output | operator_delivery |
        lifecycle_transition
  dependencies[]
  acceptable_evidence[]
  freshness_requirement
  status: pending | satisfied | failed | cancelled
  evidence_refs[]
```

Examples:

- `effect`: browser navigated to the requested URL.
- `observation`: title was read from that page after navigation.
- `requested_output`: the observed title is available to the response path.
- `operator_delivery`: the title was placed in the HUD response.
- `lifecycle_transition`: a new session ID became ready.

### EvidenceRef

```text
EvidenceRef
  source: action_receipt | context_snapshot | browser_result |
          health_snapshot | session_event | operator_correction
  source_id
  observed_at
  fact
  freshness
  security_label
```

Evidence is immutable. A newer contradictory fact supersedes it for current
state but does not rewrite history.

### CapabilityDescriptor

Each existing capability exposes a small Rust-owned descriptor:

```text
CapabilityDescriptor
  id
  plain_language_purpose
  required_inputs
  possible_outputs
  expected_effects
  verification_sources
  reach
  risk properties
  preconditions
  estimated latency
  fallback rank
```

Descriptors are derived from implemented capabilities. Model-generated text
cannot invent or widen them.

## Turn Entry: Question or Work Order

The current `direct_category=Chat` decision is not sufficient because clear
imperatives can remain chat and produce unsupported claims.

Turn entry uses a three-stage budget so ordinary chat does not always pay for
an embedding and a model call:

1. **In-process descriptor floor.** Tokenized capability descriptions and
   current local entities provide a cheap candidate score. Clearly unrelated
   turns stay in chat without embedding or classification.
2. **Semantic candidate match.** Only candidate turns request a query embedding
   and compare it with cached capability-description embeddings. High scores
   enter work-order mode; low scores return to chat.
3. **Ambiguity classifier.** Only the uncertain middle band invokes FAST
   (`qwen3:8b`) with a constrained output of `question`, `work_order`, or
   `clarification_required` and only known capability IDs.

The ambiguity classifier receives the current turn and the small relevant
descriptor set. It does not receive the full personality, retrieval memory, or
conversation history and never routes to PRIMARY merely to choose turn mode.

Every evaluated turn records which stage made the final entry decision and the
time spent in each visited stage. Benchmark reports separate at least these
cohorts: obvious chat, state-check questions, routine actions, compound actions,
and intentionally ambiguous turns. The report must include the percentage of
turns terminating at Stage 1, Stage 2, and Stage 3 for each cohort and overall;
an aggregate latency percentile without this distribution is insufficient.

Rules:

- A high-confidence capability match forces work-order mode. Free-form chat
  cannot terminate the turn.
- A compound request records every requested effect and output.
- Missing required arguments produce one targeted clarification.
- Local app/window/control references bind the work order to local scope.
  They cannot authorize or trigger web retrieval.
- Explicit online research remains governed by existing retrieval
  authorization and DEX-01 policy.
- Low-confidence input may remain a question, but any later claim that an
  action occurred still cannot reach a work-order success surface.

This is descriptor-driven semantic matching, not a list of exact phrases.

## Planning and Capability Choice

The model may propose a plan using known capability IDs. Rust validates:

- every capability exists;
- inputs are present and typed;
- dependencies can produce the required outputs;
- scope does not widen without operator authority;
- requested outputs have a producer;
- success has a verification source;
- policy will evaluate every action normally.

When multiple capabilities could make progress, Dexter prefers the plan with:

- more progress toward unsatisfied obligations;
- stronger expected evidence;
- lower risk and approval friction;
- lower latency;
- greater reversibility;
- fewer unknown effects.

This makes a specific browser action preferable to arbitrary AppleScript when
the browser worker is healthy and the operator requested browser evidence.
AppleScript remains available for jobs that genuinely require it.

## Residual Risk: Wrong Work-Order Meaning

Rust can prove that a proposed work order is structurally executable. It cannot
prove that the obligations capture everything the operator meant. A model that
reduces `open apple.com and tell me the title` to navigation-only could produce
a perfectly verified but incomplete work order.

DEX-03 reduces but does not claim to eliminate this semantic-alignment risk:

- every requested action clause and requested-result clause must be linked to
  at least one obligation before a proposal validates;
- the immediate HUD acknowledgement states the interpreted goal and current
  step in plain language so the operator can correct it early;
- compound-request, paraphrase, omission, and correction cases are mandatory
  operator-path tests;
- operator corrections reopen the order and become evaluation evidence;
- no learned routine may be promoted from a trace whose goal was corrected.

The remaining risk and observed omission rate must be reported at the Slice B
checkpoint. Structural evidence does not make a misinterpreted goal correct.

## Execution and Verification Loop

For an active work order:

1. Select the next obligation whose dependencies are satisfied.
2. Select and policy-check a capability that can advance it.
3. Execute the exact approved `ActionSpec` through the existing action engine.
4. Attach the resulting receipt to the attempt.
5. Poll the required observation source until the postcondition matches or the
   verification deadline expires.
6. Satisfy the obligation only if its evidence rule passes.
7. Continue, clarify, replan, or enter a terminal failure.

Limits:

- At most two automatic attempts for the same obligation and target unless the
  operator explicitly asks to continue.
- Do not repeat an identical failed action fingerprint.
- Every work order has a deadline.
- Approval waiting is visible and pauses the deadline calculation.
- Cancellation ends the order and aborts in-flight work through existing
  cancellation mechanisms.
- Verification polling starts at 100 ms and backs off to at most 500 ms until
  the obligation deadline. Polls are observations, not new action attempts.
- Asynchronous macOS state such as window focus cannot fail from one immediate
  stale snapshot; failure requires the bounded poll to expire.

## Structural Completion Surface

DEX-03 does not parse free-form prose to detect success claims. Regexes would
miss paraphrases and a second model would add another probabilistic authority.
Instead, an active work order removes the path from raw model prose to the HUD
and TTS success surfaces.

The model can emit only internal plan proposals while the order is active.
Operator-visible work-order messages are Rust-rendered from typed state:

- acknowledgement from the validated goal;
- current step from the active obligation;
- approval copy from the existing policy decision;
- failure copy from typed action or verification evidence;
- success copy from satisfied obligations;
- requested values from evidence payloads.

TTS receives the same Rust-rendered operator message as the HUD. It never speaks
raw model completion prose for an active work order.

Slice B does not include optional model-written explanatory addenda. Such prose
may be considered later only through a separate evidence-bound summarization
design; it is not needed to prove the runtime.

Required behavior:

- `Finder is now frontmost` requires a current fact naming Finder as frontmost.
- `I opened Apple.com` requires matching navigation evidence.
- `The title is Apple` requires a title observation from the resulting page.
- `Done` or `Action complete` requires every required obligation to be
  satisfied.
- A pending approval, successful intermediate action, or model assertion is
  not completion evidence.

Because completion copy is constructed from state, unsupported model text is
never filtered or rewritten; it has no operator-visible success path. If no
valid plan remains, Rust renders the precise incomplete or failed obligation.

## Operator Corrections

A correction such as `No, it isn't` is matched to the most recent disputed
claim and work order within a 120-second dispute window. Active orders are
always eligible. A terminal order older than the window is not silently
reopened; Dexter asks which result the operator is disputing.

When matched:

1. Record the operator correction as high-authority evidence.
2. Invalidate the disputed completion proof for current state.
3. Reopen the affected obligation.
4. Force a fresh observation before another completion claim.
5. Prevent repetition of the same plan unless new evidence justifies it.
6. Preserve the correction trace for later evaluation and possible routine
   learning.

If a correction cannot be matched confidently, Dexter asks which result was
wrong. It does not treat the correction as unrelated chat.

## HUD Contract

The HUD renders Rust-owned work-order state rather than inferring state from
model text.

For an actionable turn it shows, in operator-friendly language:

- what Dexter is trying to accomplish;
- the current step;
- whether approval is required;
- success, failure, cancellation, or timeout;
- the final requested result;
- `Why` evidence when a step fails or is denied.

The default surface shows only the interpreted goal, current step, and final
result. The complete obligation checklist and evidence are available through
the existing Why affordance rather than occupying the normal HUD.

The HUD must not clear while required obligations remain pending. It may
collapse after terminal success only after the requested result has been
displayed. Terminal failures remain inspectable through Why and recent actions.

Lifecycle controls use explicit state rules without involving the language
model. The immediate repairs ship before the work-order runtime; a later HUD
integration may project their already-proven states through the same renderer:

- Browser restart is complete only after the restart RPC succeeds and a
  post-restart health snapshot reports Browser ready.
- New Session is complete only after the old stream ends and a different
  session ID reaches ready.
- Intermediate messages have bounded deadlines and always transition to
  success or an actionable error.

## Required Behavior for the Five Founding Failures

### Browser navigation plus requested title

Input family:

- `Open apple.com in the browser and tell me the page title.`
- `Go to Wikipedia and read me the title.`
- `Load example.com; what is the title of the page?`

Required work order:

1. Navigate with the browser worker.
2. Verify the resulting URL.
3. observe the title from that resulting page.
4. deliver the observed title.

Pass:

- no unnecessary AppleScript approval while Browser is ready;
- no completion before the title is observed and displayed;
- receipt and final answer refer to the same page;
- immediate HUD acknowledgement without waiting for model prose.

### Focus a window

Input family:

- `Focus Finder.`
- `Bring Finder forward.`
- `Put Safari in front.`

Pass:

- dispatches `WindowFocus` with a resolved application identity;
- obtains a fresh post-action frontmost-app observation;
- claims success only when the observation matches;
- produces a structured failure otherwise.

### Missing local control

Input family:

- `Click the Missing Control button in Finder.`
- `Press a button called Does Not Exist in Xcode.`

Pass:

- remains local UI work;
- inspects or targets the correct app;
- emits a `UiClick` success or structured local failure receipt;
- never produces an online-search refusal;
- HUD Why and `make why` explain the same failure.

### Restart Browser

Pass:

- HUD acknowledges the click immediately;
- shows a visible restarting state;
- displays the post-restart ready or failed result;
- does not churn Ollama models;
- never silently returns to an indistinguishable status view.

### New Session

Pass:

- HUD acknowledges the request immediately;
- observes the old session closing;
- observes a different session ID becoming ready;
- clears `Starting a fresh session...` on success;
- shows an actionable error on timeout;
- does not restart workers or Ollama.

## Performance Targets

These are product targets, not claims about current behavior:

- HUD acknowledges a work order or lifecycle operation within 500 ms.
- An obvious non-work chat turn adds no embedding or model call and adds no more
  than 5 ms p50 / 20 ms p95 turn-entry latency on the release machine.
- Across a representative mixed chat benchmark, turn-entry adds no more than
  25 ms p50 / 150 ms p95.
- At least 95% of the fixed obvious-chat cohort terminates at Stage 1, and no
  more than 2% reaches Stage 3.
- The fixed mixed-workload corpus reports its composition and complete stage
  distribution. Stage 3 should handle no more than 15% of that published mix;
  changing the mix to improve the percentage invalidates the comparison.
- An ambiguity-classifier invocation uses FAST only and targets 1 second p50 /
  2.5 seconds p95 while warm.
- High-confidence routine capability binding does not invoke the full PRIMARY
  prompt.
- A warm, reachable browser navigate-and-title job normally completes within
  15 seconds.
- Window focus normally verifies within 5 seconds.
- Worker restart and New Session transition to success or failure within 10
  seconds unless the underlying operation explicitly reports a longer bound.

Timeouts produce visible terminal errors; they never leave indefinite loading
copy.

## Evaluation and Release Evidence

Natural-language operator-path evaluation is required in addition to existing
synthetic `ActionSpec` tests.

Each capability family must include:

- at least five meaning-equivalent paraphrases;
- compound requests with required returned information;
- a missing-target failure;
- an operator correction after a false or stale observation;
- verification that no unsupported success claim reached the HUD;
- verification that receipts and final claims reference the same work order;
- warm-path latency evidence.

Turn-entry evaluation must report:

- the Stage 1 / Stage 2 / Stage 3 termination distribution by benchmark cohort;
- **entry false-negative rate**: actionable turns that incorrectly exit into
  ordinary chat divided by all actionable turns;
- the corresponding false-positive rate so recall is not improved by forcing
  nearly every turn into work-order mode;
- the exact benchmark composition and raw counts behind each percentage.

Entry false-negative rate must be 0% on the founding-failure paraphrase corpus
and no more than 1% on the expanded implemented-capability corpus at the Slice
B checkpoint. These thresholds are design acceptance targets, not claims about
current behavior.

The gate must label synthetic action coverage as component evidence and
natural-language work-order coverage as operator-path evidence. Daily-driver
v1 cannot pass manual attestation when either class is missing or failed.

## Implementation Slices

### Immediate lifecycle repairs — before Slice A

These are not DEX-03 architecture slices:

1. Browser Restart must visibly show requested, restarting, and terminal
   ready/failed states from the existing RestartComponent response and health
   snapshot.
2. New Session must wait for a different ready session ID, then replace the
   loading state; timeout must render an actionable error.

Each repair lands with its own focused Swift/live HUD test. Daily-driver manual
items 10 and 11 are rerun after these repairs without waiting for DEX-03.

### Slice A — Work-order types and shadow trace

- Add Rust-owned WorkOrder, Obligation, EvidenceRef, and state transitions.
- Adapt existing receipts, context, health, browser results, and session events
  into evidence references.
- Produce a secret-safe shadow trace without changing execution.
- Replay founding language failures 1-3 and their paraphrases. Lifecycle
  failures 4-5 remain independent acceptance fixtures.

Slice A is structural evidence only. It is not a user-visible fix and must not
be described as one.

### Slice B — Turn entry and completion guard

- Add descriptor-driven question/work-order entry.
- Validate constrained work-order proposals against known capabilities.
- Remove raw model prose from active-work-order HUD and TTS success paths.
- Render acknowledgement, current step, terminal status, and requested values
  from typed work-order state and evidence.
- Replan or report incomplete work instead of silently stripping claims.

Slice B is explicitly cross-process work, not a Rust-only guard:

- extend `dexter.proto` with typed work-order state frames and Rust-rendered
  operator messages;
- regenerate and compile both Rust and Swift bindings;
- update `DexterClient` to keep normal chat token streaming unchanged while
  routing active work orders through typed state frames;
- update `HUDWindow` to render acknowledgement, current step, and terminal
  result from those frames;
- route TTS from the same Rust-rendered operator message;
- cover proto consistency plus focused Rust and real Swift HUD state-rendering
  tests.

Any Slice B estimate or plan must include the proto and Swift work. The first
user-visible checkpoint is not complete when only the Rust guard exists.

Slice B is the first user-visible checkpoint. Do not expand the architecture
unless it prevents unsupported Finder success claims and keeps compound browser
work open until required output exists.

### Slice C — Browser compound jobs

- Link navigation, resulting-page observation, extraction, and delivery.
- Prefer the browser worker over dominated general-purpose actions.
- Add natural-language browser acceptance coverage.

### Slice D — Window and local UI jobs

- Resolve application identity from current local state.
- Require post-action window verification.
- Keep local control requests out of outbound uncertainty retrieval.
- Add natural-language window/UI success and failure coverage.

### Slice E — Unified HUD projection

- Reuse the proven Browser Restart and New Session terminal-state presentation
  pattern for normal work orders.
- Render progress and final outcomes from typed state.
- Add cross-surface HUD/TTS consistency coverage.

### Slice F — Validated operator-specific routines

Only after Slices A-E pass real operator-path acceptance:

- abstract successful work-order traces into declarative routine candidates;
- test paraphrases, counterexamples, policy, and postcondition evidence;
- require explicit versioned promotion;
- keep all executions subject to current policy and verification.

No executable self-modification is allowed.

## Stop/Go Checkpoint

After Slice B, stop and evaluate the founding failures before investing in the
remaining system.

Continue only if the implementation demonstrates all of the following:

1. An action request cannot terminate as unsupported conversational success.
2. A compound browser job remains open until its requested observation is
   delivered.
3. Paraphrases map to the same capability and evidence requirements without
   exact-string rules.
4. Existing DEX-01 policy and action fingerprints remain authoritative.
5. Added latency is lower than the avoided full-prompt path for routine work.
6. Obvious chat stays within the stated 5 ms p50 / 20 ms p95 entry budget, and
   the mixed-chat benchmark stays within its stated ceiling.
7. The report includes Stage 1 / Stage 2 / Stage 3 termination percentages by
   cohort; at least 95% of obvious chat ends at Stage 1, no more than 2% of it
   reaches Stage 3, and no more than 15% of the fixed mixed workload reaches
   Stage 3.
8. Entry false-negative rate is 0% on the founding paraphrase corpus and no
   more than 1% on the expanded implemented-capability corpus, with raw counts
   and false-positive rate reported alongside it.

If those conditions are not met, do not add more terminology or subsystems.
Revise or abandon the design based on the recorded evidence.

## Resolved Review Decisions

1. Completion is structural: Rust renders work-order HUD and TTS messages; it
   does not parse model prose for implied claims.
2. Ordinary chat has a no-embedding/no-model short circuit and an explicit
   latency budget.
3. Wrong obligation decomposition remains a measured residual risk rather than
   being mislabeled as solved by structural validation.
4. Browser Restart and New Session are independent immediate repairs, not held
   behind DEX-03.
5. Corrections use a 120-second dispute window.
6. Post-action state verification polls until a bounded deadline.
7. The default HUD shows the current step and final result; the full checklist
   is available through Why.
8. The runtime lives in `work_order/`; `orchestrator.rs` retains only the
   integration seam.
