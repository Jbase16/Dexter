#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-ui-type-core.log"
ACTION_OUT="/tmp/dexter-ui-type.out"
RECENT_OUT="/tmp/dexter-ui-type-recent.out"
FIXTURE_SWIFT="/tmp/DexterUITypeFixture.swift"
FIXTURE_BIN="/tmp/DexterUITypeFixture"
FIXTURE_OUT="/tmp/dexter-ui-type-fixture-value.txt"
FIXTURE_PID=""
SMOKE_TEXT="Dexter typed this through structured ui_type."
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
    python3 - "$SMOKE_TEXT" <<'PY'
import json
import sys

print(json.dumps({
    "type": "ui_type",
    "app_name": "DexterUITypeFixture",
    "role": "AXTextField",
    "label": "Dexter smoke input",
    "text": sys.argv[1],
    "max_depth": 4,
    "rationale": "UI_TYPE_SMOKE type into a temporary AppKit text field"
}))
PY
}

write_fixture_source() {
    cat >"$FIXTURE_SWIFT" <<'SWIFT'
import AppKit
import Foundation

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var field: NSTextField?
    private let outputPath: String

    init(outputPath: String) {
        self.outputPath = outputPath
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 120, y: 120, width: 480, height: 180),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter UI Type Fixture"

        let field = NSTextField(frame: NSRect(x: 40, y: 78, width: 400, height: 28))
        field.stringValue = ""
        field.placeholderString = "Dexter smoke input"
        field.identifier = NSUserInterfaceItemIdentifier("dexterSmokeInput")
        field.setAccessibilityLabel("Dexter smoke input")
        field.setAccessibilityIdentifier("dexterSmokeInput")

        window.contentView?.addSubview(field)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        self.window = window
        self.field = field

        Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) { [weak self] _ in
            guard let self, let field = self.field else { return }
            try? field.stringValue.write(toFile: self.outputPath, atomically: true, encoding: .utf8)
        }
    }
}

let outputPath = CommandLine.arguments.dropFirst().first ?? "/tmp/dexter-ui-type-fixture-value.txt"
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

say INFO "building temporary AppKit text-field fixture"
write_fixture_source
swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"

say INFO "building release core and CLI"
cd "$RUST_DIR"
cargo build --release --bin dexter-core --bin dexter-cli >/dev/null

say INFO "starting release core; log: $CORE_LOG"
make -C "$ROOT_DIR" run-core >"$CORE_LOG" 2>&1 &

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

say INFO "starting temporary UI type fixture"
"$FIXTURE_BIN" "$FIXTURE_OUT" >/tmp/dexter-ui-type-fixture.log 2>&1 &
FIXTURE_PID="$!"

say INFO "waiting for fixture Accessibility surface"
for _ in {1..40}; do
    if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUITypeFixture"
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

say INFO "driving ui_type action against temporary text field"
"$CLI_BIN" --idle-timeout 180 --action-json "$(json_action)" >"$ACTION_OUT" 2>&1 \
    || fail "ui_type action did not return cleanly to CLI"

grep -Fq "[ACTION RECEIPT" "$ACTION_OUT" \
    || fail "ui_type action did not emit a receipt"
grep -Fq "ui_type" "$ACTION_OUT" \
    || fail "ui_type action type was not surfaced"
grep -Fq "outcome=executed" "$ACTION_OUT" \
    || fail "ui_type action did not execute"
grep -Fq "Succeeded: typed into UI control:" "$ACTION_OUT" \
    || fail "ui_type action did not report the target control"
grep -Fq "text: <" "$ACTION_OUT" \
    || fail "ui_type action did not report redacted text length"
if grep -Fq "$SMOKE_TEXT" "$ACTION_OUT"; then
    fail "ui_type leaked typed text into CLI action receipt"
fi
if grep -Fq "approval required" "$ACTION_OUT"; then
    fail "ordinary ui_type unexpectedly required approval"
fi

say INFO "waiting for fixture to observe typed value"
for _ in {1..40}; do
    if [[ -f "$FIXTURE_OUT" ]] && [[ "$(cat "$FIXTURE_OUT")" == "$SMOKE_TEXT" ]]; then
        break
    fi
    sleep 0.25
done
[[ -f "$FIXTURE_OUT" ]] \
    || fail "fixture did not write observed text value"
[[ "$(cat "$FIXTURE_OUT")" == "$SMOKE_TEXT" ]] \
    || fail "fixture text field did not contain typed smoke text"

"$CLI_BIN" --actions recent --limit 20 >"$RECENT_OUT"
grep -Fq "ui_type" "$RECENT_OUT" \
    || fail "recent action receipts did not include ui_type action type"
grep -Fq "UI type: DexterUITypeFixture AXTextField \"Dexter smoke input\"" "$RECENT_OUT" \
    || fail "recent action receipts did not show readable UI type target"
grep -Fq "EXECUTED" "$RECENT_OUT" \
    || fail "recent action receipts did not record execution"
grep -Fq "Succeeded: typed into UI control:" "$RECENT_OUT" \
    || fail "recent action receipts did not record typed control"
if grep -Fq "$SMOKE_TEXT" "$RECENT_OUT"; then
    fail "ui_type leaked typed text into recent action receipts"
fi

say PASS "ui_type action smoke passed"
