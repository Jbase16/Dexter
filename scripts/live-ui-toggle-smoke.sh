#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ui-toggle-core.log"
ACTION_OUT="/tmp/dexter-ui-toggle.out"
RECENT_OUT="/tmp/dexter-ui-toggle-recent.out"
FIXTURE_SWIFT="/tmp/DexterUIToggleFixture.swift"
FIXTURE_BIN="/tmp/DexterUIToggleFixture"
FIXTURE_OUT="/tmp/dexter-ui-toggle-fixture-value.txt"
FIXTURE_PID=""
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
    python3 - <<'PY'
import json

print(json.dumps({
    "type": "ui_toggle",
    "app_name": "DexterUIToggleFixture",
    "role": "AXCheckBox",
    "label": "Enable sprinkles",
    "state": True,
    "max_depth": 4,
    "rationale": "UI_TOGGLE_SMOKE turn on a temporary AppKit checkbox"
}))
PY
}

write_fixture_source() {
    cat >"$FIXTURE_SWIFT" <<'SWIFT'
import AppKit
import Foundation

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var checkbox: NSButton?
    private let outputPath: String

    init(outputPath: String) {
        self.outputPath = outputPath
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 160, y: 160, width: 500, height: 190),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter UI Toggle Fixture"

        let checkbox = NSButton(checkboxWithTitle: "Enable sprinkles", target: nil, action: nil)
        checkbox.frame = NSRect(x: 40, y: 84, width: 260, height: 28)
        checkbox.state = .off
        checkbox.identifier = NSUserInterfaceItemIdentifier("enableSprinkles")
        checkbox.setAccessibilityLabel("Enable sprinkles")
        checkbox.setAccessibilityIdentifier("enableSprinkles")

        window.contentView?.addSubview(checkbox)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        self.window = window
        self.checkbox = checkbox

        Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) { [weak self] _ in
            guard let self, let checkbox = self.checkbox else { return }
            let value = checkbox.state == .on ? "on" : "off"
            try? value.write(toFile: self.outputPath, atomically: true, encoding: .utf8)
        }
    }
}

let outputPath = CommandLine.arguments.dropFirst().first ?? "/tmp/dexter-ui-toggle-fixture-value.txt"
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

say INFO "building temporary AppKit checkbox fixture"
write_fixture_source
swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

say INFO "starting temporary UI toggle fixture"
"$FIXTURE_BIN" "$FIXTURE_OUT" >/tmp/dexter-ui-toggle-fixture.log 2>&1 &
FIXTURE_PID="$!"

say INFO "waiting for fixture Accessibility surface"
for _ in {1..40}; do
    if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUIToggleFixture"
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

say INFO "driving ui_toggle action against temporary checkbox"
"$CLI_BIN" --idle-timeout 180 --action-json "$(json_action)" >"$ACTION_OUT" 2>&1 \
    || fail "ui_toggle action did not return cleanly to CLI"

grep -Fq "[ACTION RECEIPT" "$ACTION_OUT" \
    || fail "ui_toggle action did not emit a receipt"
grep -Fq "ui_toggle" "$ACTION_OUT" \
    || fail "ui_toggle action type was not surfaced"
grep -Fq "outcome=executed" "$ACTION_OUT" \
    || fail "ui_toggle action did not execute"
grep -Fq "Succeeded: set UI toggle:" "$ACTION_OUT" \
    || fail "ui_toggle action did not report toggle result"
grep -Fq "state: on" "$ACTION_OUT" \
    || fail "ui_toggle action did not report requested state"
grep -Fq "changed: true" "$ACTION_OUT" \
    || fail "ui_toggle action did not report a state change"
grep -Fq "approval required" "$ACTION_OUT" \
    && fail "ordinary ui_toggle unexpectedly required approval"

say INFO "waiting for fixture to observe checked value"
for _ in {1..40}; do
    if [[ -f "$FIXTURE_OUT" ]] && [[ "$(cat "$FIXTURE_OUT")" == "on" ]]; then
        break
    fi
    sleep 0.25
done
[[ -f "$FIXTURE_OUT" ]] \
    || fail "fixture did not write observed checkbox value"
[[ "$(cat "$FIXTURE_OUT")" == "on" ]] \
    || fail "fixture checkbox did not contain requested checked state"

"$CLI_BIN" --actions recent --limit 20 >"$RECENT_OUT"
grep -Fq "ui_toggle" "$RECENT_OUT" \
    || fail "recent action receipts did not include ui_toggle action type"
grep -Fq "UI toggle: DexterUIToggleFixture AXCheckBox \"Enable sprinkles\" -> on" "$RECENT_OUT" \
    || fail "recent action receipts did not show readable UI toggle target"
grep -Fq "EXECUTED" "$RECENT_OUT" \
    || fail "recent action receipts did not record execution"
grep -Fq "Succeeded: set UI toggle:" "$RECENT_OUT" \
    || fail "recent action receipts did not record toggle result"

say PASS "ui_toggle action smoke passed"
