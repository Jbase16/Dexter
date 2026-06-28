#!/usr/bin/env bash
# Prepare this Mac for a clean Dexter operator launch.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXPECTED_MODELS_DIR="/Users/jason/ollama-models"
CONFIG_PATH="${DEXTER_CONFIG:-$HOME/.dexter/config.toml}"

info() {
    printf '[INFO] %s\n' "$1"
}

pass() {
    printf '[PASS] %s\n' "$1"
}

fail() {
    printf '[FAIL] %s\n' "$1" >&2
    exit 1
}

require_command() {
    local name="$1"
    command -v "$name" >/dev/null 2>&1 || fail "required command not found: $name"
}

required_models() {
    python3 - "$CONFIG_PATH" <<'PY'
import os
import sys
import tomllib

config_path = os.path.expanduser(sys.argv[1])
defaults = {
    "fast": "qwen3:8b",
    "primary": "gemma4:26b",
    "heavy": "deepseek-r1:32b",
    "code": "deepseek-coder-v2:16b",
    "vision": "gemma4:26b",
    "embed": "mxbai-embed-large",
}

models = dict(defaults)
try:
    with open(config_path, "rb") as handle:
        config = tomllib.load(handle)
except FileNotFoundError:
    config = {}
except tomllib.TOMLDecodeError as exc:
    raise SystemExit(f"config parse failed: {config_path}: {exc}")

configured_models = config.get("models", {})
if configured_models is None:
    configured_models = {}
if not isinstance(configured_models, dict):
    raise SystemExit(f"config parse failed: [models] must be a table in {config_path}")

for key in defaults:
    value = configured_models.get(key)
    if value is None:
        continue
    if not isinstance(value, str):
        raise SystemExit(f"config parse failed: models.{key} must be a string")
    value = value.strip()
    if not value:
        raise SystemExit(f"config parse failed: models.{key} is empty")
    models[key] = value

seen = set()
for value in models.values():
    if value in seen:
        continue
    seen.add(value)
    print(value)
PY
}

verify_ollama_models() {
    require_command ollama
    require_command python3

    models=()
    local model
    while IFS= read -r model; do
        [[ -n "$model" ]] && models+=("$model")
    done < <(required_models)
    if [[ "${#models[@]}" -eq 0 ]]; then
        fail "no required Ollama models resolved from $CONFIG_PATH"
    fi

    local list_output
    if ! list_output="$(OLLAMA_MODELS="$EXPECTED_MODELS_DIR" ollama list 2>&1)"; then
        printf '%s\n' "$list_output" >&2
        fail "ollama list failed with OLLAMA_MODELS=$EXPECTED_MODELS_DIR"
    fi

    local available
    available="$(printf '%s\n' "$list_output" | awk 'NR > 1 { print $1 }')"

    local missing=()
    local lookup
    for model in "${models[@]}"; do
        lookup="$model"
        if [[ "$lookup" != *:* ]]; then
            lookup="$lookup:latest"
        fi
        if ! grep -Fxq "$model" <<< "$available" && ! grep -Fxq "$lookup" <<< "$available"; then
            missing+=("$model")
        fi
    done

    if [[ "${#missing[@]}" -gt 0 ]]; then
        printf '[FAIL] missing required Ollama model(s) from %s:\n' "$EXPECTED_MODELS_DIR" >&2
        printf '  %s\n' "${missing[@]}" >&2
        printf '\n[INFO] Visible models:\n' >&2
        printf '%s\n' "$list_output" >&2
        exit 1
    fi

    pass "Ollama can see Dexter's configured model set in $EXPECTED_MODELS_DIR"
}

info "stopping stale Dexter processes and sockets"
bash "$ROOT_DIR/scripts/stop-dexter.sh" --quiet
pass "stale Dexter state is clean"

info "configuring launchctl OLLAMA_MODELS"
bash "$ROOT_DIR/scripts/configure-ollama-models-env.sh"

info "verifying Ollama model visibility"
verify_ollama_models

info "building release Rust core and CLI"
(cd "$ROOT_DIR/src/rust-core" && cargo build --release --bin dexter-core --bin dexter-cli)
pass "release Rust artifacts are built"

info "building Swift app"
(cd "$ROOT_DIR/src/swift" && swift build 2>&1 | tail -12)
pass "Swift app builds"

info "installing Dock launcher"
bash "$ROOT_DIR/scripts/install-dexter-app.sh" >/tmp/dexter-operator-ready-install.out
cat /tmp/dexter-operator-ready-install.out
pass "Dock launcher installed"

cat <<'TEXT'

Dexter operator readiness complete.

Start Dexter:
  open "$HOME/Applications/Dexter.app"

Or from this repo:
  make run

After Dexter is running:
  make status

To inspect saved acceptance evidence:
  make acceptance-status
  make acceptance-status-strict

To run one fresh main acceptance receipt:
  make live-smoke-acceptance

Focused acceptance slices:
  make live-smoke-operator-controls
  make live-smoke-runtime-health
  make live-smoke-action-safety-shared
  make live-smoke-action-safety
  make live-smoke-action-safety-full

TEXT
