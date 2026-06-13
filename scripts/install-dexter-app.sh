#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app_dir="${1:-"$HOME/Applications/Dexter.app"}"
macos_dir="$app_dir/Contents/MacOS"
repo_applescript="${repo_dir//\\/\\\\}"
repo_applescript="${repo_applescript//\"/\\\"}"
app_applescript="${app_dir//\\/\\\\}"
app_applescript="${app_applescript//\"/\\\"}"

mkdir -p "$macos_dir"

cat > "$app_dir/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>DexterLauncher</string>
    <key>CFBundleIdentifier</key>
    <string>com.jason.dexter.launcher</string>
    <key>CFBundleName</key>
    <string>Dexter</string>
    <key>CFBundleDisplayName</key>
    <string>Dexter</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>15.0</string>
    <key>LSUIElement</key>
    <false/>
</dict>
</plist>
PLIST

cat > "$macos_dir/DexterLauncher" <<LAUNCHER
#!/usr/bin/env zsh
set -euo pipefail

osascript <<OSA
set repoPath to "$repo_applescript"
set appPath to "$app_applescript"
tell application "Terminal"
    activate
    set dexterTab to do script "cd " & quoted form of repoPath & "; export OLLAMA_MODELS=/Users/jason/ollama-models; clear; echo 'Dexter live logs'; echo 'Started from: " & appPath & "'; echo 'OLLAMA_MODELS=/Users/jason/ollama-models'; echo; echo 'Use Dexter > New Session for a fresh conversation.'; echo 'Use Dexter > Restart Dexter to restart the app/core.'; echo 'Use Dexter > Quit Dexter to stop the app/core.'; echo; make configure-ollama-models && make stop && make run"
    set custom title of dexterTab to "Dexter Live Logs"
end tell
OSA
LAUNCHER

chmod +x "$macos_dir/DexterLauncher"

echo "Installed Dexter launcher app:"
echo "  $app_dir"
echo
echo "You can open it with:"
echo "  open '$app_dir'"
echo
echo "Add it to the Dock by dragging it from Finder:"
echo "  $HOME/Applications"
