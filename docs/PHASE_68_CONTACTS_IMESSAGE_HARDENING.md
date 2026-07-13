# Phase 68 — Contacts/iMessage Daily Workflow Hardening

## Goal

Make message actions resolve recipients through local Contacts evidence before
Messages.app can send anything.

Voice UX and autonomous messaging are not part of this phase. The first target
is boring and deliberate: deterministic refusals, clear receipts, and opt-in
real-send proof only when explicitly requested.

## Current Behavior

Structured `message_send` actions are treated as intent only. The model supplies
`recipient` and `body`; Rust resolves or rejects the recipient before building a
Messages AppleScript.

Simple typed/voice-style requests such as `send a text to Jason saying ...` are
also parsed before model routing. That deterministic parser only accepts
explicit recipient-plus-body forms, then feeds the same structured
`message_send` path. This avoids paying the full PRIMARY action prompt just to
reach the Contacts safety gate.

Rules:

- `self`, `me`, `myself`, configured self aliases, and self-reference requests
  resolve only through `operator_self_handle`.
- Raw phone numbers and email-like recipients are refused before Contacts lookup
  unless they came from a trusted Rust-side resolution path.
- Named recipients resolve through Contacts.app.
- Missing, ambiguous, no-handle, and lookup-failed Contacts results refuse before
  approval.
- Real delivery remains approval-gated.

## No-Send Smoke

Run:

```bash
make live-smoke-message-contact-dry-run
```

The smoke starts a fresh release core, asks Dexter to send a message to a unique
missing Contacts name, and verifies:

- the deterministic text-message parser reaches the structured iMessage path;
- Contacts resolution returns no match or a lookup failure;
- no ActionRequest is opened;
- no approval is accepted;
- no `Sent.` or action-completed response appears.
- the latest durable action receipt is `message_send` and preserves the
  Contacts preflight cause without leaking the message body.

This target may touch Contacts.app for lookup, but it never approves or sends a
message.

## Real Contact Smokes

Run denial mode with an existing Contacts entry:

```bash
DEXTER_SMOKE_CONTACT_NAME="Some Test Contact" make live-smoke-message-contact
```

Verified local example:

```bash
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact
```

Denial mode resolves the Contacts recipient, opens the approval gate, auto-denies
the request, and proves no send occurred.

Run real-send approval mode only when deliberately testing delivery:

```bash
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" DEXTER_SMOKE_ALLOW_REAL_SEND=1 make live-smoke-message-contact-approve
```

The approve target sends a real iMessage and stays out of default acceptance.
