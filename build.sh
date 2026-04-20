#!/usr/bin/env bash
# Build ah4c-stream.
#
#   ./build.sh              # native build (fast local sanity check)
#   ./build.sh amd64        # linux/amd64 static (musl) via rustup
#   ./build.sh arm64        # linux/arm64 static (musl) via rustup
#   ./build.sh both         # both linux targets
#
# Linux cross-builds need rustup + musl targets + musl-cross linkers:
#   brew install rustup filosottile/musl-cross/musl-cross \
#                filosottile/musl-cross/musl-cross-aarch64
#   rustup-init -y
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
#
# If you don't want to set that up locally, push and let the GitHub Actions
# release workflow build both binaries for you.

set -euo pipefail
cd "$(dirname "$0")"

build_native() {
    echo "==> cargo build --release (native)"
    cargo build --release
    echo "==> target/release/ah4c-stream"
    ls -lh target/release/ah4c-stream
}

build_linux() {
    local arch="$1" target out
    case "$arch" in
        amd64) target="x86_64-unknown-linux-musl" ; out="ah4c-stream-amd64" ;;
        arm64) target="aarch64-unknown-linux-musl"; out="ah4c-stream-arm64" ;;
        *) echo "unknown arch: $arch" >&2; exit 2 ;;
    esac

    if ! command -v rustup >/dev/null 2>&1; then
        echo "rustup not found — can't cross-compile locally." >&2
        echo "Install it, or push and use .github/workflows/release.yml." >&2
        exit 2
    fi

    echo "==> cargo build --release --target $target"
    cargo build --release --target "$target"
    cp "target/$target/release/ah4c-stream" "$out"
    file "$out"
    ls -lh "$out"
}

case "${1:-native}" in
    native|"")       build_native ;;
    amd64)           build_linux amd64 ;;
    arm64)           build_linux arm64 ;;
    both)            build_linux amd64 ; build_linux arm64 ;;
    *) echo "usage: $0 [native|amd64|arm64|both]" >&2; exit 2 ;;
esac
