#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ui-select-core.log"
ACTION_OUT="/tmp/dexter-ui-select.out"
RECENT_OUT="/tmp/dexter-ui-select-recent.out"
FIXTURE_SWIFT="/tmp/DexterUISelectFixture.swift"
FIXTURE_BIN="/tmp/DexterUISelectFixture"
FIXTURE_OUT="/tmp/dexter-ui-select-fixture-value.txt"
FIXTURE_PID=""
SMOKE_OPTION="Chocolate"
SOCKET="/tmp/dexter.sock"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    for file in "$ACTION_OUT" "$RECENT_OUT" "$FIXTURE_OUT"; do
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
    python3 - "$SMOKE_OPTION" <<'PY'
import json
import sys

print(json.dumps({
    "type": "ui_select",
    "app_name": "DexterUISelectFixture",
    "role": "AXPopUpButton",
    "label": "Dessert flavor",
    "option": sys.argv[1],
    "max_depth": 4,
    "rationale": "UI_SELECT_SMOKE select an option in a temporary AppKit popup"
}))
PY
}

write_fixture_source() {
    cat >"$FIXTURE_SWIFT" <<'SWIFT'
import AppKit
import Foundation

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var popup: NSPopUpButton?
    private let outputPath: String

    init(outputPath: String) {
        self.outputPath = outputPath
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 140, y: 140, width: 500, height: 190),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter UI Select Fixture"

        let label = NSTextField(labelWithString: "Dessert flavor")
        label.frame = NSRect(x: 40, y: 112, width: 140, height: 24)

        let popup = NSPopUpButton(frame: NSRect(x: 180, y: 108, width: 240, height: 32), pullsDown: false)
        popup.addItems(withTitles: ["Vanilla", "Chocolate", "Strawberry"])
        popup.selectItem(withTitle: "Vanilla")
        popup.identifier = NSUserInterfaceItemIdentifier("dessertFlavor")
        popup.setAccessibilityLabel("Dessert flavor")
        popup.setAccessibilityIdentifier("dessertFlavor")

        window.contentView?.addSubview(label)
        window.contentView?.addSubview(popup)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        self.window = window
        self.popup = popup

        Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) { [weak self] _ in
            guard let self, let popup = self.popup else { return }
            let value = popup.titleOfSelectedItem ?? ""
            try? value.write(toFile: self.outputPath, atomically: true, encoding: .utf8)
        }
    }
}

let outputPath = CommandLine.arguments.dropFirst().first ?? "/tmp/dexter-ui-select-fixture-value.txt"
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
    rm -f "$FIXTURE_SWIFT" "$FIXTURE_BIN" "$FIXTURE_OUT"
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$ACTION_OUT" "$RECENT_OUT" "$FIXTURE_OUT" "$FIXTURE_SWIFT" "$FIXTURE_BIN"

say INFO "building temporary AppKit popup fixture"
write_fixture_source
swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

say INFO "starting temporary UI select fixture"
"$FIXTURE_BIN" "$FIXTURE_OUT" >/tmp/dexter-ui-select-fixture.log 2>&1 &
FIXTURE_PID="$!"

say INFO "waiting for fixture Accessibility surface"
for _ in {1..40}; do
    if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUISelectFixture"
    if (count of matchingProcesses) is 0 then error "fixture not running" number 1728
    set targetProcess to item 1 of matchingProcesses
    if (count of UI elements of targetProcess) is 0 then error "fixture not exposed" number 1728
end tell
APPLESCRIPT
    then
        break
    fi
    sleep 0.25
done

say INFO "driving ui_select action against temporary popup"
"$CLI_BIN" --idle-timeout 180 --action-json "$(json_action)" >"$ACTION_OUT" 2>&1 \
    || fail "ui_select action did not return cleanly to CLI"

grep -Fq "[ACTION RECEIPT" "$ACTION_OUT" \
    || fail "ui_select action did not emit a receipt"
grep -Fq "ui_select" "$ACTION_OUT" \
    || fail "ui_select action type was not surfaced"
grep -Fq "outcome=executed" "$ACTION_OUT" \
    || fail "ui_select action did not execute"
grep -Fq "Succeeded: selected UI option:" "$ACTION_OUT" \
    || fail "ui_select action did not report selected option"
grep -Fq "control: AXPopUpButton" "$ACTION_OUT" \
    || fail "ui_select action did not report popup control"
grep -Fq "approval required" "$ACTION_OUT" \
    && fail "ordinary ui_select unexpectedly required approval"

say INFO "waiting for fixture to observe selected value"
for _ in {1..40}; do
    if [[ -f "$FIXTURE_OUT" ]] && [[ "$(cat "$FIXTURE_OUT")" == "$SMOKE_OPTION" ]]; then
        break
    fi
    sleep 0.25
done
[[ -f "$FIXTURE_OUT" ]] \
    || fail "fixture did not write observed popup value"
[[ "$(cat "$FIXTURE_OUT")" == "$SMOKE_OPTION" ]] \
    || fail "fixture popup did not contain selected smoke option"

"$CLI_BIN" --actions recent --limit 20 >"$RECENT_OUT"
grep -Fq "ui_select" "$RECENT_OUT" \
    || fail "recent action receipts did not include ui_select action type"
grep -Fq "UI select: DexterUISelectFixture AXPopUpButton \"Dessert flavor\" -> \"Chocolate\"" "$RECENT_OUT" \
    || fail "recent action receipts did not show readable UI select target"
grep -Fq "EXECUTED" "$RECENT_OUT" \
    || fail "recent action receipts did not record execution"
grep -Fq "Succeeded: selected UI option:" "$RECENT_OUT" \
    || fail "recent action receipts did not record selected option"

say PASS "ui_select action smoke passed"
