#!/usr/bin/env bash
# scripts/verify-primary-warmth.sh — Dexter PRIMARY keepalive + retrieval verifier
#
# Evaluates a captured `make run` log against the pass/fail criteria defined
# during Phase 37.8 (PRIMARY keepalive + weather multi-city fix).
#
# Usage:
#   # Terminal 1 (launch Dexter with logging to a fixed path):
#   make run 2>&1 | tee /tmp/dexter-verify.log
#
#   # (Exercise the following during the session, leaving ≥6 min idle in the
#   #  middle so the keepalive has time to fire multiple pings:)
#   #     1. Weather multi-city:  "what's the weather in Tokyo? and in Sacramento?"
#   #     2. PRIMARY-routed turn: any "explain ..." or "analyze ..." question
#   #     3. Wait 6+ minutes idle (keepalive task should fire 4+ pings at 90s)
#   #     4. PRIMARY-routed turn again: another "explain ..." question
#
#   # Terminal 2 (evaluate the log):
#   ./scripts/verify-primary-warmth.sh
#
# Exit codes:
#   0  — all hard checks pass
#   1  — one or more hard checks failed (FAIL rows)
#   2  — log file missing (nothing to evaluate yet)
#
# The script is read-only: it never modifies the log or the process tree.

set -uo pipefail

PASS="✓"
FAIL="✗"
WARN="⚠"
INFO="·"

LOG="${1:-/tmp/dexter-verify.log}"

if [[ ! -f "$LOG" ]]; then
    echo "$FAIL Log file not found: $LOG"
    echo "  Start Dexter with:"
    echo "    make run 2>&1 | tee $LOG"
    exit 2
fi

echo "Evaluating: $LOG"
echo "─────────────────────────────────────────────────"

hard_pass=0
hard_fail=0
soft_note=0

# ── A. Weather multi-city fast-path ───────────────────────────────────────────
# Informational — only triggers if the operator actually asked a multi-city
# weather question during the session. Not a hard fail if absent.
multi=$(grep -c "multi-city fast-path hit" "$LOG" 2>/dev/null || true)
multi=${multi:-0}
if [[ "$multi" -ge 1 ]]; then
    echo "$PASS A. weather multi-city fast-path: $multi hit(s)"
    hard_pass=$((hard_pass + 1))
else
    echo "$INFO A. weather multi-city: no multi-city query observed this session"
    echo "      (to exercise: ask \"weather in Tokyo and Sacramento\")"
    soft_note=$((soft_note + 1))
fi

# ── B. PRIMARY keepalive pings fire ───────────────────────────────────────────
# Any observable ping at info level confirms the spawned task is alive and
# the warm flag gate isn't stuck closed.
pings=$(grep -c "PRIMARY keepalive ping" "$LOG" 2>/dev/null || true)
pings=${pings:-0}
if [[ "$pings" -ge 1 ]]; then
    echo "$PASS B. keepalive task alive: $pings ping(s) observed"
    hard_pass=$((hard_pass + 1))
else
    echo "$INFO B. no keepalive pings yet — let Dexter idle for >90 s"
    soft_note=$((soft_note + 1))
fi

# ── C. Zero cold-load incidents ───────────────────────────────────────────────
# The whole point of the keepalive: pings should stay fast and user-initiated
# turns should never hit a supposedly-warm model cold.
cold_pings=$(grep -c "keepalive ping took a cold-load" "$LOG" 2>/dev/null || true)
cold_pings=${cold_pings:-0}
cold_turns=$(grep -c "Unexpected cold-load on supposedly-warm model" "$LOG" 2>/dev/null || true)
cold_turns=${cold_turns:-0}
total_cold=$((cold_pings + cold_turns))

if [[ "$total_cold" -eq 0 ]]; then
    echo "$PASS C. zero cold-loads on warm model"
    hard_pass=$((hard_pass + 1))
else
    echo "$FAIL C. cold-loads detected: $cold_pings on ping(s), $cold_turns on turn(s)"
    echo "      → ping interval still too wide; consider lowering"
    echo "        PRIMARY_KEEPALIVE_PING_INTERVAL_SECS further"
    hard_fail=$((hard_fail + 1))
fi

# ── Supplementary: distribution of ping load_ms values ────────────────────────
# A summary to eyeball whether the "good" pings are truly fast or just not-WARN.
# Extracts the numeric load_ms values from ping log lines.
if [[ "$pings" -ge 1 ]]; then
    echo ""
    echo "$INFO ping load_ms distribution:"
    grep "PRIMARY keepalive ping" "$LOG" \
        | grep -oE '"load_ms":[0-9]+' \
        | cut -d: -f2 \
        | sort -n \
        | awk '
            { values[NR] = $1; sum += $1 }
            END {
                if (NR == 0) exit
                printf "      n=%d  min=%d  median=%d  max=%d  mean=%d\n",
                    NR, values[1], values[int((NR+1)/2)], values[NR], int(sum/NR)
            }
        '
fi

echo "─────────────────────────────────────────────────"
echo "  hard: $hard_pass pass, $hard_fail fail   soft: $soft_note note(s)"

if [[ "$hard_fail" -gt 0 ]]; then
    exit 1
fi
exit 0
