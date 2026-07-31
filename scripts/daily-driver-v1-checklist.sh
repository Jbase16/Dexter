#!/usr/bin/env bash
# Print the finite manual checklist for Dexter Daily-driver v1.

set -euo pipefail

cat <<'EOF'
# Dexter Daily-driver v1 Manual Checklist

Automated release gate:

```bash
make daily-driver-v1-gate
```

That runs:

1. Rust unit and release builds.
2. The pinned Python worker suite.
3. Non-mutating proto consistency checks.
4. The Swift release build.
5. Exact release-artifact hashing.
6. `make live-smoke-acceptance`.
7. `make live-smoke-action-safety-full`.
8. Start/end source, configuration, model, and toolchain identity comparison.

Automated success is recorded as `AUTOMATED_PASS_MANUAL_PENDING`. Printing this
checklist does not record or imply manual completion.

Manual checks before calling v1 done:

- Launch Dexter from the Dock app.
- Move Dexter with snap/drag placement and confirm it does not jump monitors unexpectedly.
- Confirm the orb/HUD click-through boundary matches the visible shape.
- Open HUD Status and confirm readiness, model state, workers, disk, and recovery controls are understandable.
- Ask: `what's using so much RAM right now?` and confirm Dexter gives a top-process report from local evidence.
- Ask: `what do those Dexter Notices mean?` and confirm Dexter explains local ambient notices without model guessing.
- Run one safe browser action and inspect its receipt.
- Run one safe UI/window action and inspect its receipt.
- Trigger or inspect one failed action and confirm HUD Why plus `make why` explain it.
- Restart a worker from HUD Status, then confirm status returns to ready.
- Start a new session from the HUD.
- Quit Dexter from the HUD/app menu without hunting for the terminal.

Opt-in messaging checks, run only when deliberately testing Contacts/iMessage:

```bash
make live-smoke-message-contact-dry-run
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" DEXTER_SMOKE_ALLOW_REAL_SEND=1 make live-smoke-message-contact-approve
```

Voice policy:

- Voice is maintenance-only for v1.
- Existing STT/TTS health and restart behavior must stay intact.
- No richer voice UX is required for Daily-driver v1.

Optional run-bound attestation, only after completing every manual item:

```bash
make acceptance-status-strict
make daily-driver-v1-attest RUN_ID="<exact Run ID from acceptance-status>"
```

Attestation records only the run ID, checklist version, timestamp, and identity
fingerprint. It does not record checklist answers or operator content. Any
identity change invalidates it.
EOF
