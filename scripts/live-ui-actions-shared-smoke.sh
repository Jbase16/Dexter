#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CORE_BIN="$RUST_DIR/target/release/dexter-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
CORE_LOG="/tmp/dexter-ui-actions-shared-core.log"
ACTION_OUT="/tmp/dexter-ui-actions-shared-action.out"
LAST_OUT="/tmp/dexter-ui-actions-shared-last.out"
WHY_OUT="/tmp/dexter-ui-actions-shared-why.out"
FIXTURE_SWIFT="/tmp/DexterUISharedFixture.swift"
FIXTURE_BIN="/tmp/DexterUISharedFixture"
TEXT_OUT="/tmp/dexter-ui-shared-text.txt"
SELECT_OUT="/tmp/dexter-ui-shared-select.txt"
TOGGLE_OUT="/tmp/dexter-ui-shared-toggle.txt"
PICK_OUT="/tmp/dexter-ui-shared-pick.txt"
CLICK_OUT="/tmp/dexter-ui-shared-click.txt"
FIXTURE_PID=""
CORE_PID=""
START_CORE=1
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"
SMOKE_TEXT="Dexter typed this through shared ui_type."
SMOKE_OPTION="Chocolate"
SMOKE_ITEM="Invoices"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --existing-core)
            START_CORE=0
            shift
            if [[ "$#" -gt 0 ]]; then
                CORE_LOG="$1"
                shift
            fi
            ;;
        *)
            CORE_LOG="$1"
            shift
            ;;
    esac
done

fail() {
    say FAIL "$*"
    for file in "$ACTION_OUT" "$LAST_OUT" "$WHY_OUT" "$TEXT_OUT" "$SELECT_OUT" "$TOGGLE_OUT" "$PICK_OUT" "$CLICK_OUT"; do
        if [[ -f "$file" ]]; then
            say INFO "$file:"
            cat "$file" || true
        fi
    done
    if [[ -f "$CORE_LOG" ]]; then
        say INFO "core log tail:"
        tail -n 120 "$CORE_LOG" || true
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
    local type="$1"
    python3 - "$type" "$SMOKE_TEXT" "$SMOKE_OPTION" "$SMOKE_ITEM" <<'PY'
import json
import sys

action_type, smoke_text, smoke_option, smoke_item = sys.argv[1:5]
actions = {
    "window_focus": {
        "type": "window_focus",
        "app_name": "Finder",
        "title_contains": None,
        "rationale": "UI_ACTIONS_SHARED focus a stable local app window",
    },
    "window_inspect": {
        "type": "window_inspect",
        "app_name": "DexterUISharedFixture",
        "rationale": "UI_ACTIONS_SHARED inspect the shared fixture window",
    },
    "ui_snapshot": {
        "type": "ui_snapshot",
        "app_name": "DexterUISharedFixture",
        "max_depth": 2,
        "rationale": "UI_ACTIONS_SHARED snapshot the shared fixture UI",
    },
    "ui_click": {
        "type": "ui_click",
        "app_name": "DexterUISharedFixture",
        "role": "AXButton",
        "label": "Continue",
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED click an unambiguous fixture button",
    },
    "ui_type": {
        "type": "ui_type",
        "app_name": "DexterUISharedFixture",
        "role": "AXTextField",
        "label": "Dexter smoke input",
        "text": smoke_text,
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED type into the fixture text field",
    },
    "ui_select": {
        "type": "ui_select",
        "app_name": "DexterUISharedFixture",
        "role": "AXPopUpButton",
        "label": "Dessert flavor",
        "option": smoke_option,
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED select a fixture popup option",
    },
    "ui_toggle": {
        "type": "ui_toggle",
        "app_name": "DexterUISharedFixture",
        "role": "AXCheckBox",
        "label": "Enable sprinkles",
        "state": True,
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED toggle the fixture checkbox on",
    },
    "ui_pick": {
        "type": "ui_pick",
        "app_name": "DexterUISharedFixture",
        "role": "AXRow",
        "label": smoke_item,
        "container_label": None,
        "max_depth": 7,
        "rationale": "UI_ACTIONS_SHARED pick a fixture table row",
    },
    "missing": {
        "type": "ui_click",
        "app_name": "DexterUISharedFixture",
        "role": "AXButton",
        "label": "Save",
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise missing UI failure receipt",
    },
    "disabled_click": {
        "type": "ui_click",
        "app_name": "DexterUISharedFixture",
        "role": "AXButton",
        "label": "Disabled action",
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise disabled ui_click evidence",
    },
    "missing_type": {
        "type": "ui_type",
        "app_name": "DexterUISharedFixture",
        "role": "AXTextField",
        "label": "Missing text field",
        "text": "shared smoke secret text must stay redacted in evidence",
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise missing ui_type evidence",
    },
    "disabled_type": {
        "type": "ui_type",
        "app_name": "DexterUISharedFixture",
        "role": "AXTextField",
        "label": "Disabled text field",
        "text": "shared smoke disabled secret must stay redacted in evidence",
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise disabled ui_type evidence",
    },
    "missing_select": {
        "type": "ui_select",
        "app_name": "DexterUISharedFixture",
        "role": "AXPopUpButton",
        "label": "Missing flavor selector",
        "option": smoke_option,
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise missing ui_select evidence",
    },
    "disabled_select": {
        "type": "ui_select",
        "app_name": "DexterUISharedFixture",
        "role": "AXPopUpButton",
        "label": "Disabled flavor",
        "option": smoke_option,
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise disabled ui_select evidence",
    },
    "missing_toggle": {
        "type": "ui_toggle",
        "app_name": "DexterUISharedFixture",
        "role": "AXCheckBox",
        "label": "Missing toggle",
        "state": True,
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise missing ui_toggle evidence",
    },
    "disabled_toggle": {
        "type": "ui_toggle",
        "app_name": "DexterUISharedFixture",
        "role": "AXCheckBox",
        "label": "Disabled toggle",
        "state": True,
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise disabled ui_toggle evidence",
    },
    "missing_pick": {
        "type": "ui_pick",
        "app_name": "DexterUISharedFixture",
        "role": "AXRow",
        "label": "Missing row",
        "container_label": None,
        "max_depth": 7,
        "rationale": "UI_ACTIONS_SHARED exercise missing ui_pick evidence",
    },
    "ambiguous": {
        "type": "ui_click",
        "app_name": "DexterUISharedFixture",
        "role": "AXButton",
        "label": "OK",
        "max_depth": 5,
        "rationale": "UI_ACTIONS_SHARED exercise ambiguous UI failure receipt",
    },
}
print(json.dumps(actions[action_type]))
PY
}

write_fixture_source() {
    cat >"$FIXTURE_SWIFT" <<'SWIFT'
import AppKit
import Foundation

final class PickTableController: NSObject, NSTableViewDataSource, NSTableViewDelegate {
    private let items = ["Inbox", "Invoices", "Archive"]
    private let outputPath: String

    init(outputPath: String) {
        self.outputPath = outputPath
        super.init()
    }

    func numberOfRows(in tableView: NSTableView) -> Int {
        items.count
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let identifier = NSUserInterfaceItemIdentifier("sharedPickCell")
        let textField: NSTextField
        if let reused = tableView.makeView(withIdentifier: identifier, owner: self) as? NSTextField {
            textField = reused
        } else {
            textField = NSTextField(labelWithString: "")
            textField.identifier = identifier
            textField.isBordered = false
            textField.drawsBackground = false
            textField.lineBreakMode = .byTruncatingTail
        }
        textField.stringValue = items[row]
        textField.setAccessibilityLabel(items[row])
        textField.setAccessibilityIdentifier("shared-pick-\(items[row].lowercased())")
        return textField
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        guard let tableView = notification.object as? NSTableView else { return }
        let row = tableView.selectedRow
        guard row >= 0 && row < items.count else { return }
        try? items[row].write(toFile: outputPath, atomically: true, encoding: .utf8)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var field: NSTextField?
    private var popup: NSPopUpButton?
    private var checkbox: NSButton?
    private var tableController: PickTableController?
    private let textPath: String
    private let selectPath: String
    private let togglePath: String
    private let pickPath: String
    private let clickPath: String

    init(textPath: String, selectPath: String, togglePath: String, pickPath: String, clickPath: String) {
        self.textPath = textPath
        self.selectPath = selectPath
        self.togglePath = togglePath
        self.pickPath = pickPath
        self.clickPath = clickPath
        super.init()
    }

    @objc func continuePressed(_ sender: NSButton) {
        try? "continue".write(toFile: clickPath, atomically: true, encoding: .utf8)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 130, y: 130, width: 620, height: 520),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter UI Shared Fixture"

        let title = NSTextField(labelWithString: "Dexter shared UI action fixture")
        title.frame = NSRect(x: 32, y: 468, width: 360, height: 24)

        let continueButton = NSButton(title: "Continue", target: self, action: #selector(continuePressed(_:)))
        continueButton.frame = NSRect(x: 32, y: 414, width: 130, height: 34)
        continueButton.identifier = NSUserInterfaceItemIdentifier("continueButton")
        continueButton.setAccessibilityLabel("Continue")
        continueButton.setAccessibilityIdentifier("continueButton")

        let disabledButton = NSButton(title: "Disabled action", target: nil, action: nil)
        disabledButton.frame = NSRect(x: 422, y: 414, width: 150, height: 34)
        disabledButton.isEnabled = false
        disabledButton.identifier = NSUserInterfaceItemIdentifier("disabledAction")
        disabledButton.setAccessibilityLabel("Disabled action")
        disabledButton.setAccessibilityIdentifier("disabledAction")

        let firstOK = NSButton(title: "OK", target: nil, action: nil)
        firstOK.frame = NSRect(x: 190, y: 414, width: 92, height: 34)
        firstOK.identifier = NSUserInterfaceItemIdentifier("firstOK")
        firstOK.setAccessibilityLabel("OK")
        firstOK.setAccessibilityIdentifier("firstOK")

        let secondOK = NSButton(title: "OK", target: nil, action: nil)
        secondOK.frame = NSRect(x: 304, y: 414, width: 92, height: 34)
        secondOK.identifier = NSUserInterfaceItemIdentifier("secondOK")
        secondOK.setAccessibilityLabel("OK")
        secondOK.setAccessibilityIdentifier("secondOK")

        let fieldLabel = NSTextField(labelWithString: "Dexter smoke input")
        fieldLabel.frame = NSRect(x: 32, y: 358, width: 180, height: 24)
        let field = NSTextField(frame: NSRect(x: 220, y: 354, width: 320, height: 28))
        field.placeholderString = "Dexter smoke input"
        field.identifier = NSUserInterfaceItemIdentifier("sharedSmokeInput")
        field.setAccessibilityLabel("Dexter smoke input")
        field.setAccessibilityIdentifier("sharedSmokeInput")

        let disabledField = NSTextField(frame: NSRect(x: 220, y: 322, width: 320, height: 28))
        disabledField.stringValue = ""
        disabledField.placeholderString = "Disabled text field"
        disabledField.isEnabled = false
        disabledField.identifier = NSUserInterfaceItemIdentifier("disabledTextField")
        disabledField.setAccessibilityLabel("Disabled text field")
        disabledField.setAccessibilityIdentifier("disabledTextField")

        let popupLabel = NSTextField(labelWithString: "Dessert flavor")
        popupLabel.frame = NSRect(x: 32, y: 302, width: 160, height: 24)
        let popup = NSPopUpButton(frame: NSRect(x: 220, y: 298, width: 240, height: 32), pullsDown: false)
        popup.addItems(withTitles: ["Vanilla", "Chocolate", "Strawberry"])
        popup.selectItem(withTitle: "Vanilla")
        popup.identifier = NSUserInterfaceItemIdentifier("dessertFlavor")
        popup.setAccessibilityLabel("Dessert flavor")
        popup.setAccessibilityIdentifier("dessertFlavor")

        let disabledPopup = NSPopUpButton(frame: NSRect(x: 220, y: 264, width: 240, height: 32), pullsDown: false)
        disabledPopup.addItems(withTitles: ["Vanilla", "Chocolate"])
        disabledPopup.selectItem(withTitle: "Vanilla")
        disabledPopup.isEnabled = false
        disabledPopup.identifier = NSUserInterfaceItemIdentifier("disabledFlavor")
        disabledPopup.setAccessibilityLabel("Disabled flavor")
        disabledPopup.setAccessibilityIdentifier("disabledFlavor")

        let checkbox = NSButton(checkboxWithTitle: "Enable sprinkles", target: nil, action: nil)
        checkbox.frame = NSRect(x: 32, y: 244, width: 260, height: 28)
        checkbox.state = .off
        checkbox.identifier = NSUserInterfaceItemIdentifier("enableSprinkles")
        checkbox.setAccessibilityLabel("Enable sprinkles")
        checkbox.setAccessibilityIdentifier("enableSprinkles")

        let disabledCheckbox = NSButton(checkboxWithTitle: "Disabled toggle", target: nil, action: nil)
        disabledCheckbox.frame = NSRect(x: 300, y: 244, width: 220, height: 28)
        disabledCheckbox.state = .off
        disabledCheckbox.isEnabled = false
        disabledCheckbox.identifier = NSUserInterfaceItemIdentifier("disabledToggle")
        disabledCheckbox.setAccessibilityLabel("Disabled toggle")
        disabledCheckbox.setAccessibilityIdentifier("disabledToggle")

        let tableLabel = NSTextField(labelWithString: "Smoke picks")
        tableLabel.frame = NSRect(x: 32, y: 198, width: 160, height: 24)
        let tableView = NSTableView(frame: NSRect(x: 0, y: 0, width: 360, height: 120))
        tableView.headerView = nil
        tableView.allowsMultipleSelection = false
        tableView.identifier = NSUserInterfaceItemIdentifier("smokePicksTable")
        tableView.setAccessibilityLabel("Smoke picks")
        tableView.setAccessibilityIdentifier("smokePicksTable")

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("name"))
        column.width = 340
        tableView.addTableColumn(column)
        let controller = PickTableController(outputPath: pickPath)
        tableView.dataSource = controller
        tableView.delegate = controller
        tableView.rowHeight = 30

        let scrollView = NSScrollView(frame: NSRect(x: 32, y: 48, width: 380, height: 132))
        scrollView.hasVerticalScroller = true
        scrollView.documentView = tableView
        scrollView.setAccessibilityLabel("Smoke picks")
        scrollView.setAccessibilityIdentifier("smokePicksScroll")

        for view in [title, continueButton, disabledButton, firstOK, secondOK, fieldLabel, field, disabledField, popupLabel, popup, disabledPopup, checkbox, disabledCheckbox, tableLabel, scrollView] {
            window.contentView?.addSubview(view)
        }
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        self.window = window
        self.field = field
        self.popup = popup
        self.checkbox = checkbox
        self.tableController = controller

        Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) { [weak self] _ in
            guard let self else { return }
            try? self.field?.stringValue.write(toFile: self.textPath, atomically: true, encoding: .utf8)
            try? (self.popup?.titleOfSelectedItem ?? "").write(toFile: self.selectPath, atomically: true, encoding: .utf8)
            let toggleValue = self.checkbox?.state == .on ? "on" : "off"
            try? toggleValue.write(toFile: self.togglePath, atomically: true, encoding: .utf8)
        }
    }
}

let args = Array(CommandLine.arguments.dropFirst())
let app = NSApplication.shared
let delegate = AppDelegate(
    textPath: args.indices.contains(0) ? args[0] : "/tmp/dexter-ui-shared-text.txt",
    selectPath: args.indices.contains(1) ? args[1] : "/tmp/dexter-ui-shared-select.txt",
    togglePath: args.indices.contains(2) ? args[2] : "/tmp/dexter-ui-shared-toggle.txt",
    pickPath: args.indices.contains(3) ? args[3] : "/tmp/dexter-ui-shared-pick.txt",
    clickPath: args.indices.contains(4) ? args[4] : "/tmp/dexter-ui-shared-click.txt"
)
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
SWIFT
}

stop_fixture() {
    if [[ -n "$FIXTURE_PID" ]]; then
        kill "$FIXTURE_PID" >/dev/null 2>&1 || true
        wait "$FIXTURE_PID" >/dev/null 2>&1 || true
        FIXTURE_PID=""
    fi
    pkill -f "$FIXTURE_BIN" >/dev/null 2>&1 || true
}

cleanup() {
    stop_fixture
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
    rm -f "$FIXTURE_SWIFT" "$FIXTURE_BIN"
}
trap cleanup EXIT INT TERM

assert_contains() {
    local file="$1"
    local pattern="$2"
    local label="$3"
    if ! grep -Fq "$pattern" "$file"; then
        fail "$label - missing: $pattern"
    fi
}

wait_for_file_value() {
    local file="$1"
    local expected="$2"
    local label="$3"
    for _ in {1..50}; do
        if [[ -f "$file" ]] && [[ "$(cat "$file")" == "$expected" ]]; then
            return 0
        fi
        sleep 0.2
    done
    fail "$label - expected '$expected'"
}

run_action() {
    local label="$1"
    local action_type="$2"
    shift 2
    local patterns=("$@")

    say INFO "driving $label"
    "$CLI_BIN" --idle-timeout 180 --action-json "$(json_action "$action_type")" >"$ACTION_OUT" 2>&1 \
        || fail "$label did not return cleanly to CLI"

    assert_contains "$ACTION_OUT" "[ACTION RECEIPT" "$label emitted receipt"
    for pattern in "${patterns[@]}"; do
        assert_contains "$ACTION_OUT" "$pattern" "$label output"
    done
}

run_failure_case() {
    local label="$1"
    local action_type="$2"
    local expected_kind="$3"
    local expected_directive="$4"
    local expected_next_step="$5"
    shift 5
    local extra_patterns=("$@")

    run_action "$label" "$action_type" \
        "outcome=failed" \
        "UI failure [$expected_kind]" \
        "Next [$expected_directive]" \
        "${extra_patterns[@]}"

    "$CLI_BIN" --actions last >"$LAST_OUT" 2>&1 \
        || fail "$label latest receipt inspection failed"
    assert_contains "$LAST_OUT" "UI failed ($expected_kind)" "$label latest receipt typed failure"

    "$CLI_BIN" --why >"$WHY_OUT" 2>&1 \
        || fail "$label why inspection failed"
    assert_contains "$WHY_OUT" "UI automation failed after the action was dispatched ($expected_kind)." "$label why typed cause"
    assert_contains "$WHY_OUT" "$expected_next_step" "$label why guidance"
}

require_clean_socket() {
    if socket_accepts; then
        fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
    fi
}

build_binaries() {
    say INFO "building release core and CLI"
    (
        cd "$RUST_DIR" || exit 2
        cargo build --release --bin dexter-core --bin dexter-cli >/dev/null
    )
}

start_core() {
    rm -f "$SOCKET" "$SHELL_SOCKET"
    : > "$CORE_LOG"
    say INFO "starting one shared release core; log: $CORE_LOG"
    "$CORE_BIN" >>"$CORE_LOG" 2>&1 &
    CORE_PID="$!"

    local waited=0
    while [[ "$waited" -lt 90 ]]; do
        if socket_accepts; then
            break
        fi
        if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            fail "shared core exited before opening socket"
        fi
        sleep 1
        waited=$((waited + 1))
    done
    socket_accepts || fail "shared core did not open socket within 90s"

    waited=0
    while [[ "$waited" -lt "$CORE_WARMUP_TIMEOUT_SECS" ]]; do
        "$CLI_BIN" --doctor >/tmp/dexter-ui-actions-shared-doctor.out 2>&1 || true
        if grep -Fq "OK   daemon health      status ready" /tmp/dexter-ui-actions-shared-doctor.out \
            && grep -Fq "Result: OK - no failed checks." /tmp/dexter-ui-actions-shared-doctor.out; then
            say INFO "shared core doctor-ready after ${waited}s"
            return
        fi
        if ! kill -0 "$CORE_PID" >/dev/null 2>&1; then
            fail "shared core exited during warmup"
        fi
        sleep 2
        waited=$((waited + 2))
    done

    say INFO "last doctor report:"
    cat /tmp/dexter-ui-actions-shared-doctor.out 2>/dev/null || true
    fail "shared core did not become doctor-ready within ${CORE_WARMUP_TIMEOUT_SECS}s"
}

start_fixture() {
    say INFO "building shared AppKit UI fixture"
    write_fixture_source
    swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"

    say INFO "starting shared UI fixture"
    "$FIXTURE_BIN" "$TEXT_OUT" "$SELECT_OUT" "$TOGGLE_OUT" "$PICK_OUT" "$CLICK_OUT" >/tmp/dexter-ui-shared-fixture.log 2>&1 &
    FIXTURE_PID="$!"

    say INFO "waiting for fixture Accessibility surface"
    for _ in {1..80}; do
        if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUISharedFixture"
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
    fail "fixture Accessibility surface did not become ready"
}

main() {
    rm -f "$ACTION_OUT" "$LAST_OUT" "$WHY_OUT" "$TEXT_OUT" "$SELECT_OUT" "$TOGGLE_OUT" "$PICK_OUT" "$CLICK_OUT" "$FIXTURE_SWIFT" "$FIXTURE_BIN"

    if [[ "$START_CORE" -eq 1 ]]; then
        require_clean_socket
        build_binaries
        start_core
    elif ! socket_accepts; then
        fail "no Dexter daemon accepting connections at $SOCKET"
    fi
    start_fixture

    run_action "window_focus" "window_focus" "outcome=executed" "Succeeded: focused Finder"
    run_action "window_inspect" "window_inspect" "outcome=executed" "Succeeded: inspected app: DexterUISharedFixture" "front window:"
    run_action "ui_snapshot" "ui_snapshot" "outcome=executed" "Succeeded: ui snapshot app: DexterUISharedFixture" "controls:"
    run_action "ui_click" "ui_click" "outcome=executed" "Succeeded: pressed UI control:" "app: DexterUISharedFixture"
    wait_for_file_value "$CLICK_OUT" "continue" "fixture button click"

    run_action "ui_type" "ui_type" "outcome=executed" "Succeeded: typed into UI control:" "control: AXTextField"
    wait_for_file_value "$TEXT_OUT" "$SMOKE_TEXT" "fixture text field"

    run_action "ui_select" "ui_select" "outcome=executed" "Succeeded: selected UI option:" "control: AXPopUpButton"
    wait_for_file_value "$SELECT_OUT" "$SMOKE_OPTION" "fixture popup"

    run_action "ui_toggle" "ui_toggle" "outcome=executed" "Succeeded: set UI toggle:" "state: on"
    wait_for_file_value "$TOGGLE_OUT" "on" "fixture checkbox"

    run_action "ui_pick" "ui_pick" "outcome=executed" "Succeeded: picked UI item:" "verified: true"
    wait_for_file_value "$PICK_OUT" "$SMOKE_ITEM" "fixture table"

    say INFO "restarting shared UI fixture for failure diagnostics"
    stop_fixture
    rm -f "$TEXT_OUT" "$SELECT_OUT" "$TOGGLE_OUT" "$PICK_OUT" "$CLICK_OUT"
    start_fixture

    run_failure_case \
        "missing-control ui_click" \
        "missing" \
        "control_not_found" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_click" \
        "Evidence: match_count=0 nearest_safe_candidates:"

    run_failure_case \
        "disabled-control ui_click" \
        "disabled_click" \
        "control_disabled" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_click" \
        "Evidence: matched_control:" \
        "enabled=false"

    run_failure_case \
        "missing-control ui_type" \
        "missing_type" \
        "not_typeable" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_type" \
        "text=<redacted>" \
        "Evidence: match_count=0 nearest_safe_candidates:"

    if grep -Fq "shared smoke secret text must stay redacted in evidence" "$ACTION_OUT"; then
        fail "missing-control ui_type leaked typed text into receipt evidence"
    fi

    run_failure_case \
        "disabled-control ui_type" \
        "disabled_type" \
        "control_disabled" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_type" \
        "text=<redacted>" \
        "Evidence: matched_control:" \
        "enabled=false"

    if grep -Fq "shared smoke disabled secret must stay redacted in evidence" "$ACTION_OUT"; then
        fail "disabled-control ui_type leaked typed text into receipt evidence"
    fi

    run_failure_case \
        "missing-control ui_select" \
        "missing_select" \
        "not_selectable" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_select" \
        "option='Chocolate'" \
        "Evidence: match_count=0 nearest_safe_candidates:"

    run_failure_case \
        "disabled-control ui_select" \
        "disabled_select" \
        "control_disabled" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_select" \
        "option='Chocolate'" \
        "Evidence: matched_control:" \
        "enabled=false"

    run_failure_case \
        "missing-control ui_toggle" \
        "missing_toggle" \
        "not_selectable" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_toggle" \
        "state=on" \
        "Evidence: match_count=0 nearest_safe_candidates:"

    run_failure_case \
        "disabled-control ui_toggle" \
        "disabled_toggle" \
        "control_disabled" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_toggle" \
        "state=on" \
        "Evidence: matched_control:" \
        "enabled=false"

    run_failure_case \
        "missing-control ui_pick" \
        "missing_pick" \
        "not_selectable" \
        "snapshot_then_replan" \
        "Capture a UI snapshot before choosing another control. Do not repeat the same label blindly." \
        "Target: action=ui_pick" \
        "container='<none>'" \
        "Evidence: match_count=0 nearest_safe_candidates:"

    run_failure_case \
        "ambiguous-control ui_click" \
        "ambiguous" \
        "ambiguous_control" \
        "ask_for_clarification" \
        "Ask which matching control to use, or collect a UI snapshot and choose a more specific target." \
        "Target: action=ui_click" \
        "Evidence: match_count=" \
        "candidates:"

    say PASS "shared-core UI actions smoke passed"
}

main "$@"
