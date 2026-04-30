#!/usr/bin/env bash
# scripts/setup.sh — Dexter developer environment checker
#
# Validates that all required tools are present and meet minimum version
# requirements. Reports each check explicitly with pass/fail status.
# Exits 1 if any tool is missing or below the required version.
#
# This script diagnoses only — it does NOT install anything. Installation
# commands are printed alongside each failure so the operator knows exactly
# what to run.
#
# Also creates ~/.dexter/state/ if it does not exist. This is the one
# side-effecting action the script takes, and it is idempotent.

set -euo pipefail

PASS="✓"
FAIL="✗"
WARN="⚠"
overall_ok=true

# ── Helpers ───────────────────────────────────────────────────────────────────

# Check whether $1 >= $2 using sort -V (semantic version aware).
# Returns 0 (true) if the installed version meets the minimum.
version_gte() {
    local installed="$1"
    local required="$2"
    # `sort -V` sorts version strings semantically. If the minimum version sorts
    # first (or is equal), the installed version meets the requirement.
    [ "$(printf '%s\n%s\n' "$required" "$installed" | sort -V | head -1)" = "$required" ]
}

pass() {
    printf "  %s  %s\n" "$PASS" "$1"
}

fail() {
    printf "  %s  %s\n" "$FAIL" "$1" >&2
    overall_ok=false
}

# ── Tool checks ───────────────────────────────────────────────────────────────

echo ""
echo "==> Checking required toolchains"

# rustc ─────────────────────────────────────────────────────────────────────
if command -v rustc &>/dev/null; then
    rustc_ver="$(rustc --version | awk '{print $2}')"
    if version_gte "$rustc_ver" "1.92.0"; then
        pass "rustc $rustc_ver (>= 1.92.0 required)"
    else
        fail "rustc $rustc_ver found but >= 1.92.0 required"
        echo "       Install: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    fi
else
    fail "rustc not found"
    echo "       Install: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

# cargo ──────────────────────────────────────────────────────────────────────
if command -v cargo &>/dev/null; then
    cargo_ver="$(cargo --version | awk '{print $2}')"
    if version_gte "$cargo_ver" "1.92.0"; then
        pass "cargo $cargo_ver (>= 1.92.0 required)"
    else
        fail "cargo $cargo_ver found but >= 1.92.0 required"
        echo "       Install: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    fi
else
    fail "cargo not found (normally ships with rustc)"
    echo "       Install: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

# swift ──────────────────────────────────────────────────────────────────────
if command -v swift &>/dev/null; then
    # swift --version output format: "swift-driver version: X.Y.Z Apple Swift version A.B.C ..."
    # We want the "Apple Swift version" segment.
    swift_ver="$(swift --version 2>&1 | grep -oE 'Swift version [0-9]+\.[0-9]+' | awk '{print $3}' | head -1)"
    if version_gte "$swift_ver" "6.0"; then
        pass "swift $swift_ver (>= 6.0 required)"
    else
        fail "swift $swift_ver found but >= 6.0 required"
        echo "       Install: Install Xcode from the App Store (includes Swift 6)"
    fi
else
    fail "swift not found"
    echo "       Install: Install Xcode from the App Store"
fi

# python3 ────────────────────────────────────────────────────────────────────
if command -v python3 &>/dev/null; then
    python_ver="$(python3 --version | awk '{print $2}')"
    if version_gte "$python_ver" "3.12"; then
        pass "python3 $python_ver (>= 3.12 required)"
    else
        fail "python3 $python_ver found but >= 3.12 required"
        echo "       Install: brew install python@3.12"
    fi
else
    fail "python3 not found"
    echo "       Install: brew install python@3.12"
fi

echo ""
echo "==> Checking protoc and plugins"

# protoc ─────────────────────────────────────────────────────────────────────
if command -v protoc &>/dev/null; then
    protoc_ver="$(protoc --version | awk '{print $2}')"
    pass "protoc $protoc_ver"
else
    fail "protoc not found"
    echo "       Install: brew install protobuf"
fi

# protoc-gen-swift ───────────────────────────────────────────────────────────
if command -v protoc-gen-swift &>/dev/null; then
    pass "protoc-gen-swift $(protoc-gen-swift --version 2>/dev/null || echo '(version unknown)')"
else
    fail "protoc-gen-swift not found"
    echo "       Install: brew install swift-protobuf"
fi

# protoc-gen-grpc-swift-2 ────────────────────────────────────────────────────
# grpc-swift 2.x ships as `protoc-gen-grpc-swift-2`. It is installed by Homebrew
# under a versioned Cellar path — we search there because it may not be in PATH.
grpc_plugin="$(find /opt/homebrew/Cellar/grpc-swift -name "protoc-gen-grpc-swift-2" 2>/dev/null | head -1 || true)"
if [ -n "$grpc_plugin" ] || command -v protoc-gen-grpc-swift-2 &>/dev/null; then
    pass "protoc-gen-grpc-swift-2 found"
else
    fail "protoc-gen-grpc-swift-2 not found"
    echo "       Install: brew install grpc-swift"
fi

echo ""
echo "==> Checking inference runtime"

# ollama ─────────────────────────────────────────────────────────────────────
if command -v ollama &>/dev/null; then
    ollama_ver="$(ollama --version 2>/dev/null | awk '{print $NF}' || echo 'unknown')"
    pass "ollama $ollama_ver"
else
    fail "ollama not found"
    echo "       Install: brew install ollama"
fi

# ── State directory ───────────────────────────────────────────────────────────

echo ""
echo "==> Checking state directory"

state_dir="${HOME}/.dexter/state"
if [ -d "$state_dir" ]; then
    pass "~/.dexter/state/ exists"
else
    # setup.sh only creates the default path. The binary creates the configured
    # path (which may differ) at runtime. See design note in PHASE_2_PLAN.md §8.
    mkdir -p "$state_dir"
    pass "~/.dexter/state/ created"
fi

# ── Result ────────────────────────────────────────────────────────────────────

echo ""
if [ "$overall_ok" = "true" ]; then
    echo "==> All checks passed"
    exit 0
else
    echo "==> One or more checks failed — see above for installation instructions" >&2
    exit 1
fi
