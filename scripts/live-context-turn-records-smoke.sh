#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CORE_BIN="$RUST_DIR/target/release/dexter-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
CORE_LOG="/tmp/dexter-context-turn-records-core.log"
UI_OUT="/tmp/dexter-context-turn-records-ui.out"
BROWSER_OUT="/tmp/dexter-context-turn-records-browser.out"
INSPECT_OUT="/tmp/dexter-context-turn-records-inspect.out"
FIXTURE_SWIFT="/tmp/DexterContextTurnRecordFixture.swift"
FIXTURE_BIN="/tmp/DexterContextTurnRecordFixture"
BROWSER_FIXTURE="/tmp/dexter-context-turn-records-browser.html"
MARKER_FILE="/tmp/dexter-context-turn-records.marker"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
FIXTURE_PID=""
CORE_PID=""
export OLLAMA_MODELS="${OLLAMA_MODELS:-/Users/jason/ollama-models}"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    for file in "$UI_OUT" "$BROWSER_OUT" "$INSPECT_OUT"; do
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

json_string() {
    python3 - "$1" <<'PY'
import json
import sys

print(json.dumps(sys.argv[1]))
PY
}

state_dir() {
    python3 <<'PY'
import pathlib
import sys

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None

default = pathlib.Path.home() / ".dexter" / "state"
config_path = pathlib.Path.home() / ".dexter" / "config.toml"
if not config_path.exists() or tomllib is None:
    print(default)
    sys.exit(0)

try:
    config = tomllib.loads(config_path.read_text(encoding="utf-8"))
except Exception:
    print(default)
    sys.exit(0)

print(config.get("core", {}).get("state_dir", str(default)))
PY
}

ui_action_json() {
    python3 <<'PY'
import json

print(json.dumps({
    "type": "ui_click",
    "app_name": "DexterContextTurnRecordFixture",
    "role": "AXButton",
    "label": "Save",
    "max_depth": 5,
    "rationale": "CONTEXT_TURN_RECORD_SMOKE exercise C3 UI action evidence"
}))
PY
}

browser_action_json() {
    local action="$1"
    local value="$2"
    if [[ "$action" == "navigate" ]]; then
        printf '{"type":"browser","action":"navigate","url":%s,"rationale":"CONTEXT_TURN_RECORD_SMOKE browser setup"}\n' "$(json_string "$value")"
    else
        printf '{"type":"browser","action":"click","selector":%s,"rationale":"CONTEXT_TURN_RECORD_SMOKE browser failure"}\n' "$(json_string "$value")"
    fi
}

write_ui_fixture_source() {
    cat >"$FIXTURE_SWIFT" <<'SWIFT'
import AppKit
import Foundation

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 160, y: 160, width: 440, height: 210),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter Context Turn Record Fixture"

        let label = NSTextField(labelWithString: "No Save button; two OK buttons for nearby evidence")
        label.frame = NSRect(x: 36, y: 144, width: 370, height: 24)

        let firstButton = NSButton(title: "OK", target: nil, action: nil)
        firstButton.frame = NSRect(x: 78, y: 68, width: 112, height: 34)
        firstButton.identifier = NSUserInterfaceItemIdentifier("firstOK")
        firstButton.setAccessibilityLabel("OK")
        firstButton.setAccessibilityIdentifier("firstOK")

        let secondButton = NSButton(title: "OK", target: nil, action: nil)
        secondButton.frame = NSRect(x: 250, y: 68, width: 112, height: 34)
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

write_browser_fixture() {
    cat >"$BROWSER_FIXTURE" <<'HTML'
<!doctype html>
<html>
  <head><meta charset="utf-8"><title>Dexter Context Turn Record Browser Fixture</title></head>
  <body>
    <button id="real-turn-record-button">Real turn record button</button>
    <div id="status">ready</div>
  </body>
</html>
HTML
}

cleanup() {
    if [[ -n "$FIXTURE_PID" ]]; then
        kill "$FIXTURE_PID" >/dev/null 2>&1 || true
        wait "$FIXTURE_PID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
    pkill -f "$FIXTURE_BIN" >/dev/null 2>&1 || true
    make -C "$ROOT_DIR" stop >/dev/null 2>&1 || true
    rm -f "$FIXTURE_SWIFT" "$FIXTURE_BIN" "$BROWSER_FIXTURE" "$MARKER_FILE"
}
trap cleanup EXIT INT TERM

wait_for_fixture() {
    say INFO "waiting for UI fixture Accessibility surface"
    for _ in {1..60}; do
        if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterContextTurnRecordFixture"
    if (count of matchingProcesses) is 0 then error "fixture not running" number 1728
    set targetProcess to item 1 of matchingProcesses
    set targetWindow to front window of targetProcess
    set targetButtons to buttons of targetWindow whose name is "OK"
    if (count of targetButtons) is less than 2 then error "duplicate buttons not exposed" number 1728
end tell
APPLESCRIPT
        then
            return 0
        fi
        sleep 0.25
    done
    fail "UI fixture did not expose expected Accessibility controls"
}

inspect_records() {
    local state="$1"
    python3 - "$state" "$MARKER_FILE" >"$INSPECT_OUT" <<'PY'
import json
import sys
from pathlib import Path

state_dir = Path(sys.argv[1]).expanduser()
marker = Path(sys.argv[2])
marker_mtime = marker.stat().st_mtime
records_dir = state_dir / "context_turns"
records = []

if records_dir.exists():
    for path in records_dir.rglob("*.json"):
        try:
            if path.stat().st_mtime < marker_mtime:
                continue
            record = json.loads(path.read_text(encoding="utf-8"))
        except Exception:
            continue
        records.append((path, record))

def diagnostic(record):
    action = record.get("action") or {}
    return action, action.get("diagnostic") or {}

def synthetic(record):
    return (
        record.get("route_category") == "SyntheticAction"
        and record.get("model") == "synthetic_action"
        and (record.get("context_diagnostics") or {}).get("scope") == "ambient_only"
        and record.get("privacy_mode") == "redacted_preview_v1"
    )

def has_ui(record):
    action, diag = diagnostic(record)
    haystack = json.dumps(record, sort_keys=True)
    return (
        synthetic(record)
        and action.get("action_kind") == "ui_click"
        and diag.get("source") == "ui_window"
        and diag.get("failure_kind") == "control_not_found"
        and diag.get("recovery_directive") == "snapshot_then_replan"
        and "DexterContextTurnRecordFixture" in (diag.get("target_preview") or "")
        and "label='Save'" in (diag.get("target_preview") or "")
        and "text=" not in haystack
    )

def has_browser(record):
    action, diag = diagnostic(record)
    return (
        synthetic(record)
        and action.get("action_kind") == "browser"
        and diag.get("source") == "browser"
        and diag.get("failure_kind") == "selector_not_found"
        and diag.get("recovery_directive") == "extract_page_then_replan"
        and "#missing-turn-record-button" in (diag.get("target_preview") or "")
        and "Dexter Context Turn Record Browser Fixture" in (diag.get("target_preview") or "")
        and "#real-turn-record-button" in (diag.get("evidence_preview") or "")
    )

ui_matches = [(str(path), record.get("trace_id")) for path, record in records if has_ui(record)]
browser_matches = [(str(path), record.get("trace_id")) for path, record in records if has_browser(record)]

print(f"records_checked={len(records)}")
print(f"ui_matches={len(ui_matches)}")
for path, trace_id in ui_matches[:3]:
    print(f"ui_record={path} trace_id={trace_id}")
print(f"browser_matches={len(browser_matches)}")
for path, trace_id in browser_matches[:3]:
    print(f"browser_record={path} trace_id={trace_id}")

if not ui_matches:
    print("missing=ui_context_turn_record")
if not browser_matches:
    print("missing=browser_context_turn_record")
sys.exit(0 if ui_matches and browser_matches else 1)
PY
}

if socket_accepts; then
    fail "a Dexter daemon is already accepting connections; stop it before running this smoke"
fi

rm -f "$CORE_LOG" "$UI_OUT" "$BROWSER_OUT" "$INSPECT_OUT" "$MARKER_FILE" "$BROWSER_FIXTURE"
rm -f "$FIXTURE_SWIFT" "$FIXTURE_BIN"

say INFO "building release core and CLI"
(
    cd "$RUST_DIR" || exit 2
    cargo build --release --bin dexter-core --bin dexter-cli >/dev/null
)

say INFO "building temporary AppKit UI fixture"
write_ui_fixture_source
swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"
write_browser_fixture

say INFO "starting release core; log: $CORE_LOG"
rm -f "$SOCKET" "$SHELL_SOCKET"
"$CORE_BIN" >"$CORE_LOG" 2>&1 &
CORE_PID="$!"

say INFO "waiting for daemon readiness"
make -C "$ROOT_DIR" wait-for-ready >/dev/null

touch "$MARKER_FILE"

say INFO "starting temporary UI fixture"
"$FIXTURE_BIN" >/tmp/dexter-context-turn-records-fixture.log 2>&1 &
FIXTURE_PID="$!"
wait_for_fixture

say INFO "driving failed UI action for C3 record"
"$CLI_BIN" --quiet --idle-timeout 180 --action-json "$(ui_action_json)" >"$UI_OUT" 2>&1 \
    || fail "failed UI action did not return cleanly to CLI"
grep -Fq "UI failure [control_not_found]" "$UI_OUT" \
    || fail "failed UI action did not surface typed UI diagnostic"

say INFO "driving failed browser action for C3 record"
file_url="file://$BROWSER_FIXTURE"
"$CLI_BIN" --quiet --idle-timeout 180 --action-json "$(browser_action_json navigate "$file_url")" >"$BROWSER_OUT" 2>&1 \
    || fail "browser navigate did not return cleanly to CLI"
"$CLI_BIN" --quiet --idle-timeout 180 --action-json "$(browser_action_json click "#missing-turn-record-button")" >>"$BROWSER_OUT" 2>&1 \
    || fail "failed browser click did not return cleanly to CLI"
grep -Fq "Browser failure [selector_not_found]" "$BROWSER_OUT" \
    || fail "failed browser action did not surface typed browser diagnostic"

say INFO "inspecting context turn records"
inspect_records "$(state_dir)" || fail "context turn records did not contain expected action diagnostics"

say PASS "context turn records preserve typed UI/browser action outcome evidence"
