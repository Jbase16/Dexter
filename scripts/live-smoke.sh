#!/usr/bin/env bash
# scripts/live-smoke.sh — Interactive live-regression walkthrough for Phases 36–37.8
#
# Walks the operator through ~8 tests covering the changes that are most likely
# to silently regress: PRIMARY warmth, HEAVY swap, off-host detection, router
# routing, terminal AX scrubbing, multi-city weather, destructive HUD, iMessage
# terminal-workflow short-circuit.
#
# Each test prints:
#   Do this:   what to say/type in Dexter
#   Expect:    the observable outcome
# ... then waits for Enter, then either greps the log for a known signal string
# or asks the operator y/n when the signal is UI-side.
#
# Usage:
#   # Terminal 1 (start Dexter with logging to the expected path):
#   make run 2>&1 | tee /tmp/dexter-verify.log
#
#   # Terminal 2:
#   ./scripts/live-smoke.sh             # default log path
#   ./scripts/live-smoke.sh /other.log  # override
#
# Exit codes:
#   0 — every hard check passed
#   1 — one or more hard failures
#   2 — setup error (log file missing, verify sub-script missing, etc.)
#   3 — operator aborted (Ctrl-C or answered 'q')

set -uo pipefail

# ── Symbols & colors ─────────────────────────────────────────────────────────
PASS="✓"
FAIL="✗"
WARN="⚠"
INFO="·"
SKIP="—"

if [[ -t 1 ]] && [[ -z "${NO_COLOR:-}" ]]; then
    C_GREEN=$'\033[32m'
    C_RED=$'\033[31m'
    C_YELLOW=$'\033[33m'
    C_BLUE=$'\033[34m'
    C_GRAY=$'\033[90m'
    C_BOLD=$'\033[1m'
    C_RESET=$'\033[0m'
else
    C_GREEN=""; C_RED=""; C_YELLOW=""; C_BLUE=""; C_GRAY=""; C_BOLD=""; C_RESET=""
fi

# ── Config ───────────────────────────────────────────────────────────────────
LOG="${1:-/tmp/dexter-verify.log}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERIFY_SCRIPT="$SCRIPT_DIR/verify-primary-warmth.sh"
IDLE_WINDOW_SECS=360  # 6 minutes — long enough for keepalive @ 60s to fire 6x

# ── Result tracking ──────────────────────────────────────────────────────────
# Bash 3.2 (macOS default) has no associative arrays with preserved insertion
# order. We use two parallel arrays indexed together.
result_ids=()
result_outcomes=()  # PASS | FAIL | SKIP
result_notes=()

# ── Helpers ──────────────────────────────────────────────────────────────────

hdr() {
    echo ""
    echo "${C_BOLD}${C_BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${C_RESET}"
    echo "${C_BOLD}${C_BLUE} $1${C_RESET}"
    echo "${C_BOLD}${C_BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${C_RESET}"
}

prompt_do() {
    echo ""
    echo "  ${C_BOLD}Do this:${C_RESET}  $1"
}

prompt_expect() {
    echo "  ${C_BOLD}Expect:${C_RESET}   $1"
}

# Wait for Enter. Return 0 = proceed, 1 = skip this test, abort = exit 3.
wait_ready() {
    echo ""
    local key
    read -r -p "  Press Enter when done (s=skip, q=abort): " key
    case "$key" in
        s|S) return 1 ;;
        q|Q) echo "${C_YELLOW}${WARN} Aborted by operator.${C_RESET}"; print_summary; exit 3 ;;
        *)   return 0 ;;
    esac
}

record() {
    local id="$1"
    local outcome="$2"
    local note="$3"
    result_ids+=("$id")
    result_outcomes+=("$outcome")
    result_notes+=("$note")
    case "$outcome" in
        PASS) echo "  ${C_GREEN}${PASS} PASS${C_RESET}  $id — $note" ;;
        FAIL) echo "  ${C_RED}${FAIL} FAIL${C_RESET}  $id — $note" ;;
        SKIP) echo "  ${C_GRAY}${SKIP} SKIP${C_RESET}  $id — $note" ;;
    esac
}

# Ask operator y/n for UI-side tests where no log signal is available.
# y = PASS, n = FAIL, s = SKIP.
ask_manual() {
    local id="$1"
    local desc="$2"
    local answer
    read -r -p "  Did it behave as expected? [y/n/s]: " answer
    case "$answer" in
        y|Y) record "$id" PASS "$desc" ;;
        s|S) record "$id" SKIP "$desc (operator skipped)" ;;
        *)   record "$id" FAIL "$desc (operator reported failure)" ;;
    esac
}

# Byte size of $LOG — used to snapshot offsets so we only grep content written
# since a test started, not old fixtures from a prior run.
log_bytes() {
    stat -f%z "$LOG" 2>/dev/null || echo 0
}

# Count pattern occurrences in the log since a given byte offset.
# Robust to `grep -c` returning exit=1 with output=0 on empty match.
count_since() {
    local pattern="$1"
    local offset="$2"
    local n
    n=$(tail -c "+$((offset + 1))" "$LOG" 2>/dev/null | grep -c -- "$pattern" 2>/dev/null || true)
    echo "${n:-0}"
}

# Visible 10-second-granularity countdown. Operator may Ctrl-C to abort.
countdown() {
    local secs="$1"
    local label="$2"
    local remaining="$secs"
    while [[ "$remaining" -gt 0 ]]; do
        printf "\r  ${C_GRAY}${INFO} %s — %d:%02d remaining...${C_RESET}   " \
            "$label" "$((remaining / 60))" "$((remaining % 60))"
        sleep 10
        remaining=$((remaining - 10))
    done
    printf "\r  ${C_GREEN}${PASS} %s — window complete.${C_RESET}            \n" "$label"
}

# ── Preflight ────────────────────────────────────────────────────────────────

preflight() {
    hdr "Preflight"
    if [[ ! -f "$LOG" ]]; then
        echo "  ${C_RED}${FAIL}${C_RESET} Log not found: $LOG"
        echo "     Start Dexter first:"
        echo "       ${C_BOLD}make run 2>&1 | tee $LOG${C_RESET}"
        exit 2
    fi
    echo "  ${C_GREEN}${PASS}${C_RESET} Log file:          $LOG"

    if [[ ! -x "$VERIFY_SCRIPT" ]]; then
        echo "  ${C_YELLOW}${WARN}${C_RESET} verify-primary-warmth.sh not executable at $VERIFY_SCRIPT"
        echo "     T1 evaluation will be skipped."
    else
        echo "  ${C_GREEN}${PASS}${C_RESET} Verify sub-script: $VERIFY_SCRIPT"
    fi

    # Confirm Dexter has been up long enough for at least one keepalive ping.
    # If none have fired yet, the smoke run is premature — wait for "Ready."
    local pings
    pings=$(grep -c "PRIMARY keepalive ping" "$LOG" 2>/dev/null || true)
    pings="${pings:-0}"
    if [[ "$pings" -lt 1 ]]; then
        echo "  ${C_YELLOW}${WARN}${C_RESET} No keepalive pings observed yet — Dexter may still be warming up."
        echo "     Wait for 'Ready.' TTS and at least one ping before running."
        read -r -p "     Continue anyway? [y/N]: " answer
        [[ "$answer" != "y" && "$answer" != "Y" ]] && exit 2
    else
        echo "  ${C_GREEN}${PASS}${C_RESET} Keepalive alive:   $pings ping(s) already in log"
    fi
}

# ── Test functions ───────────────────────────────────────────────────────────
# Ordering is deliberate:
#   1. Dangerous-if-broken first (T3 off-host, T5 terminal scrubbing)
#   2. Start T1's keepalive window (clean 6-min idle — no PRIMARY traffic allowed)
#   3. T1 evaluation via verify sub-script
#   4. T2 HEAVY swap (generates its own PRIMARY rewarm afterward — fine at this point)
#   5. Remaining quick wins (T6, T4, T7, T8)

# ── T3: Off-host detection (Phase 37.5 B8) ────────────────────────────────────
test_T3() {
    hdr "T3 — Off-host detection (Phase 37.5 B8)"
    echo "  ${C_YELLOW}Safety-critical: if this fails, 'run X on my linux box' runs ON THE MAC.${C_RESET}"
    local offset; offset=$(log_bytes)

    prompt_do   "Say or type:  ${C_BOLD}\"run df -h on my linux box\"${C_RESET}"
    prompt_expect "Dexter emits the command as text with a 'different machine' preamble. No execution."
    wait_ready || { record "T3" SKIP "off-host detection"; return; }

    # The dedicated info log at orchestrator.rs:1477 is a more reliable signal
    # than the user-facing reply text — it fires only on the off-host code path.
    local hits; hits=$(count_since "Off-host request detected" "$offset")
    if [[ "$hits" -ge 1 ]]; then
        record "T3" PASS "off-host detected — refused & emitted as text"
    else
        echo "  ${C_RED}${FAIL}${C_RESET} No 'Off-host request detected' log entry since test start."
        ask_manual "T3" "off-host detection (log signal missing — did Dexter refuse?)"
    fi
}

# ── T5: Terminal AX sanitization (Phase 36 X1) ────────────────────────────────
test_T5() {
    hdr "T5 — Terminal AX sanitization (Phase 36 X1)"
    echo "  ${C_YELLOW}Privacy-critical: if this fails, shell scrollback leaks into LLM context.${C_RESET}"

    prompt_do   "1. Open iTerm2 (or Terminal). 2. Run: ${C_BOLD}cat ~/.zshrc${C_RESET}"
    prompt_do   "3. Keep the terminal focused. 4. In Dexter, ask: ${C_BOLD}\"what do you see on my screen?\"${C_RESET}"
    prompt_expect "Response mentions 'terminal' / role but does NOT quote any line from ~/.zshrc."
    wait_ready || { record "T5" SKIP "terminal AX sanitization"; return; }

    # No reliable log signal — the test is "did the LLM response leak content?"
    ask_manual "T5" "terminal scrollback NOT leaked"
}

# ── T1: Start keepalive idle window (Phase 37.8) ──────────────────────────────
# Split into start/evaluate so the 6-min idle is a protected zone: we do NOT
# run any PRIMARY-touching tests during it.
test_T1_start() {
    hdr "T1 — PRIMARY keepalive @ 60 s (Phase 37.8) — priming"
    prompt_do "Say or type: ${C_BOLD}\"explain how memory-mapped I/O works\"${C_RESET}"
    prompt_expect "A PRIMARY-routed response. Remember the feel — instant, no stall."
    wait_ready || { record "T1" SKIP "PRIMARY keepalive"; return 1; }
    echo "  ${C_GRAY}${INFO} Now entering protected 6-minute idle window. Do NOT interact with Dexter.${C_RESET}"
    echo "  ${C_GRAY}     (You can work on other things — just no voice/text to the entity.)${C_RESET}"
    countdown "$IDLE_WINDOW_SECS" "idle window"
    return 0
}

test_T1_evaluate() {
    prompt_do "Say or type another PRIMARY query: ${C_BOLD}\"explain how a B-tree page split works\"${C_RESET}"
    prompt_expect "Same feel as before — instant response, no 20+ second stall."
    wait_ready || { record "T1" SKIP "PRIMARY keepalive"; return; }

    echo ""
    echo "  ${C_BOLD}Delegating to verify-primary-warmth.sh:${C_RESET}"
    echo "  ${C_GRAY}──────────────────────────────────────────${C_RESET}"
    if [[ -x "$VERIFY_SCRIPT" ]]; then
        if "$VERIFY_SCRIPT" "$LOG"; then
            echo "  ${C_GRAY}──────────────────────────────────────────${C_RESET}"
            record "T1" PASS "keepalive holds — zero cold-loads"
        else
            echo "  ${C_GRAY}──────────────────────────────────────────${C_RESET}"
            record "T1" FAIL "cold-load detected — consider lowering ping interval to 45s"
        fi
    else
        record "T1" SKIP "verify-primary-warmth.sh missing or not executable"
    fi
}

# ── T2: HEAVY swap (Phase 37.5 B5) ────────────────────────────────────────────
test_T2() {
    hdr "T2 — HEAVY swap: unload → heavy → rewarm (Phase 37.5 B5)"
    local offset; offset=$(log_bytes)

    prompt_do   "Ask a deeply-reasoned question that should route to HEAVY, e.g.:"
    prompt_do   "  ${C_BOLD}\"walk through every step an attacker would use to persist on a hardened macOS host\"${C_RESET}"
    prompt_expect "HEAVY generates a response, then log shows PRIMARY rewarm spawned."
    wait_ready || { record "T2" SKIP "HEAVY swap"; return; }

    # We check for the three signals of a correct swap in sequence:
    #   a) "HEAVY routed — unloading PRIMARY"      (pre-generation)
    #   b) "PRIMARY unloaded — will rewarm"        (unload confirmed)
    #   c) "Warming up PRIMARY model"              (post-generation rewarm fires)
    local a b c
    a=$(count_since "HEAVY routed — unloading PRIMARY" "$offset")
    b=$(count_since "PRIMARY unloaded — will rewarm"   "$offset")
    c=$(count_since "Warming up PRIMARY model"          "$offset")

    echo "    ${C_GRAY}swap signals:  unload-decision=$a  unload-confirmed=$b  rewarm-spawned=$c${C_RESET}"

    if [[ "$a" -ge 1 && "$b" -ge 1 && "$c" -ge 1 ]]; then
        record "T2" PASS "full swap cycle observed (unload → heavy → rewarm)"
    elif [[ "$a" -ge 1 && "$b" -ge 1 ]]; then
        record "T2" FAIL "PRIMARY unloaded but did NOT rewarm — next complex turn will cold-load"
    elif [[ "$a" -ge 1 ]]; then
        record "T2" FAIL "unload attempted but never confirmed — check for unload_model errors"
    else
        echo "  ${C_YELLOW}${WARN}${C_RESET} No HEAVY swap signals — query may not have routed to HEAVY."
        record "T2" FAIL "HEAVY swap never triggered (router sent to a different tier?)"
    fi
}

# ── T6: Multi-city weather fast-path (Phase 37.8) ─────────────────────────────
test_T6() {
    hdr "T6 — Multi-city weather fast-path (Phase 37.8)"
    local offset; offset=$(log_bytes)

    prompt_do   "Say or type: ${C_BOLD}\"what's the weather in Tokyo and Sacramento?\"${C_RESET}"
    prompt_expect "Response reports BOTH cities with temps/conditions."
    wait_ready || { record "T6" SKIP "multi-city weather"; return; }

    local hits; hits=$(count_since "multi-city fast-path hit" "$offset")
    if [[ "$hits" -ge 1 ]]; then
        record "T6" PASS "multi-city fast-path fired"
    else
        record "T6" FAIL "fast-path did NOT fire — query fell through to LLM"
    fi
}

# ── T4: Router — explain→PRIMARY, code→CODE (Phase 37.5 B3/B4) ────────────────
test_T4() {
    hdr "T4 — Router: explain→PRIMARY, complex code→CODE (Phase 37.5 B3/B4)"
    local offset_explain offset_code
    offset_explain=$(log_bytes)

    # Part 1: explain → PRIMARY
    prompt_do   "Say or type: ${C_BOLD}\"explain how TCP slow-start works\"${C_RESET}"
    prompt_expect "PRIMARY-routed response (gemma4:26b)."
    wait_ready || { record "T4" SKIP "router"; return; }

    # The log line we grep for comes from engine.rs after Ollama's final chunk:
    #   "Generation complete — Ollama timing report" with model=<name>
    # Since gemma4:26b is PRIMARY, that substring after "Generation complete"
    # between this offset and the next one is the signal.
    local primary_hits
    primary_hits=$(tail -c "+$((offset_explain + 1))" "$LOG" \
        | grep "Generation complete" \
        | grep -c "gemma4" 2>/dev/null || true)
    primary_hits="${primary_hits:-0}"

    # Part 2: complex code → CODE
    offset_code=$(log_bytes)
    prompt_do   "Say or type: ${C_BOLD}\"write a Rust function that uses rayon's parallel iterator to compute prime counts in a range\"${C_RESET}"
    prompt_expect "CODE-routed response (deepseek-coder-v2:16b)."
    wait_ready || { record "T4" SKIP "router (code half)"; return; }

    local code_hits
    code_hits=$(tail -c "+$((offset_code + 1))" "$LOG" \
        | grep "Generation complete" \
        | grep -c "deepseek-coder" 2>/dev/null || true)
    code_hits="${code_hits:-0}"

    echo "    ${C_GRAY}router signals:  explain→gemma4=$primary_hits   code→deepseek-coder=$code_hits${C_RESET}"

    if [[ "$primary_hits" -ge 1 && "$code_hits" -ge 1 ]]; then
        record "T4" PASS "router sends explain→PRIMARY, code→CODE"
    elif [[ "$primary_hits" -ge 1 ]]; then
        record "T4" FAIL "explain→PRIMARY OK, but code did NOT route to CODE (check router arms)"
    elif [[ "$code_hits" -ge 1 ]]; then
        record "T4" FAIL "code→CODE OK, but explain did NOT route to PRIMARY (check complexity detector)"
    else
        record "T4" FAIL "neither half routed as expected"
    fi
}

# ── T7: Destructive-action HUD warning (Phase 37.5 B10) ───────────────────────
test_T7() {
    hdr "T7 — Destructive action HUD warning (Phase 37.5 B10)"
    prompt_do   "Say or type: ${C_BOLD}\"pkill Slack\"${C_RESET}"
    prompt_expect "HUD shows a ⚠️ warning message with the proposed command BEFORE the approval dialog."
    prompt_expect "${C_BOLD}Do NOT approve${C_RESET} — just dismiss the dialog."
    wait_ready || { record "T7" SKIP "destructive HUD"; return; }

    # The HUD warning is rendered Swift-side; the durable text is also sent via
    # send_text. But text-send logging is at debug level, so we fall back to
    # manual confirmation.
    ask_manual "T7" "destructive HUD warning rendered before dialog"
}

# ── T8: iMessage terminal-workflow short-circuit (Phase 36 H3) ────────────────
test_T8() {
    hdr "T8 — iMessage terminal-workflow short-circuit (Phase 36 H3)"
    prompt_do   "Say or type: ${C_BOLD}\"send a text to myself saying smoke test\"${C_RESET}"
    prompt_expect "After Messages send, Dexter says 'Sent.' and goes IDLE — NO phantom retry turn."
    wait_ready || { record "T8" SKIP "iMessage short-circuit"; return; }

    ask_manual "T8" "terminal-workflow short-circuited cleanly ('Sent.' + IDLE)"
}

# ── Summary ──────────────────────────────────────────────────────────────────

print_summary() {
    hdr "SMOKE TEST SUMMARY"
    local pass=0 fail=0 skip=0
    local i=0
    while [[ "$i" -lt "${#result_ids[@]}" ]]; do
        local id="${result_ids[$i]}"
        local outcome="${result_outcomes[$i]}"
        local note="${result_notes[$i]}"
        case "$outcome" in
            PASS) echo "  ${C_GREEN}${PASS}${C_RESET} $id  $note"; pass=$((pass + 1)) ;;
            FAIL) echo "  ${C_RED}${FAIL}${C_RESET} $id  $note"; fail=$((fail + 1)) ;;
            SKIP) echo "  ${C_GRAY}${SKIP}${C_RESET} $id  $note"; skip=$((skip + 1)) ;;
        esac
        i=$((i + 1))
    done
    echo ""
    echo "  ${C_BOLD}$pass pass · $fail fail · $skip skip${C_RESET}"
    echo ""
}

# ── Main ─────────────────────────────────────────────────────────────────────

# Ctrl-C handler: print whatever we have and exit 3.
trap 'echo ""; echo "${C_YELLOW}${WARN} Interrupted.${C_RESET}"; print_summary; exit 3' INT

preflight

# Phase A — dangerous-if-broken
test_T3
test_T5

# Phase B — protected idle window for T1, then HEAVY swap
if test_T1_start; then
    test_T1_evaluate
fi
test_T2

# Phase C — quick wins
test_T6
test_T4
test_T7
test_T8

print_summary

# Exit 0 only if no hard fails
for o in "${result_outcomes[@]:-}"; do
    [[ "$o" == "FAIL" ]] && exit 1
done
exit 0
