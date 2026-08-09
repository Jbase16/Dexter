# Dexter Game Plan — From Verified Runtime to Daily Driver

## Status

Proposed roadmap, drafted 2026-08-08 after the DEX-03 Slice A code review.
Revised the same day after implementer cross-review. Tier 0 as written here is
the formally accepted design; the provisional label-aware trace edit is to be
refined into the split-facts model below, not reverted.

This document is a companion to `DEXTER_WORK_ORDER_RUNTIME_SPEC.md`. It does
not replace that specification, change its slice definitions, or authorize
implementation beyond Tier 0. It exists to answer one question: what work, in
what order, makes Dexter a trusted daily driver rather than a well-tested
collection of components.

## Premise

Dexter's differentiator is not intelligence. Local models will always trail
the frontier. The thing Dexter can be that nothing else is: an entity that
finishes what the operator says, proves what it claims, and learns the
operator's machine — entirely on local hardware.

Every tier below serves a single metric:

> How large a task can the operator hand to Dexter and walk away, trusting
> whatever Dexter reports when they return?

The year of development is validated the day that trust holds for seven
consecutive days of real use. The attestation definition is at the end of
this document.

## How Tiers Map to DEX-03 Slices

Tiers and slices are two views of the same work. DEX-03 slices are
implementation units of the work-order runtime; tiers are the value ordering
across the whole project, wrapping the slices and attaching work that is not
part of the runtime (lifecycle repairs, Slice A corrections, the provenance
ledger, latency targets, attestation). Tiers never reorder slices.

| Order | Work                                             | Tier | Slice |
|-------|--------------------------------------------------|------|-------|
| 1     | Tier 0 corrections, then commit                  | 0    | completes A |
| 2     | New Session + Browser Restart repairs; items 10-11 | 1  | none (outside DEX-03) |
| 3     | Turn entry, render gate, descriptors → stop/go   | 1    | B     |
| 4     | Browser compound + window/UI jobs, reflex targets | 2   | C + D |
| 5     | HUD projection + provenance ledger + Why queries | 3    | E (+ ledger) |
| 6     | Learned routines                                 | 4    | F     |

Tier 0 is not scope beside Slice A; it is the finishing of Slice A. The
commit is the boundary. The spec originally ordered the lifecycle repairs
before Slice A; Slice A was implemented first, which cost nothing because it
is shadow-only, but the repairs still precede Slice B for their original
reasons: independent release blockers, and the proven terminal-state pattern
Slice E generalizes. Everything after the Slice B stop/go checkpoint is
conditional on that checkpoint passing.

## Tier 0 — Foundation Honesty

Scope: inside `src/rust-core/src/work_order/` only. No production surface.
These are corrections to Slice A found in review, and they are load-bearing:
each one becomes a migration instead of an edit once Slice B has live callers.

1. **Split mixed-sensitivity evidence into separate facts.** A per-fact label
   cannot honestly serve a fact that mixes `success` with `page_url`: any
   single label either hides the diagnostic bit or leaks the private one.
   Fingerprinting low-entropy values (`true`, bundle IDs) is also
   dictionary-reversible and protects nothing. Each adapter therefore emits
   facts homogeneous in sensitivity: browser results produce a `Public`
   execution-status fact (`{action_id}:status`) and an `OperatorPrivate`
   page-metadata fact (`{action_id}:page`), with namespaced source IDs so
   obligations and journal dedup address them individually. Projection by
   label: `Public` cleartext; `OperatorPrivate` keeps key structure with
   fingerprinted values (supports equality correlation without disclosure);
   `Sensitive` fully fingerprinted. Canary tests pin the boundary.
2. **Remove `EvidenceFreshness`.** Every adapter hardcodes `Current`; the
   field carries no information. Staleness is a relation between evidence and
   a question, not a property of evidence. `FreshnessRequirement` becomes
   `Any | ObservedAfter(DateTime<Utc>)`, and post-action verification is
   expressed as `ObservedAfter(attempt.dispatched_at)`.
3. **Order-level evidence journal plus one satisfying proof per obligation.**
   `Obligation.evidence_refs` is a `Vec` that can never hold more than one
   element, and the duplicate-evidence check in `try_satisfy` is unreachable.
   Decision: an immutable, append-only `evidence_journal` on `WorkOrder` is
   the single entry point for all evidence (dedup by source, source ID, and
   observation time lives here; evidence arriving before its obligation is
   satisfiable is retained, not dropped). Each obligation holds a
   `satisfying_evidence` reference naming the one journal entry that proves
   it. Supersession and corrections append and re-point; history is never
   rewritten. This honors the spec's immutability rule without building a
   corroboration engine.
4. **Make `Attempt` real.** Validated constructor matching the module's
   invariant style, with `proposed_at` / `dispatched_at` / `completed_at`
   timestamps, plus `WorkOrder::record_attempt` enforcing the spec's limits
   now: at most two automatic attempts per obligation and target, and no
   repeat of an identical failed action fingerprint order-wide within the
   same correction generation. `dispatched_at` is the anchor for
   `ObservedAfter` verification. These rules become tested invariants before
   the execution loop exists.
5. **Hygiene.** `#[cfg_attr(not(test), allow(dead_code))]` with Slice B
   pointers on the not-yet-called state machine (production build must return
   to zero warnings); `OnceLock` replaces `Mutex<Option<ShadowTracker>>` in
   `BrowserCoordinator`; the action-receipt adapter takes the three fields it
   uses instead of a fabricated `ActionAuditReceipt`; a `Fingerprint` newtype
   replaces the 64-hex string sniff in `normalized_fingerprint`; add
   `Proposed → Failed` to the transition table (construction-invalid
   proposals never become orders; validated-then-rejected proposals fail as
   orders so the ledger and HUD retain the record); collapse the duplicate
   resume methods; the trace store holds its file handle and running length
   instead of re-statting per append.

Tier 0 completion: all changes land with focused tests, `cargo build`
produces zero warnings, and the full suite passes.

## Tier 1 — The Trust Spine

Scope: the two independent lifecycle repairs, then DEX-03 Slice B as
specified. Sequencing is already fixed: New Session, Browser Restart, rerun
manual items 10 and 11, then Slice B.

One Slice B mechanism is elevated from implication to requirement, because it
is the property the entire system stands on:

**Success copy requires proof-bearing state in its type signature.**

```rust
// work_order/render.rs — the ONLY functions that produce success output
fn render_success(order: &SucceededWorkOrder) -> OperatorMessage;
fn render_progress(order: &WorkOrder) -> OperatorMessage;
fn render_failure(order: &WorkOrder, why: &EvidenceRef) -> OperatorMessage;
```

`SucceededWorkOrder` is itself the proof: it cannot be constructed without a
consumed, validated `CompletionProof`, and it cannot be deserialized into
existence. Requiring the proof again alongside it would be redundant — the
proof was consumed by `succeed()` and its guarantee now lives in the type.

The HUD and TTS pipelines accept `OperatorMessage` values only. No function
anywhere in the codebase has a signature that converts model prose into a
success surface. "Dexter cannot claim what it did not do" stops being a
policy and becomes a compile error.

Additional Tier 1 requirements:

- **Descriptor drift-proofing.** An exhaustive
  `fn descriptor_for(spec: &ActionSpec) -> CapabilityDescriptor` match:
  adding an `ActionSpec` variant without a descriptor becomes a compile
  error, not a test failure. Capability and descriptor cannot diverge.
- **Stage-distribution metrics.** Turn entry reports what fraction of turns
  resolve at stage 1 / 2 / 3 alongside the latency budgets. The fast path
  must be the common path, not just a possible one.
- Acceptance is the DEX-03 Slice B stop/go checkpoint, unchanged.

## Tier 2 — The Reflex Path

The work-order runtime makes verification independent of model claims. That
permits an architectural inversion: for high-confidence routine commands, the
language model leaves the loop entirely.

Happy path for a routine command:

1. Stage-1 descriptor match (in-process, microseconds).
2. Argument binding from `ContextSnapshot` entities and descriptor inputs.
3. Normal `PolicyEngine::evaluate`.
4. Dispatch the `ActionSpec`.
5. Bounded post-action observation poll.
6. `render_success(proof)`.

No embedding, no generation, no prose. Targets: **typed-command-to-verified-
result under 500 ms** for the top capability families — window focus, app
launch, browser navigation, volume/media, clipboard. The voice ceiling is
set from measured pipeline latency (capture, endpointing, STT, dispatch,
observation) after the typed path lands — not promised in advance. The model
remains the path for ambiguity, compound requests, and anything novel.

Guardrails: policy evaluates every action exactly as today; evidence still
gates completion; any binding ambiguity falls back to the model path. The
reflex path is a fast lane through the same checkpoints, never a bypass.
Stage-1 matching stays descriptor-owned with paraphrase evaluation — a
growing pile of exact-string intercepts is the trigger-phrase treadmill this
architecture exists to escape, and it is the most tempting shortcut here.

This is Slice C/D scope but is named as its own tier because it changes what
Dexter feels like: the difference between a chatbot with tools and an entity
sharing the machine.

## Tier 3 — The Provenance Dividend

With the label-aware trace (Tier 0) and evidence-bound orders (Tier 1),
Dexter can answer questions about its own past actions from typed evidence
instead of model memory:

- "Why did that fail?" renders the failing obligation and its evidence.
- "What did you do while I was out?" renders recent work orders and outcomes.

Implementation requires separating three stores with different lifetimes:

- **Shadow trace** — bounded rotating diagnostics (exists; 8 MiB, days of
  horizon). Development and incident forensics only.
- **Provenance ledger** — durable typed work-order outcomes with explicit
  retention, in SQLite alongside the existing stores. This is what answers
  operator questions about past work.
- **Action audit** — the existing immutable privileged-action history,
  unchanged.

The Why surface queries the provenance ledger with deterministic rendering,
routed through the existing `OperatorDiagnostic` category and `Why`
affordance. The answer path for action history never touches generation.
Zero hallucinated history, structurally.

This tier is small and disproportionately trust-building: it is how Dexter
demonstrates the honesty the architecture enforces.

## Tier 4 — Operator Routines (the Moat)

DEX-03 Slice F, entered only after Tiers 0–3 pass attestation.

Successful work-order traces are, by construction, the training data for
operator-specific routines: goal fingerprint, obligation set, capability
sequence, verified outcome. The pipeline:

1. Abstract successful traces into declarative routine candidates.
2. Test candidates against paraphrases, counterexamples, policy, and
   postcondition evidence.
3. Promote only explicitly, with versioning.
4. Execute promoted routines at reflex-path speed, still subject to policy
   and evidence verification.

Declarative templates only. No executable self-modification. Policy remains
sovereign.

After a month of daily driving, the operator's recurring intents execute at
reflex speed because Dexter learned them from evidence of its own verified
successes. No cloud product can ship this: it requires the operator's
machine, history, and trust boundary. This is the payoff that makes an
always-on local entity categorically different from a subscription chatbot.

## Refusals

The discipline that protects the year:

- **No model-stack changes** until the trust spine ships. The current stack
  is sufficient for every tier above. Model chasing is the most seductive
  form of procrastination available to this project.
- **No new context sources** until work orders consume the existing ones as
  evidence.
- **No chat polish.** Conversation quality is a DEX-03 non-goal; it stays one.
- **A shrink-only budget for `orchestrator.rs`.** Every slice must hold or
  reduce its line count. Seams move out; nothing moves in.

## Attestation — Definition of Done

Daily-driver v1 attestation, in operator terms:

> Seven consecutive days of real use in which every actionable request either
> verifiably completed or produced an honest, inspectable failure — zero
> unsupported success claims, zero indefinite loading states, and the reflex
> path under 500 ms.

All four conditions are measurable from the shadow trace and work-order
records built in Slice A and hardened in Tier 0. The day this attestation
passes is the day the year was worth it — not because the architecture is
elegant, but because the operator stopped having to check Dexter's work.
