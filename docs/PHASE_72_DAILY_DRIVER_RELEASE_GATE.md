# Phase 72 — Daily-driver v1 Release Gate

## Outcome

Daily-driver v1 has a finite release gate. The goal is not to run every smoke
every day; it is to keep three lanes clear:

- a fast daily signal,
- a release-grade automated gate,
- a short manual checklist for real Mac workflow feel.

Voice remains maintenance-only for v1: existing STT/TTS health and restart
behavior must stay intact, but richer voice UX is not part of the v1 finish
line.

## Automated Lanes

### Daily Fast Lane

```bash
make live-smoke-action-safety-shared
make live-smoke-runtime-health
```

Use this during normal development. It gives a quick signal for action policy,
UI/window receipts, browser recovery evidence, cancellation, startup/readiness,
local RAM/CPU answers, Dexter Notices explanations, and HUD health.

### Release Gate

```bash
make daily-driver-v1-gate
```

This runs:

1. `make live-smoke-acceptance`
2. `make live-smoke-action-safety-full`
3. `make acceptance-status-strict`
4. `make daily-driver-v1-checklist`

`live-smoke-acceptance` proves operator controls, runtime health, local answers,
actions, browser recovery, UI/window actions, receipts, approvals, HUD action
surfaces, and cancellation.

`live-smoke-action-safety-full` adds the full action/HUD/model-driven browser
recovery sweep. It intentionally remains heavier than the daily lane.

### Opt-in Contacts/iMessage Lane

These are deliberately outside the default release gate because they depend on
local Contacts state and one variant can send a real message:

```bash
make live-smoke-message-contact-dry-run
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" make live-smoke-message-contact
DEXTER_SMOKE_CONTACT_NAME="Jason Phillips" DEXTER_SMOKE_ALLOW_REAL_SEND=1 make live-smoke-message-contact-approve
```

## Manual Checklist

Print the checklist:

```bash
make daily-driver-v1-checklist
```

Manual v1 checks:

- Launch Dexter from the Dock app.
- Move Dexter with snap/drag placement and confirm it does not jump monitors
  unexpectedly.
- Confirm the orb/HUD click-through boundary matches the visible shape.
- Open HUD Status and confirm readiness, model state, workers, disk, and
  recovery controls are understandable.
- Ask `what's using so much RAM right now?` and confirm Dexter gives a
  top-process report from local evidence.
- Ask `what do those Dexter Notices mean?` and confirm Dexter explains local
  ambient notices without model guessing.
- Run one safe browser action and inspect its receipt.
- Run one safe UI/window action and inspect its receipt.
- Trigger or inspect one failed action and confirm HUD Why plus `make why`
  explain it.
- Restart a worker from HUD Status, then confirm status returns to ready.
- Start a new session from the HUD.
- Quit Dexter from the HUD/app menu without hunting for the terminal.

## Not Required For Daily-driver v1

- Fine-tuning/personality training.
- Counterfactual replay and learned context scoring.
- Fully autonomous UI recovery clicks.
- Richer voice UX.
- Broad unsupervised high-risk actions.
- Multi-machine control.
- Consumer packaging/notarization.
