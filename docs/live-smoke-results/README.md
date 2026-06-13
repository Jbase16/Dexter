# Live Smoke Results

`make live-smoke-summary` writes timestamped live-suite receipts here and updates
`latest.md` to the most recent run.

The logs under `logs/<timestamp>/` are generated artifacts. They are useful when
a target fails because the summary file links each target to its captured
terminal output.
