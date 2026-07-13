#!/usr/bin/env bash
# scripts/permissions.sh — Check macOS TCC permissions required by Dexter.
#
# Queries the TCC databases directly.
# This works because SIP is disabled on this machine (required for Dexter).
# On a SIP-enabled machine, the system TCC database is unreadable without entitlements;
# the script falls back to guidance for opening System Settings manually.
#
# macOS TCC database layout (macOS 12+):
#   System: /Library/Application Support/com.apple.TCC/TCC.db
#     → Accessibility grants (kTCCServiceAccessibility) live here
#   User:   ~/Library/Application Support/com.apple.TCC/TCC.db
#     → Microphone grants (kTCCServiceMicrophone) live here
#
# Schema note: macOS 12+ uses auth_value column (2 = allowed) instead of the
# legacy allowed column (1 = allowed) used in older versions.
#
# Permissions checked:
#   kTCCServiceAccessibility — required for AXObserver (context observation)
#   kTCCServiceMicrophone    — required for AVCaptureSession (voice input)
#   kTCCServiceScreenCapture — required for grounded Vision answers

set -euo pipefail

PASS="✓"
FAIL="✗"
WARN="⚠"
overall_ok=true

SYSTEM_TCC_DB="/Library/Application Support/com.apple.TCC/TCC.db"
USER_TCC_DB="${HOME}/Library/Application Support/com.apple.TCC/TCC.db"

# The TCC client identifier for a SwiftPM debug build or release binary.
# SwiftPM executables are not bundled — TCC uses the executable path as the identifier.
SWIFT_BUILD_DEBUG="$(find "$(pwd)/src/swift/.build" -name "Dexter" -type f 2>/dev/null | head -1 || true)"
SWIFT_INSTALLED="/Applications/Dexter.app/Contents/MacOS/Dexter"
DEXTER_CLI="$(pwd)/src/rust-core/target/release/dexter-cli"
LIVE_DOCTOR_OUTPUT=""

if [ -S /tmp/dexter.sock ] && [ -x "$DEXTER_CLI" ]; then
    LIVE_DOCTOR_OUTPUT="$($DEXTER_CLI --doctor 2>&1 || true)"
fi

echo ""
echo "==> Checking macOS TCC permissions for Dexter"

# Query a single TCC database for a given service + client combo.
# Outputs "1" if granted (auth_value=2), "0" if denied/absent.
query_tcc_db() {
    local db="$1"
    local service="$2"
    local client="$3"
    [ -f "$db" ] || { echo "0"; return; }
    local result
    result=$(sqlite3 "$db" \
        "SELECT auth_value FROM access WHERE service='${service}' AND client='${client}' LIMIT 1;" \
        2>/dev/null || true)
    if [ "$result" = "2" ]; then echo "1"; else echo "0"; fi
}

check_tcc_permission() {
    local service="$1"
    local label="$2"
    local pref_path="$3"

    # Accessibility entries live in the system TCC db; Microphone in the user TCC db.
    # Check both to handle any TCC db layout variation.
    local found=false
    for client in "$SWIFT_BUILD_DEBUG" "$SWIFT_INSTALLED" "com.apple.Terminal" "com.googlecode.iterm2" "/usr/bin/python3"; do
        [ -z "$client" ] && continue
        for db in "$SYSTEM_TCC_DB" "$USER_TCC_DB"; do
            if [ "$(query_tcc_db "$db" "$service" "$client")" = "1" ]; then
                printf "  %s  %s: granted (client: %s)\n" "$PASS" "$label" "$(basename "$client")"
                found=true
                break 2
            fi
        done
    done

    if [ "$found" = false ]; then
        # Check if the TCC databases are readable at all.
        if [ ! -f "$SYSTEM_TCC_DB" ] && [ ! -f "$USER_TCC_DB" ]; then
            printf "  %s  %s: TCC databases not readable (SIP may be enabled)\n" "$WARN" "$label"
            echo "       Open: System Settings → Privacy & Security → $label"
            echo "       Grant access to Dexter (or to Terminal during development)"
        else
            printf "  %s  %s: not found in TCC database\n" "$FAIL" "$label"
            echo "       Open: System Settings → Privacy & Security → $label"
            echo "       Add Dexter (or Terminal for development), toggle ON"
            echo "       Command: open '${pref_path}'"
        fi
        overall_ok=false
    fi
}

check_core_permission() {
    local doctor_label="$1"
    local tcc_service="$2"
    local display_label="$3"
    local pref_path="$4"

    if [ -n "$LIVE_DOCTOR_OUTPUT" ]; then
        if printf '%s\n' "$LIVE_DOCTOR_OUTPUT" | grep -qE "^OK[[:space:]]+${doctor_label}[[:space:]]"; then
            printf "  %s  %s: granted to the running signed core\n" "$PASS" "$display_label"
            return
        fi
        if printf '%s\n' "$LIVE_DOCTOR_OUTPUT" | grep -qE "^FAIL[[:space:]]+${doctor_label}[[:space:]]"; then
            printf "  %s  %s: not granted to the running signed core\n" "$FAIL" "$display_label"
            echo "       Open: System Settings → Privacy & Security → $display_label"
            echo "       Toggle the existing dexter-core row OFF and ON, then restart Dexter"
            echo "       Command: open '${pref_path}'"
            overall_ok=false
            return
        fi
    fi

    printf "  %s  %s: live core preflight unavailable; checking TCC records only\n" "$WARN" "$display_label"
    check_tcc_permission "$tcc_service" "$display_label" "$pref_path"
}

check_core_permission \
    "Accessibility permission" \
    "kTCCServiceAccessibility" \
    "Accessibility" \
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"

check_tcc_permission \
    "kTCCServiceMicrophone" \
    "Microphone" \
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"

check_core_permission \
    "Screen Recording permission" \
    "kTCCServiceScreenCapture" \
    "Screen Recording" \
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"

echo ""
if [ "$overall_ok" = "true" ]; then
    echo "==> All permissions granted"
    exit 0
else
    echo "==> One or more permissions missing — see above for instructions" >&2
    exit 1
fi
