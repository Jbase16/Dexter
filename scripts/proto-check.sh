#!/usr/bin/env bash
# Verify generated proto bindings without modifying checked-in files.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROTO_DIR="$REPO_ROOT/src/shared/proto"
PROTO_FILE="$PROTO_DIR/dexter.proto"
SWIFT_GEN_DIR="$REPO_ROOT/src/swift/Sources/Dexter/Bridge/generated"
RUST_CORE_DIR="$REPO_ROOT/src/rust-core"

PROTOC_GEN_SWIFT="${PROTOC_GEN_SWIFT:-$(command -v protoc-gen-swift || true)}"
if [[ -z "${PROTOC_GEN_GRPC_SWIFT:-}" ]]; then
    PROTOC_GEN_GRPC_SWIFT="$(
        find /opt/homebrew/Cellar/protoc-gen-grpc-swift \
            -name "protoc-gen-grpc-swift-2" -type f 2>/dev/null |
            sort |
            tail -1
    )"
fi

if ! command -v protoc >/dev/null 2>&1; then
    echo "proto-check: protoc is unavailable; run: make setup" >&2
    exit 1
fi
if [[ -z "$PROTOC_GEN_SWIFT" || ! -x "$PROTOC_GEN_SWIFT" ]]; then
    echo "proto-check: protoc-gen-swift is unavailable; run: make setup" >&2
    exit 1
fi
if [[ -z "$PROTOC_GEN_GRPC_SWIFT" || ! -x "$PROTOC_GEN_GRPC_SWIFT" ]]; then
    echo "proto-check: protoc-gen-grpc-swift-2 is unavailable; run: make setup" >&2
    exit 1
fi

TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dexter-proto-check.XXXXXX")"
cleanup() {
    rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

echo "==> Generating Swift proto bindings in a temporary directory"
protoc \
    --proto_path="$PROTO_DIR" \
    --plugin="protoc-gen-swift=$PROTOC_GEN_SWIFT" \
    --plugin="protoc-gen-grpc-swift-2=$PROTOC_GEN_GRPC_SWIFT" \
    --swift_out="$TEMP_DIR" \
    --grpc-swift-2_out="$TEMP_DIR" \
    "$PROTO_FILE"

mismatches=()
for generated_name in dexter.pb.swift dexter.grpc.swift; do
    if [[ ! -f "$TEMP_DIR/$generated_name" ]] ||
        ! cmp -s "$TEMP_DIR/$generated_name" "$SWIFT_GEN_DIR/$generated_name"; then
        mismatches+=("$generated_name")
    fi
done

if (( ${#mismatches[@]} > 0 )); then
    echo "proto-check: checked-in Swift bindings differ:" >&2
    printf "  %s\n" "${mismatches[@]}" >&2
    echo "Regenerate them with: make proto" >&2
    exit 1
fi
echo "==> Checked-in Swift proto bindings match"

echo "==> Compiling the Rust core against the current proto"
if ! (cd "$RUST_CORE_DIR" && cargo check --bin dexter-core); then
    echo "proto-check: Rust proto compilation failed" >&2
    echo "Regenerate Swift bindings with: make proto" >&2
    echo "After resolving the error, verify bindings with: make proto-check" >&2
    exit 1
fi

echo "==> Proto check passed"
