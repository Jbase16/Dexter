#!/usr/bin/env bash
# scripts/live-hud-ui-failure-smoke.sh - Swift HUD typed UI failure receipt smoke.
#
# Starts the real Rust core, creates typed UI failure receipts through
# dexter-cli, then verifies the Swift HUD Recent Actions and Why surfaces render
# the same typed kind and recovery directive instead of a generic failure.

set -u
set -o pipefail

SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
CORE_LOG="/tmp/dexter-hud-ui-failure-core-smoke.log"
SWIFT_HISTORY_LOG="/tmp/dexter-hud-ui-failure-history-swift-smoke.log"
SWIFT_MISSING_WHY_LOG="/tmp/dexter-hud-ui-failure-missing-why-swift-smoke.log"
SWIFT_AMBIGUOUS_WHY_LOG="/tmp/dexter-hud-ui-failure-ambiguous-why-swift-smoke.log"
SWIFT_TYPE_WHY_LOG="/tmp/dexter-hud-ui-failure-type-why-swift-smoke.log"
SWIFT_SELECT_WHY_LOG="/tmp/dexter-hud-ui-failure-select-why-swift-smoke.log"
SWIFT_TOGGLE_WHY_LOG="/tmp/dexter-hud-ui-failure-toggle-why-swift-smoke.log"
SWIFT_PICK_WHY_LOG="/tmp/dexter-hud-ui-failure-pick-why-swift-smoke.log"
SWIFT_DISABLED_CLICK_WHY_LOG="/tmp/dexter-hud-ui-failure-disabled-click-why-swift-smoke.log"
SWIFT_DISABLED_TYPE_WHY_LOG="/tmp/dexter-hud-ui-failure-disabled-type-why-swift-smoke.log"
SWIFT_DISABLED_SELECT_WHY_LOG="/tmp/dexter-hud-ui-failure-disabled-select-why-swift-smoke.log"
SWIFT_DISABLED_TOGGLE_WHY_LOG="/tmp/dexter-hud-ui-failure-disabled-toggle-why-swift-smoke.log"
CLI_LOG="/tmp/dexter-hud-ui-failure-cli-smoke.log"
ACTION_OUT="/tmp/dexter-hud-ui-failure-action.out"
FIXTURE_SWIFT="/tmp/DexterHUDUIFailureFixture.swift"
FIXTURE_BIN="/tmp/DexterHUDUIFailureFixture"
START_CORE=0
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT_DIR/scripts/lib/process-tree.sh"
CORE_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-core"
CLI_BIN="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
SWIFT_DIR="$ROOT_DIR/src/swift"
CORE_PID=""
SWIFT_PID=""
FIXTURE_PID=""
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"

PASS="PASS"
FAIL="FAIL"
INFO="INFO"

if [[ "${1:-}" == "--start-core" ]]; then
    START_CORE=1
    shift
fi

say() {
    printf '[%s] %s\n' "$1" "$2"
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

json_action() {
    local action_key="$1"
    python3 - "$action_key" <<'PY'
import json
import sys

action_key = sys.argv[1]
actions = {
    "missing_click": {
        "type": "ui_click",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXButton",
        "label": "Save",
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed missing ui_click receipt",
    },
    "ambiguous_click": {
        "type": "ui_click",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXButton",
        "label": "OK",
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed ambiguous ui_click receipt",
    },
    "disabled_click": {
        "type": "ui_click",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXButton",
        "label": "Disabled action",
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed disabled ui_click receipt",
    },
    "missing_type": {
        "type": "ui_type",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXTextField",
        "label": "Missing text field",
        "text": "hud smoke secret text must stay redacted in evidence",
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed missing ui_type receipt",
    },
    "disabled_type": {
        "type": "ui_type",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXTextField",
        "label": "Disabled text field",
        "text": "hud smoke disabled secret must stay redacted in evidence",
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed disabled ui_type receipt",
    },
    "missing_select": {
        "type": "ui_select",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXPopUpButton",
        "label": "Missing flavor selector",
        "option": "Chocolate",
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed missing ui_select receipt",
    },
    "disabled_select": {
        "type": "ui_select",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXPopUpButton",
        "label": "Disabled flavor",
        "option": "Chocolate",
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed disabled ui_select receipt",
    },
    "missing_toggle": {
        "type": "ui_toggle",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXCheckBox",
        "label": "Missing toggle",
        "state": True,
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed missing ui_toggle receipt",
    },
    "disabled_toggle": {
        "type": "ui_toggle",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXCheckBox",
        "label": "Disabled toggle",
        "state": True,
        "max_depth": 5,
        "rationale": "HUD_UI_FAILURE_SMOKE seed disabled ui_toggle receipt",
    },
    "missing_pick": {
        "type": "ui_pick",
        "app_name": "DexterHUDUIFailureFixture",
        "role": "AXRow",
        "label": "Missing row",
        "container_label": None,
        "max_depth": 7,
        "rationale": "HUD_UI_FAILURE_SMOKE seed missing ui_pick receipt",
    },
}
print(json.dumps(actions[action_key]))
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
            contentRect: NSRect(x: 180, y: 180, width: 420, height: 190),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter HUD UI Failure Fixture"

        let label = NSTextField(labelWithString: "Duplicate OK buttons, no Save button")
        label.frame = NSRect(x: 40, y: 144, width: 340, height: 24)

        let firstButton = NSButton(title: "OK", target: nil, action: nil)
        firstButton.frame = NSRect(x: 74, y: 84, width: 110, height: 34)
        firstButton.identifier = NSUserInterfaceItemIdentifier("firstOK")
        firstButton.setAccessibilityLabel("OK")
        firstButton.setAccessibilityIdentifier("firstOK")

        let secondButton = NSButton(title: "OK", target: nil, action: nil)
        secondButton.frame = NSRect(x: 236, y: 84, width: 110, height: 34)
        secondButton.identifier = NSUserInterfaceItemIdentifier("secondOK")
        secondButton.setAccessibilityLabel("OK")
        secondButton.setAccessibilityIdentifier("secondOK")

        let disabledButton = NSButton(title: "Disabled action", target: nil, action: nil)
        disabledButton.frame = NSRect(x: 40, y: 114, width: 145, height: 28)
        disabledButton.isEnabled = false
        disabledButton.identifier = NSUserInterfaceItemIdentifier("disabledAction")
        disabledButton.setAccessibilityLabel("Disabled action")
        disabledButton.setAccessibilityIdentifier("disabledAction")

        window.contentView?.addSubview(label)
        window.contentView?.addSubview(firstButton)
        window.contentView?.addSubview(secondButton)
        window.contentView?.addSubview(disabledButton)

        let textField = NSTextField(frame: NSRect(x: 40, y: 34, width: 130, height: 28))
        textField.placeholderString = "Existing text field"
        textField.setAccessibilityLabel("Existing text field")
        textField.setAccessibilityIdentifier("existingTextField")

        let disabledTextField = NSTextField(frame: NSRect(x: 40, y: 4, width: 130, height: 26))
        disabledTextField.placeholderString = "Disabled text field"
        disabledTextField.isEnabled = false
        disabledTextField.setAccessibilityLabel("Disabled text field")
        disabledTextField.setAccessibilityIdentifier("disabledTextField")

        let checkbox = NSButton(checkboxWithTitle: "Existing toggle", target: nil, action: nil)
        checkbox.frame = NSRect(x: 205, y: 34, width: 160, height: 28)
        checkbox.setAccessibilityLabel("Existing toggle")
        checkbox.setAccessibilityIdentifier("existingToggle")

        let disabledCheckbox = NSButton(checkboxWithTitle: "Disabled toggle", target: nil, action: nil)
        disabledCheckbox.frame = NSRect(x: 205, y: 4, width: 160, height: 26)
        disabledCheckbox.isEnabled = false
        disabledCheckbox.setAccessibilityLabel("Disabled toggle")
        disabledCheckbox.setAccessibilityIdentifier("disabledToggle")

        let disabledPopup = NSPopUpButton(frame: NSRect(x: 205, y: 114, width: 160, height: 28), pullsDown: false)
        disabledPopup.addItems(withTitles: ["Vanilla", "Chocolate"])
        disabledPopup.selectItem(withTitle: "Vanilla")
        disabledPopup.isEnabled = false
        disabledPopup.setAccessibilityLabel("Disabled flavor")
        disabledPopup.setAccessibilityIdentifier("disabledFlavor")

        window.contentView?.addSubview(textField)
        window.contentView?.addSubview(disabledTextField)
        window.contentView?.addSubview(checkbox)
        window.contentView?.addSubview(disabledCheckbox)
        window.contentView?.addSubview(disabledPopup)
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
    if [[ -n "$SWIFT_PID" ]]; then
        stop_process_tree "$SWIFT_PID"
        wait "$SWIFT_PID" >/dev/null 2>&1 || true
        SWIFT_PID=""
    fi
    if [[ -n "$FIXTURE_PID" ]]; then
        kill "$FIXTURE_PID" >/dev/null 2>&1 || true
        wait "$FIXTURE_PID" >/dev/null 2>&1 || true
        FIXTURE_PID=""
    fi
    pkill -f "$FIXTURE_BIN" >/dev/null 2>&1 || true
    if [[ -n "$CORE_PID" ]]; then
        stop_process_tree "$CORE_PID"
        wait "$CORE_PID" >/dev/null 2>&1 || true
        CORE_PID=""
    fi
    rm -f "$FIXTURE_SWIFT" "$FIXTURE_BIN"
}
trap cleanup EXIT INT TERM

require_bins() {
    if [[ ! -x "$CORE_BIN" ]]; then
        say "$FAIL" "missing core binary: $CORE_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-core --bin dexter-cli"
        exit 2
    fi
    if [[ ! -x "$CLI_BIN" ]]; then
        say "$FAIL" "missing CLI binary: $CLI_BIN"
        say "$INFO" "build it with: cd src/rust-core && cargo build --release --bin dexter-cli"
        exit 2
    fi
}

start_core_if_requested() {
    if [[ "$START_CORE" -ne 1 ]]; then
        if ! socket_accepts; then
            say "$FAIL" "no Dexter daemon accepting connections at $SOCKET"
            exit 2
        fi
        return
    fi

    if socket_accepts; then
        say "$FAIL" "a Dexter daemon is already accepting connections at $SOCKET"
        say "$INFO" "stop it first, or run this script without --start-core against that daemon"
        exit 2
    fi

    rm -f "$SOCKET" "$SHELL_SOCKET"
    : > "$CORE_LOG"
    say "$INFO" "starting release core; log: $CORE_LOG"
    RUST_LOG=info "$CORE_BIN" >> "$CORE_LOG" 2>&1 &
    CORE_PID="$!"

    local waited=0
    while [[ "$waited" -lt 90 ]]; do
        if socket_accepts; then
            break
        fi
        if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say "$FAIL" "core exited before opening socket"
            tail -80 "$CORE_LOG" || true
            exit 2
        fi
        sleep 1
        waited=$((waited + 1))
    done
    if ! socket_accepts; then
        say "$FAIL" "core did not open $SOCKET within 90s"
        tail -80 "$CORE_LOG" || true
        exit 2
    fi

    waited=0
    while [[ "$waited" -lt "$CORE_WARMUP_TIMEOUT_SECS" ]]; do
        "$CLI_BIN" --doctor >/tmp/dexter-hud-ui-failure-doctor.out 2>&1 || true
        if grep -Fq "OK   daemon health      status ready" /tmp/dexter-hud-ui-failure-doctor.out \
            && grep -Fq "Result: OK - no failed checks." /tmp/dexter-hud-ui-failure-doctor.out; then
            say "$INFO" "core doctor-ready after ${waited}s"
            return
        fi
        if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            say "$FAIL" "core exited during startup"
            tail -120 "$CORE_LOG" || true
            exit 2
        fi
        sleep 2
        waited=$((waited + 2))
    done

    say "$FAIL" "core socket opened, but doctor-ready did not complete within ${CORE_WARMUP_TIMEOUT_SECS}s"
    cat /tmp/dexter-hud-ui-failure-doctor.out 2>/dev/null || true
    tail -120 "$CORE_LOG" || true
    exit 2
}

wait_for_pattern() {
    local file="$1"
    local pattern="$2"
    local timeout_secs="$3"
    local waited=0
    while [[ "$waited" -lt "$timeout_secs" ]]; do
        if grep -Fq "$pattern" "$file"; then
            return 0
        fi
        if [[ "$file" == /tmp/dexter-hud-ui-failure-*-swift-smoke.log && -n "$SWIFT_PID" ]] && ! kill -0 "$SWIFT_PID" >/dev/null 2>&1; then
            grep -Fq "$pattern" "$file" && return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

assert_contains() {
    local label="$1"
    local file="$2"
    local pattern="$3"
    if ! grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - missing pattern: $pattern"
        return 1
    fi
    return 0
}

assert_absent() {
    local label="$1"
    local file="$2"
    local pattern="$3"
    if grep -Fq "$pattern" "$file"; then
        say "$FAIL" "$label - unexpected pattern: $pattern"
        return 1
    fi
    return 0
}

start_fixture() {
    say "$INFO" "building temporary HUD UI failure fixture"
    write_fixture_source
    swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"

    say "$INFO" "starting temporary HUD UI failure fixture"
    "$FIXTURE_BIN" >/tmp/dexter-hud-ui-failure-fixture.log 2>&1 &
    FIXTURE_PID="$!"

    say "$INFO" "waiting for fixture Accessibility surface"
    for _ in {1..80}; do
        if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterHUDUIFailureFixture"
    if (count of matchingProcesses) is 0 then error "fixture not running" number 1728
    set targetProcess to item 1 of matchingProcesses
    if (count of UI elements of targetProcess) is 0 then error "fixture not exposed" number 1728
end tell
APPLESCRIPT
        then
            return 0
        fi
        sleep 0.25
    done
    say "$FAIL" "fixture Accessibility surface did not become ready"
    exit 1
}

seed_ui_failure() {
    local action_key="$1"
    local expected_kind="$2"
    local action_json
    action_json="$(json_action "$action_key")"
    : > "$ACTION_OUT"
    "$CLI_BIN" --quiet --idle-timeout 180 --action-json "$action_json" >"$ACTION_OUT" 2>&1 || {
        say "$FAIL" "failed to seed UI failure for $action_key"
        cat "$ACTION_OUT" || true
        exit 1
    }
    assert_contains "seed UI failure $action_key" "$ACTION_OUT" "UI failure [$expected_kind]" || {
        cat "$ACTION_OUT" || true
        exit 1
    }
}

start_swift_smoke() {
    local mode="$1"
    local log_file="$2"
    : > "$log_file"
    say "$INFO" "starting Swift HUD $mode smoke; log: $log_file"
    (
        cd "$SWIFT_DIR" || exit 2
        DEXTER_HUD_SMOKE=1 \
        DEXTER_HUD_SMOKE_ACTION_HISTORY="$([[ "$mode" == "history" ]] && echo 1 || echo 0)" \
        DEXTER_HUD_SMOKE_ACTION_DIAGNOSTIC="$([[ "$mode" == "diagnostic" ]] && echo 1 || echo 0)" \
        DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS="${DEXTER_HUD_SMOKE_SUBMIT_DELAY_SECS:-1}" \
        DEXTER_HUD_SMOKE_EXIT_AFTER_SECS="${DEXTER_HUD_SMOKE_EXIT_AFTER_SECS:-10}" \
            swift run
    ) >> "$log_file" 2>&1 &
    SWIFT_PID="$!"
}

stop_swift() {
    if [[ -n "$SWIFT_PID" ]]; then
        stop_process_tree "$SWIFT_PID"
        wait "$SWIFT_PID" >/dev/null 2>&1 || true
        SWIFT_PID=""
    fi
}

run_history_probe() {
    local ok=0
    start_swift_smoke "history" "$SWIFT_HISTORY_LOG"

    wait_for_pattern "$SWIFT_HISTORY_LOG" "[DexterClient] ActionHistory RPC OK" 60 || {
        say "$FAIL" "Swift HUD UI failure history smoke - ActionHistory RPC did not complete"
        tail -180 "$SWIFT_HISTORY_LOG" || true
        return 1
    }

    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "[HUDSmoke] actionHistoryRequest" || ok=1
    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "[HUDSmoke] showActionHistory" || ok=1
    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "Recent Receipts" || ok=1
    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "not_selectable" || ok=1
    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "snapshot_then_replan" || ok=1
    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "ambiguous_control" || ok=1
    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "ask_for_clarification" || ok=1
    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "Target: action=ui_pick" || ok=1
    assert_contains "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "nearest_safe_candidates:" || ok=1
    assert_absent "Swift HUD UI failure history smoke" "$SWIFT_HISTORY_LOG" "Result: Action failed." || ok=1
    stop_swift
    return "$ok"
}

run_diagnostic_probe() {
    local log_file="$1"
    local expected_kind="$2"
    local expected_step="$3"
    local expected_evidence="$4"
    local ok=0
    start_swift_smoke "diagnostic" "$log_file"

    wait_for_pattern "$log_file" "[DexterClient] ActionDiagnostic report generated" 60 || {
        say "$FAIL" "Swift HUD UI failure diagnostic smoke - ActionDiagnostic RPC did not complete"
        tail -180 "$log_file" || true
        return 1
    }

    assert_contains "Swift HUD UI failure diagnostic smoke" "$log_file" "[HUDSmoke] actionDiagnosticRequest" || ok=1
    assert_contains "Swift HUD UI failure diagnostic smoke" "$log_file" "[HUDSmoke] showActionDiagnostic" || ok=1
    assert_contains "Swift HUD UI failure diagnostic smoke" "$log_file" "UI automation failed after the action was dispatched ($expected_kind)." || ok=1
    assert_contains "Swift HUD UI failure diagnostic smoke" "$log_file" "$expected_step" || ok=1
    assert_contains "Swift HUD UI failure diagnostic smoke" "$log_file" "$expected_evidence" || ok=1
    assert_absent "Swift HUD UI failure diagnostic smoke" "$log_file" "Action failed." || ok=1
    stop_swift
    return "$ok"
}

main() {
    require_bins
    start_core_if_requested
    start_fixture

    local ok=0

    seed_ui_failure "missing_click" "control_not_found"
    run_diagnostic_probe \
        "$SWIFT_MISSING_WHY_LOG" \
        "control_not_found" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "nearest_safe_candidates:" || ok=1

    seed_ui_failure "missing_type" "not_typeable"
    run_diagnostic_probe \
        "$SWIFT_TYPE_WHY_LOG" \
        "not_typeable" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_type" || ok=1
    assert_contains "Swift HUD UI failure type smoke" "$SWIFT_TYPE_WHY_LOG" "text=<redacted>" || ok=1
    assert_contains "Swift HUD UI failure type smoke" "$SWIFT_TYPE_WHY_LOG" "nearest_safe_candidates:" || ok=1
    assert_absent "Swift HUD UI failure type smoke" "$SWIFT_TYPE_WHY_LOG" "hud smoke secret text must stay redacted in evidence" || ok=1

    seed_ui_failure "disabled_click" "control_disabled"
    run_diagnostic_probe \
        "$SWIFT_DISABLED_CLICK_WHY_LOG" \
        "control_disabled" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_click" || ok=1
    assert_contains "Swift HUD UI failure disabled click smoke" "$SWIFT_DISABLED_CLICK_WHY_LOG" "Evidence: matched_control:" || ok=1
    assert_contains "Swift HUD UI failure disabled click smoke" "$SWIFT_DISABLED_CLICK_WHY_LOG" "enabled=false" || ok=1

    seed_ui_failure "disabled_type" "control_disabled"
    run_diagnostic_probe \
        "$SWIFT_DISABLED_TYPE_WHY_LOG" \
        "control_disabled" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_type" || ok=1
    assert_contains "Swift HUD UI failure disabled type smoke" "$SWIFT_DISABLED_TYPE_WHY_LOG" "text=<redacted>" || ok=1
    assert_contains "Swift HUD UI failure disabled type smoke" "$SWIFT_DISABLED_TYPE_WHY_LOG" "Evidence: matched_control:" || ok=1
    assert_contains "Swift HUD UI failure disabled type smoke" "$SWIFT_DISABLED_TYPE_WHY_LOG" "enabled=false" || ok=1
    assert_absent "Swift HUD UI failure disabled type smoke" "$SWIFT_DISABLED_TYPE_WHY_LOG" "hud smoke disabled secret must stay redacted in evidence" || ok=1

    seed_ui_failure "missing_select" "not_selectable"
    run_diagnostic_probe \
        "$SWIFT_SELECT_WHY_LOG" \
        "not_selectable" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_select" || ok=1
    assert_contains "Swift HUD UI failure select smoke" "$SWIFT_SELECT_WHY_LOG" "option='Chocolate'" || ok=1
    assert_contains "Swift HUD UI failure select smoke" "$SWIFT_SELECT_WHY_LOG" "nearest_safe_candidates:" || ok=1

    seed_ui_failure "disabled_select" "control_disabled"
    run_diagnostic_probe \
        "$SWIFT_DISABLED_SELECT_WHY_LOG" \
        "control_disabled" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_select" || ok=1
    assert_contains "Swift HUD UI failure disabled select smoke" "$SWIFT_DISABLED_SELECT_WHY_LOG" "option='Chocolate'" || ok=1
    assert_contains "Swift HUD UI failure disabled select smoke" "$SWIFT_DISABLED_SELECT_WHY_LOG" "Evidence: matched_control:" || ok=1
    assert_contains "Swift HUD UI failure disabled select smoke" "$SWIFT_DISABLED_SELECT_WHY_LOG" "enabled=false" || ok=1

    seed_ui_failure "missing_toggle" "not_selectable"
    run_diagnostic_probe \
        "$SWIFT_TOGGLE_WHY_LOG" \
        "not_selectable" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_toggle" || ok=1
    assert_contains "Swift HUD UI failure toggle smoke" "$SWIFT_TOGGLE_WHY_LOG" "state=on" || ok=1
    assert_contains "Swift HUD UI failure toggle smoke" "$SWIFT_TOGGLE_WHY_LOG" "nearest_safe_candidates:" || ok=1

    seed_ui_failure "disabled_toggle" "control_disabled"
    run_diagnostic_probe \
        "$SWIFT_DISABLED_TOGGLE_WHY_LOG" \
        "control_disabled" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_toggle" || ok=1
    assert_contains "Swift HUD UI failure disabled toggle smoke" "$SWIFT_DISABLED_TOGGLE_WHY_LOG" "state=on" || ok=1
    assert_contains "Swift HUD UI failure disabled toggle smoke" "$SWIFT_DISABLED_TOGGLE_WHY_LOG" "Evidence: matched_control:" || ok=1
    assert_contains "Swift HUD UI failure disabled toggle smoke" "$SWIFT_DISABLED_TOGGLE_WHY_LOG" "enabled=false" || ok=1

    seed_ui_failure "missing_pick" "not_selectable"
    run_diagnostic_probe \
        "$SWIFT_PICK_WHY_LOG" \
        "not_selectable" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_pick" || ok=1
    assert_contains "Swift HUD UI failure pick smoke" "$SWIFT_PICK_WHY_LOG" "container='<none>'" || ok=1
    assert_contains "Swift HUD UI failure pick smoke" "$SWIFT_PICK_WHY_LOG" "nearest_safe_candidates:" || ok=1

    seed_ui_failure "ambiguous_click" "ambiguous_control"
    run_history_probe || ok=1
    run_diagnostic_probe \
        "$SWIFT_AMBIGUOUS_WHY_LOG" \
        "ambiguous_control" \
        "Ask which matching control to use, or collect a UI snapshot and choose a more specific target." \
        "match_count=2 candidates:" || ok=1

    assert_contains "Swift HUD UI failure smoke" "$CORE_LOG" "Action history requested" || ok=1
    assert_contains "Swift HUD UI failure smoke" "$CORE_LOG" "Action diagnostic requested" || ok=1

    if [[ "$ok" -eq 0 ]]; then
        say "$PASS" "Swift HUD UI failure smoke passed"
    else
        tail -180 "$SWIFT_HISTORY_LOG" || true
        tail -180 "$SWIFT_MISSING_WHY_LOG" || true
        tail -180 "$SWIFT_AMBIGUOUS_WHY_LOG" || true
        tail -180 "$SWIFT_TYPE_WHY_LOG" || true
        tail -180 "$SWIFT_SELECT_WHY_LOG" || true
        tail -180 "$SWIFT_TOGGLE_WHY_LOG" || true
        tail -180 "$SWIFT_PICK_WHY_LOG" || true
        tail -180 "$SWIFT_DISABLED_CLICK_WHY_LOG" || true
        tail -180 "$SWIFT_DISABLED_TYPE_WHY_LOG" || true
        tail -180 "$SWIFT_DISABLED_SELECT_WHY_LOG" || true
        tail -180 "$SWIFT_DISABLED_TOGGLE_WHY_LOG" || true
        tail -160 "$CORE_LOG" || true
        cat "$ACTION_OUT" || true
        cat "$CLI_LOG" || true
        exit 1
    fi
}

main "$@"
