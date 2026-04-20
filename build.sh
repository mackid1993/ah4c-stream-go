#!/usr/bin/env bash
# Build a static ah4c-stream binary for the AH4C Docker container.
# Default target: linux/amd64. Pass "arm64" as the first arg for arm64.
#
#   ./build.sh          # → ah4c-stream (linux/amd64)
#   ./build.sh arm64    # → ah4c-stream (linux/arm64)
#   ./build.sh both     # → ah4c-stream-amd64 + ah4c-stream-arm64

set -euo pipefail
cd "$(dirname "$0")"

build() {
    local arch="$1" out="$2"
    echo "building $out for linux/$arch..."
    GOOS=linux GOARCH="$arch" CGO_ENABLED=0 \
        go build -ldflags="-s -w" -trimpath -o "$out" .
    file "$out"
    ls -lh "$out"
}

case "${1:-amd64}" in
    amd64) build amd64 ah4c-stream ;;
    arm64) build arm64 ah4c-stream ;;
    both)
        build amd64 ah4c-stream-amd64
        build arm64 ah4c-stream-arm64
        ;;
    *) echo "usage: $0 [amd64|arm64|both]" >&2; exit 2 ;;
esac
