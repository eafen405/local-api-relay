#!/usr/bin/env bash
# build-archive.sh — builds the release binary and assembles the self-contained,
# versioned Linux x86_64 archive for local-api-relay (PKG-002).
#
# The archive contains exactly the Rust binary, the idempotent installer, and
# the lifecycle script. Production installs require no package repository, no
# root-owned system directories, no container runtime, no Node.js, and no
# desktop shell.
#
# Usage: packaging/build-archive.sh
# Output: dist/local-api-relay-<version>.tar.gz
#
# Requires the Rust toolchain on PATH (see the repo handoff for the exact
# environment variables when the toolchain is a temporary installation).
set -eu

cd "$(dirname -- "$0")/.."

cargo build --release

binary="target/release/local-api-relay"
if [ ! -f "$binary" ]; then
    echo "build-archive: release binary missing at $binary" >&2
    exit 1
fi

version=$("$binary" --version | awk '{print $2}')
if [ -z "$version" ]; then
    echo "build-archive: could not determine the binary version" >&2
    exit 1
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

cp "$binary" "$stage/local-api-relay"
cp packaging/install.sh "$stage/install.sh"
cp packaging/local-api-relay-service "$stage/local-api-relay-service"

mkdir -p dist
output="dist/local-api-relay-$version.tar.gz"
tar -C "$stage" -czf "$output" .

echo "archive: $output"
