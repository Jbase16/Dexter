# Phase 50 Operator Recovery Guidance

## Goal

Make health reports answer the operator's next question: "What do I do now?"

The previous recovery layer covered daemon-down launches, model-store env drift,
and worker restarts. It still had gaps:

- pending workers could be shown as restart candidates even during normal
  startup warmup;
- model-not-warm failures after warmup did not suggest the readiness/restart
  path;
- unexpected large Ollama runners warned in detail text but did not surface a
  concrete command in the standard Suggested fixes block;
- the HUD health/status surfaces did not mirror the model recovery guidance.

## Outcome

Complete.

`dexter-cli --doctor`, `make doctor`, `make status`, and diagnostic bundles now
produce recovery suggestions for:

- daemon down: `make open-app` or `make run`;
- model-store env drift: `make operator-ready`;
- model not warm after startup warmup: `make operator-ready`, then `make restart`;
- Ollama unreachable: `open -a Ollama`, then `make operator-ready`;
- unexpected large resident Ollama runner: the exact `ollama stop <model>`
  command parsed from the runner-pressure diagnostic;
- degraded workers: `dexter-cli --restart-component stt|tts|browser`.

Normal pending startup warmup deliberately does not produce restart suggestions.
Pending still means wait; degraded means act.

The HUD Health and Status surfaces now use shared recovery guidance:

- degraded workers still show the restart-button guidance;
- non-warm required models after startup show the operator-ready plus restart
  path;
- degraded health with no more specific recovery shows `make diagnostic-bundle`
  as the local evidence-gathering path.

## Evidence

Focused CLI tests:

```text
cd src/rust-core && cargo test --bin dexter-cli suggested_recovery_commands
PASS: 7 passed
```

Full CLI test pass:

```text
cd src/rust-core && cargo test --bin dexter-cli
PASS: 65 passed
```

Swift health/status rendering build:

```text
cd src/swift && swift build
PASS
```

Live startup/operator checks:

```text
make live-smoke-startup-readiness
PASS: pending startup health labels models as warming and does not show Suggested fixes

make live-smoke-operator-status
PASS
```

## Remaining Work

None for this phase.

Future health surfaces should keep the same split:

- pending startup components are not failures and should not suggest restarts;
- degraded workers can use targeted restart controls;
- model or Ollama runtime failures should point at `make operator-ready`, restart,
  and the diagnostic bundle instead of vague "needs attention" copy.
