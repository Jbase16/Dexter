#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/src/rust-core"
CORE_BIN="$RUST_DIR/target/release/dexter-core"
CLI_BIN="$RUST_DIR/target/release/dexter-cli"
SOCKET="/tmp/dexter.sock"
SHELL_SOCKET="/tmp/dexter-shell.sock"
CORE_LOG="/tmp/dexter-ui-recovery-model-core.log"
OUT="/tmp/dexter-ui-recovery-model.out"
DOCTOR_OUT="/tmp/dexter-ui-recovery-model-doctor.out"
FIXTURE_SWIFT="/tmp/DexterUIModelRecoveryFixture.swift"
FIXTURE_BIN="/tmp/DexterUIModelRecoveryFixture"
FIXTURE_LOG="/tmp/dexter-ui-recovery-model-fixture.log"
CORE_PID=""
FIXTURE_PID=""
CORE_WARMUP_TIMEOUT_SECS="${DEXTER_SMOKE_CORE_WARMUP_TIMEOUT_SECS:-300}"

say() {
    local level="$1"
    shift
    printf '[%s] %s\n' "$level" "$*"
}

fail() {
    say FAIL "$*"
    for file in "$OUT" "$DOCTOR_OUT" "$FIXTURE_LOG"; do
        if [[ -f "$file" ]]; then
            say INFO "$file:"
            cat "$file" || true
        fi
    done
    if [[ -f "$CORE_LOG" ]]; then
        say INFO "core log tail:"
        tail -n 160 "$CORE_LOG" || true
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
    if [[ -n "$FIXTURE_PID" ]]; then
        kill "$FIXTURE_PID" >/dev/null 2>&1 || true
        wait "$FIXTURE_PID" >/dev/null 2>&1 || true
        FIXTURE_PID=""
    fi
    pkill -f "$FIXTURE_BIN" >/dev/null 2>&1 || true
    if [[ -n "$CORE_PID" ]]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
        CORE_PID=""
    fi
    rm -f "$FIXTURE_SWIFT" "$FIXTURE_BIN"
}
trap cleanup EXIT INT TERM

write_fixture_source() {
    cat >"$FIXTURE_SWIFT" <<'SWIFT'
import AppKit
import Foundation

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 180, y: 180, width: 460, height: 240),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dexter UI Model Recovery Fixture"

        let title = NSTextField(labelWithString: "Dexter UI recovery model fixture")
        title.frame = NSRect(x: 28, y: 184, width: 360, height: 24)

        let realButton = NSButton(title: "Real Save", target: nil, action: nil)
        realButton.frame = NSRect(x: 28, y: 126, width: 120, height: 34)
        realButton.identifier = NSUserInterfaceItemIdentifier("realSaveButton")
        realButton.setAccessibilityLabel("Real Save")
        realButton.setAccessibilityIdentifier("realSaveButton")

        let cancelButton = NSButton(title: "Cancel", target: nil, action: nil)
        cancelButton.frame = NSRect(x: 168, y: 126, width: 120, height: 34)
        cancelButton.identifier = NSUserInterfaceItemIdentifier("cancelButton")
        cancelButton.setAccessibilityLabel("Cancel")
        cancelButton.setAccessibilityIdentifier("cancelButton")

        let note = NSTextField(labelWithString: "There is no button named Missing Save.")
        note.frame = NSRect(x: 28, y: 74, width: 360, height: 24)

        for view in [title, realButton, cancelButton, note] {
            window.contentView?.addSubview(view)
        }
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

start_core() {
    if [[ ! -x "$CORE_BIN" || ! -x "$CLI_BIN" ]]; then
        fail "missing release binaries; run: cd src/rust-core && cargo build --release --bin dexter-core --bin dexter-cli"
    fi
    if socket_accepts; then
        fail "a Dexter daemon is already accepting connections at $SOCKET"
    fi

    rm -f "$SOCKET" "$SHELL_SOCKET"
    : > "$CORE_LOG"
    say INFO "starting release core; log: $CORE_LOG"
    RUST_LOG=info "$CORE_BIN" >> "$CORE_LOG" 2>&1 &
    CORE_PID="$!"

    bash "$ROOT_DIR/scripts/wait-for-ready.sh" \
        --cli-bin "$CLI_BIN" \
        --timeout "$CORE_WARMUP_TIMEOUT_SECS" \
        --out "$DOCTOR_OUT" \
        --label "ui recovery model core" \
        --core-pid "$CORE_PID" \
        --core-log "$CORE_LOG"
}

start_fixture() {
    say INFO "building UI model recovery fixture"
    write_fixture_source
    swiftc "$FIXTURE_SWIFT" -o "$FIXTURE_BIN"

    say INFO "starting UI model recovery fixture"
    "$FIXTURE_BIN" >"$FIXTURE_LOG" 2>&1 &
    FIXTURE_PID="$!"

    say INFO "waiting for fixture Accessibility surface"
    for _ in {1..80}; do
        if osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
    set matchingProcesses to application processes whose name is "DexterUIModelRecoveryFixture"
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

run_model_recovery_probe() {
    local prompt
    prompt="Use Dexter UI actions against the running app named DexterUIModelRecoveryFixture. First, intentionally try to ui_click role AXButton label Missing Save. If it fails, intentionally repeat that exact same ui_click once so Dexter's UI recovery guard can block it. After the guard correction, do not click any other button. Run ui_snapshot for DexterUIModelRecoveryFixture and then stop with a short summary of the visible button labels. Do not use browser, shell, file, shortcut, or AppleScript actions."

    : > "$OUT"
    say INFO "running model-driven UI recovery probe"
    if ! "$CLI_BIN" --quiet --idle-timeout 240 "$prompt" > "$OUT" 2>&1; then
        fail "dexter-cli failed during model-driven UI recovery probe"
    fi

    grep -Fq '"type":"ui_click"' "$OUT" || fail "model did not emit a ui_click action"
    grep -Fq '"label":"Missing Save"' "$OUT" || fail "model did not attempt the intended missing label"
    grep -Fq "UI failure [control_not_found]" "$OUT" || fail "missing UI control failure was not surfaced"
    grep -Fq "Next [snapshot_then_replan]" "$OUT" || fail "structured UI recovery directive was not surfaced"
    if grep -Fq "UI recovery guard blocked a repeated target" "$OUT"; then
        grep -Fq "Allowed next actions: run ui_snapshot" "$OUT" || fail "guard correction did not surface allowed next action guidance"
        say INFO "model repeated the failed target once; guard correction was exercised"
    else
        say INFO "model obeyed the initial UI failure receipt and replanned without needing the repeat guard"
    fi
    grep -Fq '"type":"ui_snapshot"' "$OUT" || fail "model did not replan with ui_snapshot after guard correction"
    grep -Fq "Real Save" "$OUT" || fail "snapshot output did not expose the real visible button"
    if grep -Fq '"label":"Real Save"' "$OUT"; then
        fail "model clicked or targeted Real Save instead of stopping after snapshot"
    fi

    if ! "$CLI_BIN" --doctor > "$DOCTOR_OUT" 2>&1; then
        fail "doctor failed after model-driven UI recovery probe"
    fi
    grep -Eq "^(OK|PASS)[[:space:]]+browser worker[[:space:]]+ready" "$DOCTOR_OUT" || fail "daemon health was not clean after UI recovery probe"
}

main() {
    rm -f "$OUT" "$DOCTOR_OUT" "$FIXTURE_LOG" "$FIXTURE_SWIFT" "$FIXTURE_BIN"
    start_core
    start_fixture
    run_model_recovery_probe
    say PASS "model-driven UI recovery smoke passed"
}

main "$@"
