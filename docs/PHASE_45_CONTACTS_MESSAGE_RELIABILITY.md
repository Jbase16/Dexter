# Phase 45 Contacts Message Reliability

## Goal

Continue the action-observability work into the Contacts-backed messaging path.
The specific reliability bug was that a Contacts AppleScript failure was treated
as `NotFound`, which made Dexter tell the operator the person was missing from
Contacts even when the real issue was that Contacts lookup failed.

## Outcome

Complete.

Contacts lookup failure is now a distinct Rust-side outcome for both structured
`message_send` resolution and legacy Messages.app AppleScript recipient
cross-reference:

- structured `message_send` name resolution can return `LookupFailed`;
- generated Messages.app send validation can return `LookupFailed`;
- operator-facing replies say Contacts lookup failed and suggest checking
  Contacts access;
- daemon `ActionDiagnostic` and `dexter-cli --why` clue analysis recognize that
  copy as a Contacts lookup/access failure instead of a missing contact.

This does not add a new deny category. It preserves the existing behavior that
Dexter only proceeds with an externally visible message after Rust can verify
the recipient and the operator approves the action.

## Evidence

Targeted tests:

```text
cargo test --bin dexter-core contacts_lookup_failure: PASS
cargo test --bin dexter-cli contacts_lookup_failure: PASS
```

Full Rust tests:

```text
cargo test --bin dexter-core: PASS, 625 passed, 7 ignored
cargo test --bin dexter-cli: PASS, 49 passed
```

Focused live receipt:

```text
docs/live-smoke-results/live-smoke-20260527_212205.md
```

Relevant live targets:

```text
live-smoke-message-contact: PASS
live-smoke-external-failures: PASS
```

The known-contact smoke used `DEXTER_SMOKE_CONTACT_NAME="Jason Phillips"` with
auto-deny, so Contacts resolution reached approval and no real iMessage was
sent.

## Remaining Work

None for this checkpoint. The next capability phase should move from
messaging-specific reliability into broader operator workflow polish or another
unfinished action domain.
