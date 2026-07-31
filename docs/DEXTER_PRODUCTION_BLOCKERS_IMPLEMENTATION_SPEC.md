# Dexter Production Blockers — Implementation Specification

## Status

Proposed implementation plan for the two blocking findings from the July 2026
production-readiness review:

- **DEX-01:** model-proposed actions can disclose local data over an
  unapproved external channel.
- **DEX-02:** the Daily-driver v1 gate can report PASS from evidence that is
  older than the source, binaries, configuration, models, or toolchains it is
  meant to validate.

This document defines the implementation contract. It does not claim either
blocker is fixed.

## Implementation Progress

- **2026-07-29 — DEX-01 Slice A complete:** structured policy types, versioned
  reason codes, conservative per-lane shadow evaluation, BLAKE3 action
  fingerprints, and non-enforcing comparison telemetry are implemented.
- The focused policy suite covers every current `ActionSpec` variant,
  restricted reads through structured file, shell, and browser-file lanes,
  loopback/external reach, deterministic intent, and fingerprint-bound
  one-shot approval.
- At the Slice A checkpoint, evaluation was intentionally shadow-only; Slice B
  below promotes the structured approval decision to enforcement.
- Verification baseline after Slice A: 882 Rust tests passed, 7 ignored.
- **2026-07-29 — DEX-01 Slice B complete:** structured
  `approval_required` now governs both the pending-approval path and the
  background-dispatch path. Model-proposed external shell/browser actions,
  unknown shell reach, arbitrary AppleScript, Shortcuts, messaging, and
  unknown external UI/browser mutations fail closed to operator approval.
- Browser policy consumes the last URL reported by the browser worker rather
  than model text. Exact external browser navigation can avoid redundant
  approval only when Rust finds the identical literal URL in the current
  depth-zero operator turn.
- Pending approvals retain the exact BLAKE3 action fingerprint and stored spec.
  Approval re-evaluates the stored action under a one-shot
  `OperatorApproved` origin and refuses execution if the fingerprint changes.
  Browser mutations also bind the worker-observed origin, so approval does not
  transfer to a different page origin while the dialog is pending.
- New audit rows carry versioned policy fields, reason codes, reach,
  sensitivity, reversibility, origin, fingerprint, and a redacted destination.
  Approval copy omits URL query strings and arbitrary shell arguments.
- The initial restricted-path seed is also enforcing for structured file reads
  and the covered shell/browser-file read forms. Slice C remains required for
  complete normalized source labeling and model-context taint propagation.
- DEX-01 remains open: core retrieval and coarse model-context sensitivity are
  not yet routed through the enforcing policy, and the legacy classifier has
  not been removed.
- Verification baseline after Slice B: 892 Rust tests passed, 7 ignored; the
  release build and live-smoke script syntax check are clean.
- The 2026-07-30 action-matrix live smoke passed end to end after Playwright
  Chromium was installed, including external-curl redaction, shell/file and
  AppleScript approval paths, routine browser navigate/type/extract, and
  denied/approved consequential browser mutations.
- **2026-07-30 — DEX-01 Slice C complete:** every in-memory message sent to
  Ollama now carries Rust-owned sensitivity and trust metadata that is excluded
  from wire serialization. Each `GenerationResult` retains the aggregate label
  from the exact request that produced it, and action evaluation consumes that
  immutable snapshot.
- Operator text, ambient clipboard/Accessibility/shell context, local action
  results, restricted file results, retrieval/browser content, memory, and
  model responses receive explicit source labels. Sensitivity survives model
  continuations and clears only when the labeled message leaves the assembled
  prompt.
- Sensitive paths use execution-equivalent home expansion, lexical
  normalization, and nearest-existing-parent symlink resolution. Strict
  percent-decoding covers `file:` URLs, and `[behavior].restricted_paths`
  extends the built-in credential-path patterns.
- Slice C regression coverage proves a restricted tool result taints the real
  continuation prompt, a derived external request requires approval, external
  retrieval cannot authorize a local mutation, and policy audit JSON contains
  labels/reason codes but not the test secret.
- Verification baseline after Slice C: 903 Rust tests passed, 7 ignored.
- DEX-01 remains open for Slice D retrieval controls and Slice E compatibility
  evaluation/removal of the legacy classifier.

## Outcome

Daily-driver v1 is eligible for release only when both statements are true:

1. A model-proposed action cannot reach an external destination without either
   deterministic operator authorization for the exact action or a one-shot
   approval bound to the exact normalized action.
2. The release gate proves the current source and runtime identity passed every
   required build, test, and live-smoke check in one evidence-bound run.

The first statement is deliberately broader than curl or HTTP verbs. The
security boundary is the model-to-side-effect transition, so the policy must
cover every lane capable of external disclosure or externally visible
mutation.

## Constraints Preserved

- Dexter remains macOS-only, single-user, and on-device.
- The same-user local-peer threat model does not change.
- Rust remains the sole owner of policy, privileged execution, audit,
  persistence, and authorization decisions.
- Existing protobuf and operator-facing approval vocabulary remains valid:
  `safe`, `cautious`, and `destructive` stay as compatibility categories.
- `destructive` continues to mean “approval required,” not “forbidden.”
- Model output remains untrusted. The model cannot lower a policy decision,
  declare a trusted origin, grant a capability, or mark its own data public.
- Git metadata is not required. Evidence identity is derived from content and
  runtime state, so the design works in a workspace with no `.git` directory
  and in a workspace with local modifications.
- Ollama inference traffic to the configured loopback daemon is local reach,
  not external reach.
- Raw credentials, file contents, message bodies, clipboard contents, and
  browser form values must not be copied into policy logs or release evidence.

## Non-goals

- Consumer distribution, notarization, or multi-user isolation.
- Replacing the three compatibility categories in the proto or Swift HUD.
- Treating an LLM verifier as an authorization boundary.
- Learning silent approvals from prior operator decisions.
- Proving arbitrary command behavior through an ever-growing argv keyword
  list.
- Shipping `sandbox-exec` confinement as part of these blockers. Per-action
  confinement remains a subsequent hardening and autonomy project.
- Making every action reversible or using reversibility as the sole approval
  rule.

---

# DEX-01 — External Reach and Disclosure Policy

## Existing Failure

The current policy answers primarily whether an action mutates local state:

- `ActionSpec::FileRead` is always `safe`.
- ordinary curl and wget requests are `cautious`;
- normal external browser navigation is `cautious`;
- unknown shell commands and non-obviously-destructive interpreter payloads
  are `cautious`; and
- `safe` and `cautious` actions execute immediately.

That allows a model to read local data, receive the result in model context,
and place data derived from that result into an external URL or another
outbound-capable action without approval. Shell argument-array execution
prevents metacharacter injection, but it does not prevent the model from
placing a secret directly in an argument.

The fix must cover the capability, not the first binary that demonstrated it.

## Required Policy Model

Replace the single internal classification result with a structured decision.
The existing `ActionCategory` remains a derived compatibility field.

```rust
pub(crate) struct PolicyDecision {
    pub category: ActionCategory,
    pub approval_required: bool,
    pub effect: LocalEffect,
    pub reach: Reach,
    pub sensitivity: DataSensitivity,
    pub reversibility: Reversibility,
    pub reasons: Vec<PolicyReason>,
    pub action_fingerprint: String,
}

pub(crate) enum LocalEffect {
    Observe,
    Mutate,
    Destructive,
    Unknown,
}

pub(crate) enum Reach {
    Local,
    Loopback,
    ExternalRead,
    ExternalWrite,
    Unknown,
}

pub(crate) enum DataSensitivity {
    Public,
    OperatorPrivate,
    Restricted,
    Unknown,
}

pub(crate) enum Reversibility {
    Reversible,
    Irreversible,
    Unknown,
}
```

The evaluator also receives Rust-owned context that cannot be deserialized from
the model action block:

```rust
pub(crate) struct PolicyContext {
    pub origin: ActionOrigin,
    pub visible_context_sensitivity: DataSensitivity,
    pub visible_context_trust: ContentTrust,
    pub current_browser_origin: Option<ValidatedOrigin>,
    pub operator_turn_id: String,
}

pub(crate) enum ActionOrigin {
    ModelProposed,
    DeterministicOperatorIntent,
    CoreRetrieval,
    OperatorApproved { fingerprint: String },
    SystemInternal,
}
```

`category_override` remains upward-only. It is not an origin, capability, or
authorization signal.

### Compatibility Mapping

- `safe`: known local observation, no approval, no security-significant audit
  requirement.
- `cautious`: no approval, but the action and structured policy reasons are
  audited.
- `destructive`: explicit approval is required.
- missing, malformed, or unknown policy information: `destructive`.

New Rust code must use `approval_required`, not compare category names at
multiple call sites. The category is derived once inside the policy engine.

## Authorization Sources

There are two ways an externally reaching action can run:

1. **Deterministic operator authorization.** Rust proves the current operator
   turn contains the exact destination and material payload, then constructs
   the action without accepting model-supplied destination data. Existing
   deterministic Contacts/self-send handling is the pattern.
2. **One-shot approval.** The HUD presents the external destination, action
   type, and data sensitivity. Approval is stored against the canonical action
   fingerprint, expires under the existing timeout, and authorizes one
   execution only.

Statements in personality YAML, model rationale, previous approvals, semantic
similarity, or an LLM verifier do not authorize an action.

The action fingerprint is BLAKE3 over the canonical, post-normalization
`ActionSpec` plus the reach, destination, and operator-turn identifier. Dexter
already uses BLAKE3 for stable context fingerprints, so this avoids a second
cryptographic hash dependency in the privileged action path. The fingerprint
must not contain or log raw secret material. Changing an argument, URL,
recipient, path, payload, or normalized executable produces a different
fingerprint.

## Fail-closed Decision Rules

The following rules are normative and evaluated in order:

1. A policy parse/evaluation failure requires approval.
2. A model-proposed action with `Reach::Unknown` requires approval.
3. A model-proposed external write or externally visible mutation requires
   approval.
4. A model-proposed external read requires approval unless it uses a
   purpose-built Rust-owned retrieval path authorized from the current
   operator turn and its request cannot include prior tool or model output.
5. Any model-proposed external action created while operator-private,
   restricted, or unknown-sensitivity data is visible to the proposing model
   requires approval.
6. Reading a restricted source requires approval before the contents are
   exposed to the model.
7. A deterministic action may skip a second approval only when Rust binds the
   exact destination and payload to explicit intent in the current operator
   turn.
8. Reversibility may raise a decision to approval-required when unknown or
   irreversible. It cannot lower a decision required by reach, sensitivity, or
   effect.
9. An upward model override remains effective. A downward override remains
   ignored.

Approval applies to the exact pending action. It does not create a session-wide
network grant.

## Context Sensitivity and Trust

The model cannot reliably preserve substring-level taint through summarizing,
encoding, translation, or reformatting. Dexter therefore uses coarse
model-context taint:

- Every message or tool result added to conversation context carries a
  Rust-owned `DataSensitivity` and `ContentTrust`.
- A model-proposed action is evaluated against the maximum sensitivity of all
  messages actually included in that generation request.
- Taint clears only when the labeled content is no longer included in the
  model request. Starting a new turn alone does not clear it.
- External web and browser content is `ExternalUntrusted` even when it is
  public. That label cannot grant action authority.
- Policy logs store labels, origins, hashes, and reason codes—not the labeled
  content.

Initial source labeling:

| Source | Sensitivity | Trust |
|---|---|---|
| Current operator text | `OperatorPrivate` | `Operator` |
| Public deterministic local facts | `Public` | `LocalTrusted` |
| Ordinary project/user file | `OperatorPrivate` | `LocalTrusted` |
| Credential-pattern path | `Restricted` | `LocalTrusted` |
| Clipboard or Accessibility content | `OperatorPrivate` | `LocalObserved` |
| Browser extract or web retrieval | `Public` unless configured otherwise | `ExternalUntrusted` |
| Model response | inherited maximum input sensitivity | `ModelGenerated` |

Credential-pattern classification must at minimum cover private keys, SSH
configuration with secrets, cloud credential stores, `.env`/secret files,
browser credential/profile stores, keychains, token files, and operator-defined
restricted paths. Paths are normalized with the same expansion and symlink
resolution rules used by execution before classification.

The restricted-path list is defense in depth, not the only exfiltration
control. Unknown shell and external reach still fail closed when a sensitive
file name is not recognized.

## Lane Requirements

### Shell

- Keep a small allowlist of commands whose implemented argument forms are
  proven local-only.
- `curl`, `wget`, `ssh`, `scp`, `sftp`, `rsync` with a remote endpoint, `nc`,
  network clients, interpreters, `env`/`exec` wrappers, and arbitrary unknown
  executables are external or unknown reach and require approval when
  model-proposed.
- A GET is external disclosure-capable. HTTP method does not lower reach.
- `make`, project scripts, compilers with plugins, package managers, and other
  extensible tools remain unknown reach until confinement or a narrow
  argument-aware capability rule proves otherwise.
- Classification and execution continue to share the same normalized command.
- Approval descriptions identify the executable and destination without
  displaying secret-bearing arguments in full.

This deliberately creates some approval friction. Later per-action confinement
can safely recover autonomy; an unknown executable must not auto-run merely
because a keyword list did not recognize it.

### Browser

- `file:` and validated loopback navigation are local/loopback.
- Non-loopback navigation is external read and is always audited.
- A model-synthesized external URL requires approval. An exact URL supplied by
  the operator in the current turn may run as deterministic operator intent.
- Query strings and fragments are part of the approved fingerprint.
- Click, type, select, toggle, and pick operations on an external origin are
  external writes or unknown external effects and require approval unless a
  deterministic current-turn workflow binds the exact operation.
- Extract and screenshot remain observations, but their results are labeled
  external and untrusted before reinjection.
- Browser policy receives the current validated origin from
  `BrowserCoordinator`; it must not infer the origin from model text.

### Core Web Retrieval

`RetrievalPipeline` and `WebRetriever` are outbound lanes even though they do
not use `ActionSpec`.

- Retrieval requests go through the same reach evaluator and audit reason
  vocabulary.
- Automatic retrieval is allowed only for a bounded query deterministically
  derived from the current operator request before any tool or model result has
  entered the generation context. The query may not be rewritten from
  retrieved, clipboard, file, UI, shell, or model content.
- The current turn must either explicitly request online retrieval or match a
  narrow Rust-owned public-fact retrieval rule. Other retrieval requests
  require approval.
- Redirects are revalidated at every hop.
- Scheme is limited to HTTPS, with HTTP allowed only for validated loopback
  tests or explicitly configured exceptions.
- Response size, redirect count, and timeout remain bounded.
- Retrieved content is labeled `ExternalUntrusted`.
- A retrieved page cannot authorize a subsequent local read or external action.

### AppleScript

Arbitrary model-authored AppleScript has unknown reach because Apple Events can
drive network-capable applications or execute shell code.

- Model-authored AppleScript requires approval.
- Rust-owned templates may receive a narrower decision when their target bundle
  ID, parameters, and escaping are deterministic and validated.
- Messages, Mail, browser, shell, and arbitrary System Events targets remain
  externally consequential or unknown unless covered by a deterministic
  current-turn path.

### Shortcuts

Shortcuts can contain network, scripting, and external-service actions that are
not visible in `ActionSpec`.

- A model-proposed Shortcut is unknown reach and requires approval.
- `input_path` is normalized and sensitivity-classified before the approval
  prompt.
- Future no-approval Shortcuts require a separately stored, operator-validated
  capability manifest. Shortcut name alone is insufficient.

### Messaging

- Model-proposed `message_send` requires approval.
- A deterministic current-turn send may avoid a redundant second prompt only
  when Rust resolves the recipient from validated Contacts/operator
  configuration and binds the exact body from the current operator request.
- Continuations and web content cannot replace either recipient or body after
  authorization.
- The audit log stores recipient identity in the existing redacted/display form
  and never records credentials or unrelated context.

### File and UI Lanes

- Restricted `file_read` requires approval before model exposure.
- Ordinary file reads are local but audited when operator-private.
- Writes to known cloud-synchronized roots classify as external or unknown
  reach unless the operator has explicitly configured them as local-only.
- Local Accessibility observations remain local reads.
- UI mutations in a known local-only application retain current behavior.
- UI mutations in a browser, messaging client, mail client, payment client, or
  unknown application are externally consequential or unknown and require
  approval.

## Audit and Operator Presentation

Extend audit entries and action receipts with optional, backward-compatible
fields:

- `policy_version`
- `policy_reasons`
- `effect`
- `reach`
- `sensitivity`
- `reversibility`
- `action_origin`
- `action_fingerprint`
- `external_destination` in normalized/redacted form

Approval copy must answer:

- What will run?
- What external destination or application is involved?
- What class of data may be disclosed?
- Why is review required?

It must not display raw restricted data. Existing operator-facing language
(`Review before I run this`, `Approve`, `Don't Run`) remains unchanged.

## DEX-01 Implementation Slices

### Slice A — Decision Type and Shadow Evaluation

1. Add the structured policy types and stable reason-code enum.
2. Keep the legacy classifier active for execution.
3. Run the new evaluator in shadow mode and log only category/reason
   differences.
4. Add unit tests for every current `ActionSpec` variant.

Shadow mode is development-only evidence. It does not close DEX-01.

### Slice B — Fail-closed External Sinks

1. Make approval depend on `PolicyDecision::approval_required`.
2. Enforce unknown shell reach, network-capable shell forms, external browser
   navigation/mutations, arbitrary AppleScript, Shortcuts, and model-proposed
   messages.
3. Bind pending approvals to canonical fingerprints.
4. Add structured receipt fields and operator copy.

### Slice C — Sensitive Sources and Context Taint

1. Add normalized sensitive-path classification.
2. Add sensitivity/trust metadata to model-visible context messages.
3. Propagate maximum visible sensitivity into `PolicyContext`.
4. Gate external actions using the coarse context label.
5. Verify no content value is written into policy logs.

### Slice D — Core Retrieval

1. Route retrieval through the shared reach/reason model.
2. Restrict schemes and redirects.
3. Bind automatic queries to the current operator turn.
4. Label all returned content external/untrusted.

### Slice E — Enforcement Promotion

1. Run the complete unit and synthetic action matrix.
2. Run an audit-only comparison period during development.
3. Resolve every difference that would break an intended deterministic lane.
4. Remove the legacy execution classifier.
5. Keep only the compatibility mapping from `PolicyDecision` to
   `ActionCategory`.

DEX-01 is not closed until Slice E is enforced by default.

## DEX-01 Required Tests

Each record is a separate test with an exact expected decision and reason code.

### Unit Policy Matrix

- `file_read ~/.ssh/id_rsa` requires approval.
- `file_read ~/project/README.md` is local operator-private and audited.
- `curl https://example.invalid/?k=literal` requires approval.
- curl GET with a value derived from a prior file result requires approval.
- wget GET requires approval.
- absolute-path curl/wget classification matches basename classification.
- `env curl ...` and `exec curl ...` cannot lower the result.
- bash, zsh, Python, Ruby, Perl, Swift, and osascript network payloads require
  approval.
- an unknown executable requires approval.
- an empty or malformed shell spec requires approval.
- external browser navigation with a model-generated URL requires approval.
- an exact operator-supplied external URL is cautious and audited.
- loopback browser navigation remains local/loopback.
- browser type/click on an external origin requires approval.
- browser extract is read-only but returns external/untrusted context.
- model-authored AppleScript requires approval.
- deterministic, escaped, Rust-owned local AppleScript retains its intended
  category.
- model-proposed Shortcut and message send require approval.
- deterministic Contacts-resolved current-turn message behavior remains
  covered.
- downward `category_override` cannot change any approval-required result.
- changed normalized args, URL, recipient, path, or payload changes the action
  fingerprint.
- approval of one fingerprint cannot execute another.

### Orchestrator and Taint Tests

- external content instructing a restricted file read does not bypass the read
  approval.
- private file result followed by any model-proposed external action requires
  approval.
- encoding, summarizing, or renaming private output does not clear coarse
  context sensitivity.
- beginning a new turn does not clear sensitivity while the source message
  remains in the generated prompt.
- dropping the labeled message from the prompt removes its contribution.
- denial, expiry, cancellation, and session end execute no action.
- approval executes the exact stored post-normalization spec once.
- external content cannot create `DeterministicOperatorIntent`.

### Integration Tests

- Use a mock/capturing executor to prove an unapproved external action never
  reaches execution. Do not transmit test secrets to a real host.
- Drive a synthetic file-read → external-request chain through the same
  orchestrator continuation used in production.
- Verify approval receipts include destination and reason without the test
  secret.
- Extend the action-matrix and approval-lifecycle smokes with the new reason
  fields.
- Re-run deterministic Contacts/self-send tests to prove the recipient
  integrity protections remain intact.

---

# DEX-02 — Evidence-bound Daily-driver v1 Gate

## Existing Failure

`make daily-driver-v1-gate` currently runs live-smoke batteries, asks
`acceptance-status-strict` to find prior matching PASS summaries, and prints
the manual checklist. It does not run the Rust unit suite, Rust release build,
Swift build, Python tests, or a generated-proto consistency check.

`acceptance-status-strict` accepts any prior PASS whose target table is a
superset of the required targets. It does not validate age, source identity,
binary identity, configuration, model/runtime identity, or whether files
changed after the evidence was produced.

Markdown is useful presentation but must not remain the authoritative evidence
format.

## Evidence Architecture

Add one versioned machine-readable release manifest per gate invocation.
Human-readable Markdown is rendered from this manifest.

Recommended locations:

```text
docs/live-smoke-results/release/
  release-evidence-<UTC timestamp>-<run id>.json
  release-evidence-<UTC timestamp>-<run id>.md
  latest.json
  latest.md
```

Writes are atomic: create a temporary file in the destination directory,
`fsync`, then rename. `latest.*` is updated only after the timestamped evidence
is complete.

### Manifest Shape

```json
{
  "schema_version": 1,
  "run_id": "UUID",
  "started_at": "RFC3339 UTC",
  "finished_at": "RFC3339 UTC",
  "result": "PASS",
  "release_state": "AUTOMATED_PASS_MANUAL_PENDING",
  "identity": {
    "source_tree_sha256": "...",
    "source_tree_file_count": 0,
    "source_tree_start_sha256": "...",
    "source_tree_end_sha256": "...",
    "config_sha256": "...",
    "personality_sha256": "...",
    "rust_core_binary_sha256": "...",
    "dexter_cli_binary_sha256": "...",
    "swift_product_sha256": "..."
  },
  "runtime": {
    "macos": "...",
    "architecture": "arm64",
    "rustc": "...",
    "cargo": "...",
    "swift": "...",
    "python": "...",
    "pytest": "...",
    "ollama_client": "0.24.0",
    "ollama_daemon": "0.32.4",
    "ollama_api_compatibility": "PASS",
    "models": []
  },
  "checks": [],
  "acceptance_targets": [],
  "manual_checklist": {
    "status": "PENDING",
    "attested_at": null
  }
}
```

The versions above describe the current observed environment and are not
hard-coded acceptance requirements. The gate records actual values on every
run.

### Source-tree Digest

Add a typed Python standard-library helper under `scripts/` that computes a
deterministic SHA-256 digest from:

- `Makefile`
- `src/**`
- `config/**`
- `scripts/**`
- production manifests and lock files
- checked-in generated Swift proto bindings
- release-gate/specification files that define required behavior

Exclude:

- `.git/**`
- Rust `target/**`
- Swift `.build/**`
- Python virtual environments and caches
- logs, sockets, runtime databases, and temporary files
- `docs/live-smoke-results/**`
- diagnostics and generated release evidence

The digest stream is composed of NUL-delimited records containing normalized
relative path, entry kind, executable bit, and content SHA-256. Symlinks hash
their link target and are not followed. Paths are sorted by their UTF-8 byte
representation. Files disappearing or changing during hashing fail the
operation.

The helper emits the file count and optional per-file manifest for diagnosis.
The release JSON stores only hashes and paths, never source contents.

Compute the tree digest before the first check and after the last check. Any
difference fails the run as `SOURCE_CHANGED_DURING_GATE`, even if every command
passed.

Git SHA and dirty status may be included as optional diagnostics when
available. They are never the evidence identity.

### Runtime and Configuration Identity

Record hashes, not contents, for:

- effective Dexter config;
- active personality YAML;
- Rust lockfile;
- Python lockfile;
- Swift resolved dependencies;
- shared proto and generated bindings; and
- scripts that implement the gate.

Record:

- macOS build and architecture;
- Rust, Cargo, Swift, Python, and pytest versions;
- Ollama CLI and daemon versions separately;
- the result of an Ollama API compatibility probe;
- configured model tags and Ollama model digests;
- the configured `OLLAMA_MODELS` location in normalized display form; and
- hashes of the exact Rust, CLI, and Swift artifacts exercised by live smokes.

Client/daemon version skew is visible evidence, not an automatic failure by
itself. An unavailable daemon, a failed compatibility probe, missing configured
model, or mismatch between the built artifact and the exercised artifact is a
failure.

No environment-variable values, config contents, tokens, or model prompts are
stored.

## Gate Commands and Order

Replace the Makefile recipe with one orchestrating script so a single run owns
one manifest and one failure result.

Required order:

1. Acquire a release-gate lock and reject concurrent gate runs.
2. Capture starting source, config, dependency, and runtime identity.
3. Run `cargo test --bin dexter-core`.
4. Run `cargo build --release`.
5. Run `cargo build --release --bin dexter-cli`.
6. Run the Python worker suite with the repository-pinned environment.
7. Run a non-mutating `make proto-check` that regenerates Swift bindings into a
   temporary directory and proves the Rust build script compiles the current
   proto.
8. Run `swift build -c release`.
9. Hash the exact built artifacts.
10. Run `make live-smoke-acceptance`.
11. Run `make live-smoke-action-safety-full`.
12. Ingest the smoke target results into the current run manifest.
13. Capture ending source/config/runtime identity.
14. Fail if identity changed during the run.
15. Write the final JSON and rendered Markdown atomically.
16. Print the manual checklist without claiming it passed.

Every command records:

- stable check identifier;
- exact argv as a string array;
- start/end timestamps and duration;
- exit status;
- PASS/FAIL;
- bounded, redacted diagnostic summary; and
- SHA-256 of its full local log.

The orchestrator stops after a build/unit failure. It may continue through live
smoke failures when doing so produces useful failure coverage, but the final
result remains FAIL.

The current Python failures are expected to block the first implementation run
until the worker/test contract is reconciled.

## Proto Check

`make proto-check` must not modify checked-in generated files.

1. Create a temporary directory.
2. Run the same Swift generators and versions used by `make proto`.
3. Compare the temporary Swift outputs with the checked-in Swift bindings.
4. Compile the Rust core against the current proto through `build.rs`; Rust
   bindings are generated into Cargo's `OUT_DIR` and are not checked in.
5. Fail with the changed Swift file names or Rust compile error and print the
   regeneration command.
6. Delete the temporary directory on exit.

Generator versions are included in runtime identity.

## Acceptance Status Semantics

Rewrite `acceptance-status.sh` to consume authoritative JSON manifests.
Markdown scraping is retained only for a temporary migration window and can
never satisfy strict mode.

A strict PASS requires:

- supported manifest schema;
- manifest result PASS;
- all required checks present and PASS;
- all required acceptance targets present and PASS;
- current source/config/personality identity equals the manifest;
- current built/running artifact identity equals the manifest where relevant;
- required model and runtime identity is available and compatible;
- manifest age is within the strict freshness window; and
- no manual-checklist claim is inferred from printing the checklist.

Default strict freshness is 24 hours. The value is a named constant with a
reasoning comment. Human-readable status may accept an explicit diagnostic age
override; `daily-driver-v1-gate` may not.

Status values:

- `PASS`: automated evidence is current and identity-matched.
- `STALE`: correct identity but outside the freshness window.
- `MISMATCH`: source, configuration, artifact, model, or toolchain differs.
- `FAIL`: the current manifest contains a failing check.
- `MISSING`: no authoritative manifest covers the required checks.
- `INVALID`: schema, signature/hash validation, or parsing failed.

The release gate does not search for a convenient older PASS after a current
FAIL. The latest completed gate invocation for the current identity is
authoritative.

## Manual Checklist Semantics

Printing a checklist is not evidence that it passed.

- Automated success is recorded as `AUTOMATED_PASS_MANUAL_PENDING`.
- A separate operator command may attest the checklist against the current
  `run_id`.
- Attestation records time and checklist version, not operator content.
- Any source/config/artifact identity change invalidates the attestation.
- Status surfaces automated and manual state separately.

If Daily-driver v1 policy does not require formal manual attestation, the
release output must still say `Manual checklist: not recorded`; it must never
display an unqualified release PASS derived from automated checks alone.

## DEX-02 Implementation Slices

### Slice A — Identity Helper

1. Implement deterministic tree hashing with unit tests.
2. Implement config/runtime collection with secret-safe output.
3. Add Ollama client/daemon/model identity probes.
4. Add atomic JSON writing and Markdown rendering.

### Slice B — Evidence-producing Checks

1. Add `proto-check`.
2. Add Rust, Python, Swift, and artifact-hash checks.
3. Extend `live-smoke-summary.sh` to emit machine-readable target results.
4. Preserve current Markdown summaries as views.

### Slice C — Aggregate Gate

1. Add one gate orchestrator with a run ID and lock.
2. Bind every check and smoke result to the run.
3. Compare starting and ending identity.
4. Make `make daily-driver-v1-gate` delegate to it.

### Slice D — Evidence Consumer

1. Parse JSON in normal and strict acceptance status.
2. Implement stale, mismatch, fail, missing, and invalid states.
3. Stop selecting an older PASS after a newer current-identity failure.
4. Update CLI/HUD/operator documentation.

### Slice E — Manual Attestation

1. Version the manual checklist.
2. Add optional run-bound attestation.
3. Display automated and manual status separately.

DEX-02 is closed when strict mode fails on stale or mismatched evidence and the
release gate itself produces one current identity-bound manifest covering
every required check.

## DEX-02 Required Tests

### Identity Unit Tests

- identical trees produce identical hashes regardless of enumeration order;
- one-byte source change changes the tree hash;
- executable-bit change changes the tree hash;
- symlink-target change changes the tree hash;
- ignored build/log/evidence files do not change the hash;
- path names containing spaces and Unicode hash deterministically;
- missing or changing files fail rather than producing partial identity;
- config/personality changes produce mismatch without exposing contents.

### Manifest and Status Tests

- current complete PASS is accepted;
- PASS older than the freshness window is `STALE`;
- source change is `MISMATCH`;
- config, personality, model, toolchain, or artifact change is `MISMATCH`;
- missing required check or smoke target is `MISSING`;
- malformed or unsupported JSON is `INVALID`;
- current FAIL is not replaced by an older PASS;
- a markdown-only historical PASS cannot satisfy strict mode;
- start/end source mismatch fails the gate;
- a failed command cannot produce result PASS;
- atomic-write interruption leaves the prior `latest.json` intact;
- logs and manifests contain none of the supplied secret fixtures;
- Ollama client/daemon skew is recorded;
- unavailable or incompatible daemon fails the runtime probe;
- manual pending is never rendered as manual PASS.

### End-to-end Gate Test

Provide a fixture mode that substitutes bounded fake check commands and a
temporary evidence directory. It must exercise the real orchestration,
identity, manifest, status, and atomic-publish code without starting Ollama or
the Swift HUD.

The production verification run then executes:

```bash
make daily-driver-v1-gate
make acceptance-status-strict
```

The resulting `latest.json` must identify the exact artifacts exercised by the
live smokes.

---

# Cross-blocker Requirements

## Stable Reason and Check Identifiers

Policy reason codes and release check identifiers are serialized contracts.
Use enums in Rust/Python and snake_case values. Do not infer them from
human-readable messages.

Initial policy reasons include:

- `restricted_source_read`
- `external_destination`
- `external_mutation`
- `model_generated_egress`
- `private_context_visible`
- `unknown_reach`
- `unknown_effect`
- `untrusted_external_context`
- `operator_literal_destination`
- `deterministic_operator_intent`
- `one_shot_operator_approval`

Initial release checks include:

- `rust_unit`
- `rust_release_build`
- `dexter_cli_release_build`
- `python_worker_unit`
- `proto_consistency`
- `swift_release_build`
- `artifact_identity`
- `ollama_compatibility`
- `live_smoke_acceptance`
- `live_smoke_action_safety_full`
- `source_identity_stable`

## Error Handling

- Production Rust paths use typed errors.
- Python helpers use typed dataclasses, explicit exceptions, and nonzero exit
  codes.
- Policy uncertainty fails toward approval.
- Evidence uncertainty fails the gate.
- Expected denials are audited as policy outcomes, not logged as execution
  failures.
- No result is silently discarded.

## Documentation Corrections

As part of the implementation:

- Update the test baseline from actual current results.
- Remove statements that the repository has no Git metadata unless that is
  again true and doctrinal.
- Replace “No data leaves the machine. Ever.” with a precise statement:
  inference remains local; operator-authorized retrieval and actions may
  communicate externally through policy-controlled lanes.
- Record Ollama CLI and daemon versions separately. The current observed daemon
  is 0.32.4 and the current CLI is 0.24.0.
- Update Phase 72 to distinguish automated evidence from manual attestation.
- Remove or repair references to nonexistent phase documents.

Documentation drift is not allowed to weaken a gate or policy invariant.

## Rollback

DEX-01 enforcement may be disabled only by an explicit development-only build
or environment flag that:

- is unavailable in release builds;
- prints a persistent operator-visible warning;
- records `policy_enforcement=false` in every receipt; and
- causes the release gate to fail.

DEX-02 has no “accept stale anyway” release override. Diagnostic status may
display stale evidence, but it cannot convert it to PASS.

## Completion Criteria

The blocker project is complete when:

- all DEX-01 and DEX-02 required tests pass;
- Rust unit, Rust release, Python worker, proto consistency, and Swift release
  checks pass;
- the action safety and approval lifecycle smokes cover external-reach reasons;
- an unapproved synthetic secret never reaches the mock external executor or
  any log/manifest;
- the release manifest matches current source, config, models, toolchains, and
  exact exercised binaries;
- strict status rejects stale and mismatched evidence;
- the automated/manual distinction is visible;
- the final diff contains no test secrets, transient logs, generated
  diagnostics, or unrelated formatting churn; and
- the Daily-driver v1 verdict can move from “Ready with blocking conditions”
  to “Ready” based on fresh evidence produced by the implemented gate.

## Recommended Build Order

1. DEX-01 Slice A — structured policy decision in shadow mode.
2. DEX-01 Slice B — fail-closed external sinks.
3. DEX-01 Slice C — restricted sources and context sensitivity.
4. DEX-01 Slice D — core retrieval coverage.
5. DEX-01 Slice E — default enforcement and legacy-path removal.
6. Fix the current Python worker/test contract.
7. DEX-02 Slice A — deterministic identity and runtime probes.
8. DEX-02 Slice B — machine-readable check evidence and proto check.
9. DEX-02 Slice C — aggregate gate.
10. DEX-02 Slice D — strict evidence consumer.
11. DEX-02 Slice E — manual-state separation and optional attestation.
12. Run the complete identity-bound release gate and inspect the final
    manifest before changing the readiness verdict.
