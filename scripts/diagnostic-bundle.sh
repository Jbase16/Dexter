#!/usr/bin/env bash
# Write one low-risk Dexter diagnostic report without starting a live session.

set -u
set -o pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${DEXTER_DIAGNOSTIC_DIR:-$ROOT_DIR/docs/diagnostics}"
STAMP="$(date '+%Y%m%d_%H%M%S')"
REPORT="$OUT_DIR/dexter-diagnostic-$STAMP.md"
LATEST="$OUT_DIR/latest.md"
CLI="$ROOT_DIR/src/rust-core/target/release/dexter-cli"
INCLUDE_STATUS="${DEXTER_DIAGNOSTIC_INCLUDE_STATUS:-0}"

cd "$ROOT_DIR"
mkdir -p "$OUT_DIR"

markdown_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//|/\\|}"
    value="${value//$'\n'/ }"
    printf '%s' "$value"
}

run_block() {
    local title="$1"
    shift

    {
        echo "## $title"
        echo
        echo '```text'
    } >> "$REPORT"

    "$@" >> "$REPORT" 2>&1
    local status=$?

    {
        echo '```'
        echo
        echo "Exit status: \`$status\`"
        echo
    } >> "$REPORT"
}

run_shell_block() {
    local title="$1"
    local script="$2"
    run_block "$title" bash -lc "$script"
}

{
    echo "# Dexter Diagnostic Bundle"
    echo
    echo "- Created: \`$(date '+%Y-%m-%dT%H:%M:%S%z')\`"
    echo "- Root: \`$(markdown_escape "$ROOT_DIR")\`"
    echo "- Include full operator status: \`$INCLUDE_STATUS\`"
    echo
    echo "This report intentionally avoids full transcripts by default. Set"
    echo "\`DEXTER_DIAGNOSTIC_INCLUDE_STATUS=1\` to include \`dexter-cli --status\`."
    echo
} > "$REPORT"

run_shell_block "System" 'date; sw_vers 2>/dev/null || true; uname -a'
run_shell_block "Dexter Processes" "ps axww | rg '[d]exter-core|[s]wift run|\\.build/.+[D]exter|target/release/[d]exter-core|[D]exterLauncher' || true"
run_shell_block "Dexter Sockets" "ls -l /tmp/dexter.sock /tmp/dexter-shell.sock 2>/dev/null || true; lsof -nU 2>/dev/null | rg 'dexter\\.sock|dexter-shell\\.sock' || true"
run_shell_block "Ollama Environment" 'printf "process OLLAMA_MODELS=%s\n" "${OLLAMA_MODELS:-unset}"; printf "launchctl OLLAMA_MODELS=%s\n" "$(launchctl getenv OLLAMA_MODELS 2>/dev/null || true)"'
run_shell_block "Model Stores" 'ls -ld /Users/jason/ollama-models /Volumes/BitHappens/ollama-models /Volumes/ByteMe 2>/dev/null || true'
run_shell_block "Ollama Models" 'OLLAMA_MODELS=/Users/jason/ollama-models ollama list 2>&1 || true'
run_shell_block "Ollama Runners" 'OLLAMA_MODELS=/Users/jason/ollama-models ollama ps 2>&1 || true'
run_shell_block "Disk" 'df -h /Users/jason/.dexter/state /Users/jason/Developer/Dex /tmp 2>/dev/null || true'
run_shell_block "Dock Launcher" 'ls -ld "$HOME/Applications/Dexter.app" "$HOME/Applications/Dexter.app/Contents/MacOS/DexterLauncher" 2>/dev/null || true; /usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$HOME/Applications/Dexter.app/Contents/Info.plist" 2>/dev/null || true'
run_shell_block "Dock Launcher Command" 'if [[ -f "$HOME/Applications/Dexter.app/Contents/MacOS/DexterLauncher" ]]; then sed -n "1,180p" "$HOME/Applications/Dexter.app/Contents/MacOS/DexterLauncher"; else echo "Dexter launcher is not installed."; fi'

if [[ -x "$CLI" ]]; then
    run_block "Doctor" "$CLI" --doctor
    if [[ "$INCLUDE_STATUS" == "1" || "$INCLUDE_STATUS" == "true" ]]; then
        run_block "Operator Status" "$CLI" --status
    else
        {
            echo "## Operator Status"
            echo
            echo "Skipped by default. Re-run with:"
            echo
            echo '```bash'
            echo "DEXTER_DIAGNOSTIC_INCLUDE_STATUS=1 make diagnostic-bundle"
            echo '```'
            echo
        } >> "$REPORT"
    fi
else
    {
        echo "## Doctor"
        echo
        echo "Skipped because release dexter-cli is not built at:"
        echo
        echo '```text'
        echo "$CLI"
        echo '```'
        echo
    } >> "$REPORT"
fi

if [[ -f "$ROOT_DIR/docs/live-smoke-results/latest.md" ]]; then
    run_shell_block "Latest Live Smoke Summary" "sed -n '1,90p' '$ROOT_DIR/docs/live-smoke-results/latest.md'"
fi

if [[ -f "$ROOT_DIR/docs/live-smoke-results/index.md" ]]; then
    run_shell_block "Recent Live Smoke Index" "awk '/^## Latest$/ { exit } { print }' '$ROOT_DIR/docs/live-smoke-results/index.md' | sed -n '1,140p'"
fi

if [[ -x "$ROOT_DIR/scripts/acceptance-status.sh" ]]; then
    run_block "Acceptance Status" "$ROOT_DIR/scripts/acceptance-status.sh"
fi

cp "$REPORT" "$LATEST"

printf '[INFO] diagnostic bundle written: %s\n' "$REPORT"
printf '[INFO] latest diagnostic bundle: %s\n' "$LATEST"
