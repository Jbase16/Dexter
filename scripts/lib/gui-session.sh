#!/usr/bin/env bash
# Shared helpers for live smokes that require an unlocked, interactive macOS GUI.

gui_session_status() {
    swift - <<'SWIFT'
import AppKit
import CoreGraphics

let frontmostName = NSWorkspace.shared.frontmostApplication?.localizedName ?? ""
let frontmostPid = NSWorkspace.shared.frontmostApplication.map { String($0.processIdentifier) } ?? ""

let options = CGWindowListOption(arrayLiteral: .optionOnScreenOnly, .excludeDesktopElements)
let windows = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]] ?? []
let userWindows = windows.filter { window in
    let owner = window[kCGWindowOwnerName as String] as? String ?? ""
    let layer = window[kCGWindowLayer as String] as? Int ?? -1
    if layer != 0 {
        return false
    }
    if owner == "loginwindow" || owner == "Window Server" {
        return false
    }
    return true
}

print("frontmost=\(frontmostName.isEmpty ? "<none>" : frontmostName)")
print("frontmost_pid=\(frontmostPid.isEmpty ? "<none>" : frontmostPid)")
print("user_window_count=\(userWindows.count)")

if frontmostName.isEmpty || frontmostName == "loginwindow" {
    print("status=locked_or_shielded")
} else if userWindows.isEmpty {
    print("status=no_user_windows")
} else {
    print("status=ok")
}
SWIFT
}

require_active_gui_session() {
    local status_output
    if ! status_output="$(gui_session_status 2>&1)"; then
        printf '%s\n' "$status_output"
        printf '[FAIL] Unable to inspect GUI session state before live smoke.\n'
        return 12
    fi

    if grep -Fq "status=ok" <<<"$status_output"; then
        return 0
    fi

    printf '%s\n' "$status_output"
    printf '[FAIL] Active unlocked GUI session required for this smoke. Unlock the Mac and make a normal app window frontmost, then rerun.\n'
    return 10
}
