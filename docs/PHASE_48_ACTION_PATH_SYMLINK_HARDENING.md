# Phase 48 Action Path Symlink Hardening

## Goal

Make Dexter classify file-write actions against the same destination the OS will
actually touch.

The policy gate already collapsed `~`, `.`, and `..`, but symlinked parent
directories could still make a path look user-owned during policy classification
while resolving into a system directory at execution time.

Example shape:

```text
/tmp/link-to-etc/hosts -> /etc/hosts
```

Before this phase, the raw prefix looked like `/tmp/...`, so it could classify
as Cautious even though the kernel would write through the symlink into
`/etc/hosts`.

## Outcome

Complete.

`normalize_for_policy()` now:

1. expands leading `~`;
2. lexically collapses `.` and `..`;
3. canonicalizes the nearest existing parent directory;
4. re-appends any missing final path suffix.

That keeps policy classification and execution aligned even when the final file
does not exist yet.

The policy engine now also treats ordinary macOS writable temp roots as
non-system paths even after canonicalization:

```text
/tmp/
/private/tmp/
/var/tmp/
/private/var/tmp/
/var/folders/
/private/var/folders/
```

That matters because `mktemp` paths often canonicalize under
`/private/var/folders/...`; without the temp-root exception, harmless smoke-test
and operator scratch writes would be overclassified as destructive.

The same normalized path view is also used by shell-output classifiers such as:

- `tee /path`;
- `curl -o /path`;
- `find ... -fprint /path`.

So a model cannot avoid the approval gate by choosing a shell output command
instead of a structured FileWrite action.

## Before

Plain language behavior before this phase:

- Dexter could correctly treat `~/../../etc/hosts` as a system write after
  lexical normalization.
- Dexter did not resolve symlinked parent directories before classification.
- A write through a symlinked parent could therefore be classified less
  strictly than the real destination deserved.

## After

Plain language behavior after this phase:

- A write through a symlinked parent into `/etc`, `/usr`, `/System`,
  `/Library`, or other system prefixes requires approval.
- A write to normal temp/scratch locations remains Cautious, not Destructive.
- File-write policy and file-write execution use the same normalized path view.

This is an approval-gate fix, not a content restriction. Destructive/system
writes are still allowed when the operator approves them; the model just cannot
accidentally or adversarially downgrade the gate by hiding the real destination
behind path syntax.

## Evidence

Focused tests:

```text
cd src/rust-core && cargo test --bin dexter-core normalize_for_policy
PASS: 5 passed

cd src/rust-core && cargo test --bin dexter-core classify_file_write
PASS: 11 passed

cd src/rust-core && cargo test --bin dexter-core action::
PASS: 111 passed
```

Full Rust core test pass:

```text
cd src/rust-core && cargo test --bin dexter-core
PASS: 640 passed, 7 ignored
```

## Remaining Work

None for this phase.

Future action hardening should keep the same rule: approval gates are about
consequential side effects, not censorship. If a future path class needs
approval, the operator should be able to approve it and run it.
