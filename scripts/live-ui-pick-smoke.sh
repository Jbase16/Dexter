#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ui-pick-core.log"
ACTION_OUT="/tmp/dexter-ui-pick.out"
RECENT_OUT="/tmp/dexter-ui-pick-recent.out"
FIXTURE_SWIFT="/tmp/DexterUIPickFixture.swift"
FIXTURE_BIN="/tmp/DexterUIPickFixture"
FIXTURE_OUT="/tmp/dexter-ui-pick-fixture-value.txt"
FIXTURE_PID=""
SMOKE_ITEM="Invoices"
SOCKET="/tmp/dexter.sock"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    for file in "$ACTION_OUT" "$RECENT_OUT"; do
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
    python3 - "$SMOKE_ITEM" <<'PY'
import json
import sys

print(json.dumps({
    "type": "ui_pick",
    "app_name": "DexterUIPickFixture",
    "role": "AXRow",
    "label": sys.argv[1],
    "container_label": None,
    "max_depth": 6,
    "rationale": "UI_PICK_SMOKE select a visible temporary AppKit table row"
}))
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
        let identifier = NSUserInterfaceItemIdentifier("pickCell")
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
        textField.setAccessibilityIdentifier("pick-\(items[row].lowercased())")
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
    private var tableView: NSTableView?
    private var tableController: PickTableController?
    private let outputPath: String

    init(outputPath: String) {
        self.outputPath = outputPath
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 180, y: 180, width: 480, height: 260),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter UI Pick Fixture"

        let label = NSTextField(labelWithString: "Smoke picks")
        label.frame = NSRect(x: 28, y: 206, width: 160, height: 24)

        let tableView = NSTableView(frame: NSRect(x: 0, y: 0, width: 400, height: 150))
        tableView.headerView = nil
        tableView.allowsMultipleSelection = false
        tableView.identifier = NSUserInterfaceItemIdentifier("smokePicksTable")
        tableView.setAccessibilityLabel("Smoke picks")
        tableView.setAccessibilityIdentifier("smokePicksTable")

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("name"))
        column.width = 380
        tableView.addTableColumn(column)

        let controller = PickTableController(outputPath: outputPath)
        tableView.dataSource = controller
        tableView.delegate = controller
        tableView.rowHeight = 34

        let scrollView = NSScrollView(frame: NSRect(x: 28, y: 36, width: 400, height: 150))
        scrollView.hasVerticalScroller = true
        scrollView.documentView = tableView
        scrollView.setAccessibilityLabel("Smoke picks")
        scrollView.setAccessibilityIdentifier("smokePicksScroll")

        window.contentView?.addSubview(label)
        window.contentView?.addSubview(scrollView)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        self.window = window
        self.tableView = tableView
        self.tableController = controller
    }
}

let outputPath = CommandLine.arguments.dropFirst().first ?? "/tmp/dexter-ui-pick-fixture-value.txt"
let app = NSApplication.shared
let delegate = AppDelegate(outputPath: outputPath)
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
    rm -rf "$FIXTURE_SWIFT" "$FIXTURE_BIN" "$FIXTURE_OUT"
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$ACTION_OUT" "$RECENT_OUT" "$FIXTURE_OUT" "$FIXTURE_SWIFT" "$FIXTURE_BIN"

say INFO "building temporary AppKit table fixture"
write_fixture_source
swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

say INFO "starting temporary UI pick fixture"
"$FIXTURE_BIN" "$FIXTURE_OUT" >/tmp/dexter-ui-pick-fixture.log 2>&1 &
FIXTURE_PID="$!"

say INFO "waiting for fixture Accessibility surface"
for _ in {1..60}; do
    if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUIPickFixture"
    if (count of matchingProcesses) is 0 then error "fixture not running" number 1728
    set targetProcess to item 1 of matchingProcesses
    set targetWindow to front window of targetProcess
    set targetRows to rows of table 1 of scroll area 1 of targetWindow
    if (count of targetRows) is 0 then error "fixture rows not exposed" number 1728
end tell
APPLESCRIPT
    then
        break
    fi
    sleep 0.25
done

if ! osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUIPickFixture"
    if (count of matchingProcesses) is 0 then error "not running" number 1728
end tell
APPLESCRIPT
then
    fail "temporary ui_pick fixture app exited before Dexter could act"
fi

say INFO "driving ui_pick action against temporary table row"
"$CLI_BIN" --idle-timeout 180 --action-json "$(json_action)" >"$ACTION_OUT" 2>&1 \
    || fail "ui_pick action did not return cleanly to CLI"

grep -Fq "[ACTION RECEIPT" "$ACTION_OUT" \
    || fail "ui_pick action did not emit a receipt"
grep -Fq "ui_pick" "$ACTION_OUT" \
    || fail "ui_pick action type was not surfaced"
grep -Fq "outcome=executed" "$ACTION_OUT" \
    || fail "ui_pick action did not execute"
grep -Fq "Succeeded: picked UI item:" "$ACTION_OUT" \
    || fail "ui_pick action did not report picked item"
grep -Fq "approval required" "$ACTION_OUT" \
    && fail "ordinary ui_pick unexpectedly required approval"

say INFO "waiting for fixture to observe picked value"
for _ in {1..40}; do
    if [[ -f "$FIXTURE_OUT" ]] && [[ "$(cat "$FIXTURE_OUT")" == "$SMOKE_ITEM" ]]; then
        break
    fi
    sleep 0.25
done
[[ -f "$FIXTURE_OUT" ]] \
    || fail "fixture did not write observed picked value"
[[ "$(cat "$FIXTURE_OUT")" == "$SMOKE_ITEM" ]] \
    || fail "fixture table did not contain picked smoke item"

"$CLI_BIN" --actions recent --limit 20 >"$RECENT_OUT"
grep -Fq "ui_pick" "$RECENT_OUT" \
    || fail "recent action receipts did not include ui_pick action type"
grep -Fq "UI pick: DexterUIPickFixture AXRow \"$SMOKE_ITEM\"" "$RECENT_OUT" \
    || fail "recent action receipts did not show readable UI pick target"
grep -Fq "EXECUTED" "$RECENT_OUT" \
    || fail "recent action receipts did not record execution"
grep -Fq "Succeeded: picked UI item:" "$RECENT_OUT" \
    || fail "recent action receipts did not record pick result"

say PASS "ui_pick action smoke passed"
