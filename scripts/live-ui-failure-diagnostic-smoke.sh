#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ui-failure-diagnostic-core.log"
MISSING_ACTION_OUT="/tmp/dexter-ui-failure-missing-action.out"
MISSING_WHY_OUT="/tmp/dexter-ui-failure-missing-why.out"
MISSING_LAST_OUT="/tmp/dexter-ui-failure-missing-last.out"
AMBIGUOUS_ACTION_OUT="/tmp/dexter-ui-failure-ambiguous-action.out"
AMBIGUOUS_WHY_OUT="/tmp/dexter-ui-failure-ambiguous-why.out"
AMBIGUOUS_LAST_OUT="/tmp/dexter-ui-failure-ambiguous-last.out"
FIXTURE_SWIFT="/tmp/DexterUIFailureFixture.swift"
FIXTURE_BIN="/tmp/DexterUIFailureFixture"
FIXTURE_PID=""
SOCKET="/tmp/dexter.sock"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    for file in \
        "$MISSING_ACTION_OUT" "$MISSING_LAST_OUT" "$MISSING_WHY_OUT" \
        "$AMBIGUOUS_ACTION_OUT" "$AMBIGUOUS_LAST_OUT" "$AMBIGUOUS_WHY_OUT"; do
        if [[ -f "$file" ]]; then
            say INFO "$file:"
            cat "$file" || true
        fi
    done
    if [[ -f "$CORE_LOG" ]]; then
        say INFO "core log tail:"
        tail -n 100 "$CORE_LOG" || true
    fi
    exit 1
}

socket_accepts() {
    python3 - "$SOCKET" <<'PY'
import socket
import sys

path = sys.argv[1]
s = socket.socket(socket.AF_UNIX)
s.settimeout(1)
sys.exit(0 if s.connect_ex(path) == 0 else 1)
PY
}

json_action() {
    local label="$1"
    python3 - "$label" <<'PY'
import json
import sys

print(json.dumps({
    "type": "ui_click",
    "app_name": "DexterUIFailureFixture",
    "role": "AXButton",
    "label": sys.argv[1],
    "max_depth": 5,
    "rationale": "UI_FAILURE_DIAGNOSTIC_SMOKE exercise typed UI failure receipts"
}))
PY
}

write_fixture_source() {
    cat >"$FIXTURE_SWIFT" <<'SWIFT'
import AppKit
import Foundation

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 160, y: 160, width: 420, height: 190),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter UI Failure Fixture"

        let label = NSTextField(labelWithString: "Two identical OK buttons, no Save button")
        label.frame = NSRect(x: 40, y: 124, width: 340, height: 24)

        let firstButton = NSButton(title: "OK", target: nil, action: nil)
        firstButton.frame = NSRect(x: 74, y: 56, width: 110, height: 34)
        firstButton.identifier = NSUserInterfaceItemIdentifier("firstOK")
        firstButton.setAccessibilityLabel("OK")
        firstButton.setAccessibilityIdentifier("firstOK")

        let secondButton = NSButton(title: "OK", target: nil, action: nil)
        secondButton.frame = NSRect(x: 236, y: 56, width: 110, height: 34)
        secondButton.identifier = NSUserInterfaceItemIdentifier("secondOK")
        secondButton.setAccessibilityLabel("OK")
        secondButton.setAccessibilityIdentifier("secondOK")

        window.contentView?.addSubview(label)
        window.contentView?.addSubview(firstButton)
        window.contentView?.addSubview(secondButton)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        self.window = window
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
SWIFT
}

cleanup() {
    if [[ -n "$FIXTURE_PID" ]]; then
        kill "$FIXTURE_PID" >/dev/null 2>&1 || true
        wait "$FIXTURE_PID" >/dev/null 2>&1 || true
    fi
    pkill -f "$FIXTURE_BIN" >/dev/null 2>&1 || true
    rm -f "$FIXTURE_SWIFT" "$FIXTURE_BIN"
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

assert_contains() {
    local file="$1"
    local pattern="$2"
    local label="$3"
    if ! grep -Fq "$pattern" "$file"; then
        fail "$label - missing: $pattern"
    fi
}

run_case() {
    local label="$1"
    local action_out="$2"
    local last_out="$3"
    local why_out="$4"
    local expected_kind="$5"
    local expected_directive="$6"
    local expected_next_step="$7"
    local expected_evidence="$8"

    "$CLI_BIN" --idle-timeout 180 --action-json "$(json_action "$label")" >"$action_out" 2>&1 \
        || fail "ui_click failure case for '$label' did not return cleanly to CLI"

    assert_contains "$action_out" "[ACTION RECEIPT" "ui_click '$label' emitted receipt"
    assert_contains "$action_out" "ui_click" "ui_click '$label' action type surfaced"
    assert_contains "$action_out" "outcome=failed" "ui_click '$label' recorded failed action"
    assert_contains "$action_out" "UI failure [$expected_kind]" "ui_click '$label' typed failure surfaced"
    assert_contains "$action_out" "Target: action=ui_click" "ui_click '$label' target evidence surfaced"
    assert_contains "$action_out" "app=DexterUIFailureFixture" "ui_click '$label' app evidence surfaced"
    assert_contains "$action_out" "window='Dexter UI Failure Fixture'" "ui_click '$label' window evidence surfaced"
    assert_contains "$action_out" "role=AXButton" "ui_click '$label' role evidence surfaced"
    assert_contains "$action_out" "label='$label'" "ui_click '$label' label evidence surfaced"
    assert_contains "$action_out" "$expected_evidence" "ui_click '$label' replan evidence surfaced"
    assert_contains "$action_out" "Next [$expected_directive]" "ui_click '$label' recovery directive surfaced"

    "$CLI_BIN" --actions last >"$last_out" 2>&1 \
        || fail "ui_click '$label' latest receipt inspection failed"
    assert_contains "$last_out" "UI failed ($expected_kind)" "ui_click '$label' latest receipt summarized typed failure"
    assert_contains "$last_out" "UI click: DexterUIFailureFixture AXButton \"$label\"" "ui_click '$label' latest receipt preserved target"
    assert_contains "$last_out" "$expected_evidence" "ui_click '$label' latest receipt preserved evidence"

    "$CLI_BIN" --why >"$why_out" 2>&1 \
        || fail "ui_click '$label' why inspection failed"
    assert_contains "$why_out" "### Action Diagnostic" "ui_click '$label' why header"
    assert_contains "$why_out" "UI automation failed after the action was dispatched ($expected_kind)." "ui_click '$label' why typed cause"
    assert_contains "$why_out" "$expected_evidence" "ui_click '$label' why preserved evidence"
    assert_contains "$why_out" "$expected_next_step" "ui_click '$label' why recovery guidance"
}

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" \
    "$MISSING_ACTION_OUT" "$MISSING_LAST_OUT" "$MISSING_WHY_OUT" \
    "$AMBIGUOUS_ACTION_OUT" "$AMBIGUOUS_LAST_OUT" "$AMBIGUOUS_WHY_OUT" \
    "$FIXTURE_SWIFT" "$FIXTURE_BIN"

say INFO "building temporary AppKit UI failure fixture"
write_fixture_source
swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

say INFO "starting temporary UI failure fixture"
"$FIXTURE_BIN" >/tmp/dexter-ui-failure-fixture.log 2>&1 &
FIXTURE_PID="$!"

say INFO "waiting for fixture Accessibility surface"
for _ in {1..60}; do
    if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUIFailureFixture"
    if (count of matchingProcesses) is 0 then error "fixture not running" number 1728
    set targetProcess to item 1 of matchingProcesses
    set targetWindow to front window of targetProcess
    set targetButtons to buttons of targetWindow whose name is "OK"
    if (count of targetButtons) is less than 2 then error "duplicate buttons not exposed" number 1728
end tell
APPLESCRIPT
    then
        break
    fi
    sleep 0.25
done

say INFO "driving missing-control ui_click diagnostic"
run_case \
    "Save" \
    "$MISSING_ACTION_OUT" \
    "$MISSING_LAST_OUT" \
    "$MISSING_WHY_OUT" \
    "control_not_found" \
    "snapshot_then_replan" \
    "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
    "nearest_safe_candidates:"

say INFO "driving ambiguous-control ui_click diagnostic"
run_case \
    "OK" \
    "$AMBIGUOUS_ACTION_OUT" \
    "$AMBIGUOUS_LAST_OUT" \
    "$AMBIGUOUS_WHY_OUT" \
    "ambiguous_control" \
    "ask_for_clarification" \
    "Ask which matching control to use, or collect a UI snapshot and choose a more specific target." \
    "match_count=2 candidates:"

say PASS "UI failure diagnostic smoke passed"
