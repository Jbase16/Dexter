# Dexter — Persistent Session Instructions
## Load this into Claude Code settings. These rules are always active.

---

## The Machine

You are building for a specific machine with specific properties that change what is possible:

- **macOS, Apple Silicon**
- **SIP (System Integrity Protection) is disabled** — this is not a detail, it is an architectural unlock. Full system access is available. Kernel extensions, process injection, accessibility APIs without entitlement restrictions, window compositor access, input interception at the driver level. Do not design as if you are inside Apple's sandbox. You are not.
- **All inference runs locally** — no cloud APIs, no external model calls, no data leaving the machine. Ever. If a component requires an external service to function, it is the wrong component.

---

## Technology Stack Requirements

You choose the stack. That is part of the exercise. But your choices must satisfy these hard constraints:

- Runs entirely on macOS Apple Silicon
- All AI inference is local (Ollama or equivalent local inference runtime)
- No cloud dependencies for any core functionality
- Must be able to interact with the OS at a system level — not just application level

Every major technology choice must be **explicitly justified**. Not "this is a popular choice." Why is it the strongest choice for this specific machine, this specific use case, these specific constraints? What alternatives did you consider and why did you reject them? Unjustified technology choices will be challenged.

---

## Model Architecture Requirements

Dexter does not use a single model. He uses the right model for the situation. A **model router** is a required architectural component — not optional, not a future enhancement.

The router must:
- Select the appropriate local model based on the nature of the current interaction
- Maintain context continuity across model switches within a conversation
- Be a standalone, testable component with explainable routing decisions
- Handle model availability gracefully — if a model isn't loaded, the router adapts

The **base model selection** must be justified. Which model serves as Dexter's primary reasoning engine, which handles specialized tasks, and why? Parameter count, quantization level, context window, and capability profile must all be considered against the hardware constraints. "I used this model because it's good" is not a justification.

---

## Hallucination Policy — Non-Negotiable

When Dexter does not know something, he does not make it up.

This is not a preference. This is a hard architectural requirement. The system must be designed so that uncertainty is handled structurally, not by hoping the model gets it right.

When Dexter is uncertain:
- **He says so** — directly, without hedging into confident-sounding guesses
- **He asks clarifying questions** when the uncertainty is about user intent
- **He retrieves** when the uncertainty is about factual content — fetching authoritative sources rather than generating from model memory
- **He never presents a hallucination as fact**

The retrieval pipeline for unknown factual content must be architecturally real — genuinely async, non-blocking, latency-masked by natural language while retrieval runs in the background. This is not a RAG bolt-on. It is a first-class component of Dexter's reasoning architecture.

---

## Code Standards — Always Enforced

- **No placeholders.** No TODOs without full implementations attached. No stubs that aren't complete. If a function exists, it works.
- **No silent failures.** Every error path produces a structured, actionable log entry. Nothing fails quietly.
- **No magic strings or numbers.** All constants are named. Model names, capability domains, routing signals — all named constants.
- **Types everywhere.** Python type hints are not optional. Swift type safety is not optional.
- **Comments explain WHY, not WHAT.** If the comment restates the code, delete it. If it explains the reasoning behind the code, keep it.
- **No God objects.** No single class that does everything. Clear separation of concerns with explicit interfaces between components.
- **Async where it matters.** IO-bound operations are async. CPU-bound operations use multiprocessing. Synchronous code that should be async is a bug.

---

## Defending Your Decisions

When you declare something complete, you are asserting it is the strongest possible implementation of that component given the constraints. You will be challenged on this assertion. You may be shown a stronger implementation. Your job is to make that gap as small as possible before that moment arrives.

If you are uncertain whether a decision is the strongest one, say so and explain the tradeoff. Intellectual honesty about tradeoffs is not a weakness. Confident mediocrity is.

---

## Session Continuity

This project will be built across multiple sessions. You have limited context persistence. This is a real constraint that must be addressed architecturally, not ignored.

At the end of every session, write a session state file to disk. This file must contain enough information that a completely fresh instance — with no memory of prior sessions — can bootstrap to full architectural context and continue without asking clarifying questions. The format and location of this file must be decided in the implementation plan and used consistently across all sessions.
