#!/usr/bin/env bash
# scripts/live-local-answers-smoke.sh - deterministic local answer regression.
#
# Starts a fresh release Dexter core, asks normal operator questions that must
# be answered from local system evidence, and verifies the response did not fall
# through to model speculation.

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
LOG="/tmp/dexter-local-answers-smoke.log"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CORE_BIN="$RUST_DIR/target/release/dexter-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_PID=""
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

say() {
    printf '[%s] %s\n' "$1" "$2"
}

fail() {
    say "$FAIL" "$*"
    if [[ -f "$LOG" ]]; then
        say "$INFO" "core log tail:"
        tail -100 "$LOG" || true
    fi
    exit 1
}

socket_accepts() {
    python3 - "$SOCKET" <<'PY' >/dev/null 2>&1
import socket
import sys

path = sys.argv[1]
s = socket.socket(socket.AF_UNIX)
s.settimeout(1)
sys.exit(0 if s.connect_ex(path) == 0 else 1)
PY
}

cleanup() {
    if [[ -n "$CORE_PID" ]]; then
        local pid="$CORE_PID"
        CORE_PID=""
        kill "$pid" >/dev/null 2>&1 || true
        wait "$pid" >/dev/null 2>&1 || true
    fi
    rm -f "$SOCKET" "$SHELL_SOCKET"
}
trap cleanup EXIT INT TERM

assert_contains() {
    local file="$1"
    local pattern="$2"
    local label="$3"
    if ! grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - missing: $pattern"
        cat "$file"
        return 1
    fi
    return 0
}

assert_not_contains() {
    local file="$1"
    local pattern="$2"
    local label="$3"
    if grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - should not contain: $pattern"
        cat "$file"
        return 1
    fi
    return 0
}

require_clean_socket() {
    if socket_accepts; then
        fail "a Dexter daemon is already accepting connections at $SOCKET"
    fi
}

build_binaries() {
    say "$INFO" "building and signing release core and CLI"
    (
        cd "$RUST_DIR" || exit 2
        cargo build --release --bin dexter-core --bin dexter-cli
    ) || exit 2
    bash "$ROOT_DIR/scripts/sign-dexter-core.sh" >/dev/null \
        || fail "release core could not be assigned Dexter's stable macOS identity"
}

start_core() {
    rm -f "$SOCKET" "$SHELL_SOCKET"
    : > "$LOG"
    say "$INFO" "starting release core; log: $LOG"
    RUST_LOG=info "$CORE_BIN" >> "$LOG" 2>&1 &
    CORE_PID="$!"

    local waited=0
    while [[ "$waited" -lt 90 ]]; do
        if socket_accepts; then
            break
        fi
        if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            fail "core exited before opening socket"
        fi
        sleep 1
        waited=$((waited + 1))
    done

    if ! socket_accepts; then
        fail "core did not open $SOCKET within 90s"
    fi

    bash "$ROOT_DIR/scripts/wait-for-ready.sh" \
        --cli-bin "$CLI_BIN" \
        --timeout "$CORE_WARMUP_TIMEOUT_SECS" \
        --out /tmp/dexter-local-answers-doctor.out \
        --label "local answers core" \
        --core-pid "$CORE_PID" \
        --core-log "$LOG" \
        >/dev/null || fail "core did not reach ready health"
}

run_cli_turn() {
    local prompt="$1"
    local out_file="$2"
    "$CLI_BIN" --quiet --idle-timeout 180 "$prompt" > "$out_file" 2>&1 \
        || {
            say "$FAIL" "dexter-cli failed for prompt: $prompt"
            cat "$out_file"
            return 1
        }
}

run_cli_session() {
    local out_file="$1"
    shift
    "$CLI_BIN" --quiet --idle-timeout 180 "$@" > "$out_file" 2>&1 \
        || {
            say "$FAIL" "dexter-cli failed during multi-turn local-truth session"
            cat "$out_file"
            return 1
        }
}

main() {
    require_clean_socket
    build_binaries
    start_core

    local ram_out cpu_out notices_out ui_out session_out session_pid log_start ui_log_start ok
    ram_out="$(mktemp -t dexter-local-answers-ram.XXXXXX)"
    cpu_out="$(mktemp -t dexter-local-answers-cpu.XXXXXX)"
    notices_out="$(mktemp -t dexter-local-answers-notices.XXXXXX)"
    ui_out="$(mktemp -t dexter-local-answers-ui.XXXXXX)"
    session_out="$(mktemp -t dexter-local-answers-session.XXXXXX)"
    ok=0

    say "$INFO" "checking deterministic RAM/process answer"
    run_cli_turn "what's using so much RAM right now?" "$ram_out" || ok=1
    if [[ "$ok" -eq 0 ]]; then
        assert_contains "$ram_out" "Top memory users right now (Activity Monitor-style footprint):" "RAM answer includes Activity Monitor-style process table" || ok=1
        assert_contains "$ram_out" "Measured just now with macOS \`top -o mem\`" "RAM answer names the footprint sampler" || ok=1
        assert_not_contains "$ram_out" "GiB RSS" "RAM answer must not use misleading ps RSS" || ok=1
        assert_not_contains "$ram_out" "Ollama model residency" "RAM answer stays a compact process report" || ok=1
        assert_not_contains "$ram_out" "I don't have access" "RAM answer should not be model capability denial" || ok=1
    fi

    say "$INFO" "checking deterministic CPU/process answer"
    run_cli_turn "what's using the most CPU right now?" "$cpu_out" || ok=1
    if [[ "$ok" -eq 0 ]]; then
        assert_contains "$cpu_out" "Top CPU users right now:" "CPU answer includes CPU table" || ok=1
        assert_contains "$cpu_out" "Measured just now with the second sample from macOS \`top\`" "CPU answer includes one-second source note" || ok=1
        assert_not_contains "$cpu_out" "I don't have access" "CPU answer should not be model capability denial" || ok=1
    fi

    say "$INFO" "checking deterministic Dexter Notices explanation"
    run_cli_turn "what do those Dexter Notices mean?" "$notices_out" || ok=1
    if [[ "$ok" -eq 0 ]]; then
        assert_contains "$notices_out" "Dexter Notices are local ambient trigger notifications" "notice answer explains local source" || ok=1
        assert_contains "$notices_out" "does not mean the trigger itself failed" "notice answer explains trigger wording" || ok=1
        assert_contains "$notices_out" "make why" "notice answer points to action receipt" || ok=1
        assert_not_contains "$notices_out" "I don't know" "notice answer should not be model uncertainty" || ok=1
    fi

    say "$INFO" "checking read-only open-window evidence bypasses model routing"
    ui_log_start="$(wc -l < "$LOG" | tr -d ' ')"
    run_cli_turn "What window is open in Safari right now? Use current local UI evidence." "$ui_out" || ok=1
    if [[ "$ok" -eq 0 ]]; then
        assert_contains "$ui_out" "Action started: Inspect windows: Safari." "UI request dispatched a host window inspection" || ok=1
        assert_not_contains "$ui_out" "I don't have access" "UI request did not become a model capability denial" || ok=1
        assert_not_contains "$ui_out" "I didn't produce a usable answer" "UI request returned an action receipt" || ok=1
    fi
    local ui_log
    ui_log="$(tail -n "+$((ui_log_start + 1))" "$LOG")"
    if ! grep -Fq "Required local UI evidence action dispatched without model generation" <<<"$ui_log"; then
        say "$FAIL" "read-only UI request did not use the required host-evidence path"
        ok=1
    fi
    if grep -Fq '"message":"Routing decision"' <<<"$ui_log"; then
        say "$FAIL" "read-only UI request reached model routing"
        ok=1
    fi

    # The original production failure only appeared across one continuous
    # conversation: fresh clipboard state hijacked an unrelated turn, local
    # RAM/CPU evidence lost its provenance on a PID follow-up, the uncertainty
    # sentinel searched the web for that machine-local PID, and a later model
    # turn falsely retracted the real measurements. Keep this sequence in one
    # CLI session so isolated happy-path checks cannot hide that regression.
    session_pid="$$"
    log_start="$(wc -l < "$LOG" | tr -d ' ')"
    say "$INFO" "checking multi-turn local evidence and truthfulness provenance"
    run_cli_session "$session_out" \
        --system-event clipboard_changed \
        '{"text":"catastrophic-clipboard-marker.zip"}' \
        "Let's do some testing. Reply briefly that you are ready." \
        "What is your current status?" \
        "Are accessibility and screen recording available?" \
        "what's using so much RAM right now?" \
        "refresh that RAM report" \
        "That's not what my Activity Monitor says. You just made that up." \
        "Show me the top five processes using CPU right now." \
        "What exactly is #1 from that CPU list? Identify it from the PID you just reported and give me a fresh local measurement." \
        "You didn't actually inspect that process. You made those numbers up, didn't you?" \
        "what about $session_pid?" \
        "that isn't accurate" \
        "where is the Why panel?" \
        "There is no toggle HUD command" \
        || ok=1

    if [[ "$ok" -eq 0 ]]; then
        assert_contains "$session_out" "Top memory users right now (Activity Monitor-style footprint):" "multi-turn RAM answer stayed deterministic" || ok=1
        assert_contains "$session_out" "Top CPU users right now:" "multi-turn CPU answer stayed deterministic" || ok=1
        assert_contains "$session_out" "Dexter status: **" "natural-language status used daemon evidence" || ok=1
        assert_contains "$session_out" "Runtime evidence: PID" "status included running-binary evidence" || ok=1
        assert_contains "$session_out" "Current macOS permission preflight for the running Dexter core" "permission question used live TCC preflight" || ok=1
        assert_contains "$session_out" "These are live macOS API results, not model assumptions" "permission answer states evidence provenance" || ok=1
        assert_contains "$session_out" "PID $session_pid is" "PID follow-up used local process evidence" || ok=1
        assert_contains "$session_out" "In the preceding CPU report, #1 was" "numbered CPU follow-up used the saved host report" || ok=1
        assert_contains "$session_out" "No. The prior PID answer came from Dexter's Rust host" "numbered PID challenge used deterministic provenance" || ok=1
        assert_contains "$session_out" "I did not use a web search" "PID follow-up states local-only provenance" || ok=1
        assert_contains "$session_out" "preceding RAM/CPU report was generated by Dexter's Rust host" "RAM correction used deterministic provenance" || ok=1
        assert_contains "$session_out" "question-mark button in Dexter's HUD menu" "Why location came from the Swift HUD contract" || ok=1
        assert_contains "$session_out" 'There is no `toggle HUD` or `make hud` command' "HUD answer rejects invented commands" || ok=1
        assert_not_contains "$session_out" "catastrophic-clipboard-marker" "unreferenced clipboard did not hijack chat" || ok=1
        assert_not_contains "$session_out" "Not much. You?" "status did not fall through to chat" || ok=1
        assert_not_contains "$session_out" "enable them via System Preferences" "permissions did not use generic model advice" || ok=1
        assert_not_contains "$session_out" "You're right — I made that up" "correction did not trigger a false confession" || ok=1
        assert_not_contains "$session_out" "I didn't actually inspect" "truth challenge did not trigger a false confession" || ok=1
        assert_not_contains "$session_out" "I did not actually inspect" "truth challenge did not trigger a false confession" || ok=1
        assert_not_contains "$session_out" "I don't have access" "session did not invent a local capability denial" || ok=1
        assert_not_contains "$session_out" "I didn't produce a usable answer" "every session turn produced a usable answer" || ok=1
    fi

    local session_log
    session_log="$(tail -n "+$((log_start + 1))" "$LOG")"
    local audit_count
    audit_count="$(grep -Fc "Deterministic local-evidence truthfulness audit completed" <<<"$session_log")"
    if [[ "$audit_count" -lt 3 ]]; then
        say "$FAIL" "RAM, numbered PID, and explicit PID corrections did not all use the deterministic evidence-audit route"
        ok=1
    fi
    if grep -Fq "Truthfulness challenge detected — upgrading FAST → PRIMARY" <<<"$session_log"; then
        say "$FAIL" "deterministic local evidence challenge unnecessarily invoked PRIMARY"
        ok=1
    fi
    if grep -Fq '"last_user_text_preview":"There is no toggle HUD command"' <<<"$session_log"; then
        say "$FAIL" "HUD contract correction reached model routing instead of host evidence"
        ok=1
    fi
    if grep -Eq "Web retrieval dispatch.*(pid|PID)[[:space:]]*$|Retrieval-first dispatch.*(pid|PID)" <<<"$session_log"; then
        say "$FAIL" "machine-local PID escaped into web retrieval"
        ok=1
    fi
    if ! grep -Fq "Required local UI evidence" <<<"$session_log" && grep -Fq "read-only UI snapshot" "$session_out"; then
        say "$FAIL" "read-only UI request did not enter the required-evidence path"
        ok=1
    fi

    if ! python3 - "$ROOT_DIR" "$CORE_PID" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve()
expected_pid = int(sys.argv[2])
state_root = pathlib.Path.home() / ".dexter" / "state" / "context_turns"
records = []
for path in state_root.glob("*/*.json"):
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        continue
    if data.get("user_text_preview") == "What is your current status?":
        records.append((data.get("updated_at", ""), data, path))
if not records:
    raise SystemExit("no persisted status turn record found")
_, record, path = max(records, key=lambda row: row[0])
runtime = record.get("runtime") or {}
actual_path = pathlib.Path(runtime.get("executable_path", "")).resolve()
expected_path = (root / "src/rust-core/target/release/dexter-core").resolve()
if runtime.get("process_id") != expected_pid:
    raise SystemExit(f"turn record PID mismatch in {path}: {runtime.get('process_id')} != {expected_pid}")
if actual_path != expected_path:
    raise SystemExit(f"turn record executable mismatch in {path}: {actual_path} != {expected_path}")
if not runtime.get("identity"):
    raise SystemExit(f"turn record runtime identity missing in {path}")
if len(runtime.get("executable_blake3") or "") != 64:
    raise SystemExit(f"turn record executable content hash missing in {path}")
evidence_sources = {item.get("source") for item in record.get("evidence", [])}
if "health_snapshot" not in evidence_sources:
    raise SystemExit(f"turn record health evidence missing in {path}: {evidence_sources}")

ram_records = []
for candidate in state_root.glob("*/*.json"):
    try:
        data = json.loads(candidate.read_text())
    except (OSError, json.JSONDecodeError):
        continue
    if data.get("user_text_preview") == "what's using so much RAM right now?":
        ram_records.append((data.get("updated_at", ""), data, candidate))
if not ram_records:
    raise SystemExit("no persisted RAM turn record found")
_, ram_record, ram_path = max(ram_records, key=lambda row: row[0])
sample = next(
    (
        item
        for item in ram_record.get("evidence", [])
        if item.get("source") == "macos_top_process_sample"
    ),
    None,
)
if not sample or not sample.get("payload_hash"):
    raise SystemExit(f"macOS top sample evidence missing in {ram_path}")

challenge_records = []
for candidate in state_root.glob("*/*.json"):
    try:
        data = json.loads(candidate.read_text())
    except (OSError, json.JSONDecodeError):
        continue
    if data.get("user_text_preview") == "You didn't actually inspect that process. You made those numbers up, didn't you?":
        challenge_records.append((data.get("updated_at", ""), data, candidate))
if not challenge_records:
    raise SystemExit("no persisted numbered-process challenge record found")
_, challenge, challenge_path = max(challenge_records, key=lambda row: row[0])
if challenge.get("route_category") != "DeterministicLocalEvidenceAudit":
    raise SystemExit(f"numbered-process challenge reached the wrong route in {challenge_path}")
challenge_sources = {item.get("source") for item in challenge.get("evidence", [])}
if "macos_process_identity_sample" not in challenge_sources:
    raise SystemExit(f"fresh process evidence missing from {challenge_path}: {challenge_sources}")
PY
    then
        say "$FAIL" "persisted runtime/evidence attestation did not match the launched release core"
        ok=1
    fi

    rm -f "$ram_out" "$cpu_out" "$notices_out" "$ui_out" "$session_out"

    if [[ "$ok" -ne 0 ]]; then
        fail "local deterministic answer smoke failed"
    fi

    say "$PASS" "local deterministic answers smoke passed"
}

main "$@"
