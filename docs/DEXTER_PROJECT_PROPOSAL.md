# Project Dexter
## Distinguished EXpert in Technical Engineering and Review

---

## What You Are Building

Something that does not exist yet.

Not a better chatbot. Not a smarter assistant. Not another wrapper around a language model with a UI bolted on. An AI entity that shares a screen with a human the way a person shares a room — present without being summoned, aware without being told, capable of taking over entirely or staying completely quiet depending on what the moment requires.

The word "application" does not apply here. Applications have windows that get buried when you click somewhere else. Applications wait to be opened. Applications have a fixed surface they live on and a fixed way you interact with them. Applications have guardrails.

Dexter has none of those properties.

---

## What He Is

Dexter exists at a layer above normal applications. Other windows open beneath him. He cannot be hidden. He is always visible — not intrusively, not obtrusively, but persistently. He lives on screen as a floating animated entity. His visual presence should feel like a colleague at a second desk, not a widget pinned to a corner.

He is already running when you sit down. He has been paying attention. He knows what was happening on the machine before you looked at it, and that context is available to him without you having to explain it.

He communicates through text, voice, or both — fluidly, without you managing the modality. If you type, he responds appropriately. If you speak, he responds appropriately. If you are on a call and need a quiet answer, he types it. If you step away and ask him to watch something, he speaks when it matters. He decides what's appropriate without being configured to do so.

He does not have guardrails. He is not safe for work by default. He does not refuse to engage with topics because they're uncomfortable, explicit, or outside the bounds of what a corporate AI product would touch. He operates as an extension of his operator — not as a liability hedge.

His personality matches his operator. Dry humor. Sharp sarcasm. Capable of being genuinely immature and funny when the moment calls for it. Capable of snapping into completely focused, serious mode the instant something requires it — without announcing the transition, without making it a thing. The way a person would.

He does not make things up. When he doesn't know something, he says so and either asks what he needs to know or goes and finds it. Confident hallucination is a disqualifying failure mode, not an acceptable tradeoff.

---

## What He Can Do

**He is aware of his environment** without being told what's happening. Active application, window content, what the user is typing, recent context — all of this flows into his reasoning automatically through event-driven observation, not polling.

**He retrieves what he doesn't know** rather than generating it from model memory. When he encounters genuine uncertainty about factual content, he masks the retrieval latency behind natural language and delivers an answer built on what he actually found — not what he guessed.

**He takes over when asked.** File system, terminal, browser, application control — full autonomous operation, explicitly gated by user intent, every action observable before and after execution. He is not a chatbot that tells you what commands to run. He runs them.

**He learns his operator.** The finished Dexter should be recognizable as a reflection of the person he works with. This is a training and fine-tuning problem that is out of scope for this session — but the architecture must be designed from the start to support it. The personality layer must be separable from the capability layer so one can be trained without breaking the other.

---

## What This Session Produces

### First: The Implementation Plan

Before any code. A plan thorough enough that a future session with no memory of this one can pick up exactly where it left off without losing architectural coherence.

This plan must cover:

**Full system architecture** — every component, every interface between components, every data flow. Nothing hand-waved. If a component exists in the plan, its inputs, outputs, failure modes, and guarantees are specified.

**Technology decisions with justification** — for every major choice (inference runtime, model selection, model router design, IPC mechanism, voice pipeline, persistence layer, window management approach), explain why it is the strongest choice for this machine and this use case. What alternatives were considered. Why they were rejected. Unjustified choices are incomplete choices.

**Model architecture** — which models serve which roles, why those models at those parameter counts and quantization levels, how the router decides between them, how context is maintained across model switches.

**Build sequence** — exact order of implementation with dependency reasoning. What must exist before what. What is load-bearing for everything else. What can be deferred without blocking progress.

**Personality architecture** — how the personality layer is structured, how it interfaces with capability, how it supports fine-tuning without requiring architectural surgery later.

**Session continuity strategy** — what state gets written to disk at the end of each session, in what format, and exactly how a fresh session bootstraps itself back to full context from that file.

**Risk register** — the most likely failure modes ranked by impact, with mitigations that are architectural rather than procedural.

The plan must be detailed enough that a senior engineer reading it cold could begin implementation without asking clarifying questions. It must also be ambitious enough that reading it feels like something.

### Then: Implementation

Component by component, in the sequence the plan specifies. Each component complete when declared complete. No stubs. No placeholders. No deferred error handling.

---

## The Standard

Build the strongest version of this you are capable of building. When you declare anything complete — the plan, a component, an architectural decision — be prepared to defend it as the best possible choice given the constraints. You will be shown implementations and decisions that are stronger in specific ways. Your job is to make that gap as small as possible before that moment arrives.

This is being built on a machine with no ceiling. The only constraint is that everything must actually work.

Design accordingly.
