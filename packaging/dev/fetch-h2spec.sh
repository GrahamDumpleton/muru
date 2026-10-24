#!/usr/bin/env bash
# Put the h2spec HTTP/2 conformance tool at target/tools/h2spec.
#
# Release binaries exist for linux/amd64 and darwin/amd64 only; elsewhere
# the tool is built from source, which needs Go. Nothing is installed
# outside the repository's target directory.

set -euo pipefail

version="v2.6.0"
root="$(cd "$(dirname "$0")/../.." && pwd)"
tools="$root/target/tools"
binary="$tools/h2spec"

if [ -x "$binary" ]; then
    echo "h2spec is already at $binary"
    exit 0
fi
mkdir -p "$tools"

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
arch="$(uname -m)"
case "$os-$arch" in
    linux-x86_64|darwin-x86_64)
        archive="h2spec_${os}_amd64.tar.gz"
        url="https://github.com/summerwind/h2spec/releases/download/$version/$archive"
        echo "downloading $url"
        scratch="$(mktemp -d)"
        curl -sSL -o "$scratch/$archive" "$url"
        tar -xzf "$scratch/$archive" -C "$scratch"
        mv "$scratch/h2spec" "$binary"
        rm -rf "$scratch"
        ;;
    *)
        if ! command -v go >/dev/null; then
            echo "no h2spec release for $os/$arch and Go is not installed; skipping" >&2
            exit 0
        fi
        echo "building h2spec $version from source with Go"
        scratch="$(mktemp -d)"
        git clone -q --depth 1 --branch "$version" https://github.com/summerwind/h2spec "$scratch/h2spec"
        (cd "$scratch/h2spec" && go build -o "$binary" ./cmd/h2spec)
        rm -rf "$scratch"
        ;;
esac
chmod +x "$binary"
"$binary" --version
