#!/bin/sh
# Check the prerequisites for developing Muru and report what is missing.
#
# Plain POSIX shell so it works before uv or Rust is installed. Exits non-zero
# when a required tool is missing. `--json` prints the same report as JSON.

set -u

cd "$(dirname "$0")/.."

json=0
for arg in "$@"; do
    case "$arg" in
        --json) json=1 ;;
        -h|--help) echo "usage: scripts/doctor.sh [--json]"; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

os=$(uname -s)
case "$os" in
    Darwin) platform=macos ;;
    Linux) platform=linux ;;
    *) platform=other ;;
esac

missing_required=0
first=1

emit() {
    # emit NAME STATUS REQUIRED VERSION INSTALL NOTE
    if [ "$json" -eq 1 ]; then
        [ "$first" -eq 1 ] || printf ',\n'
        first=0
        printf '    {"name": "%s", "status": "%s", "required": %s, "version": "%s", "install": "%s", "note": "%s"}' \
            "$1" "$2" "$3" "$4" "$5" "$6"
    else
        printf '%-12s %-8s %-28s %s\n' "$1" "$2" "$4" "${6:-$5}"
    fi
}

version_of() {
    "$@" 2>/dev/null | head -n 1 | tr -d '"' | cut -c1-28
}

check_tool() {
    # check_tool NAME REQUIRED MACOS_INSTALL LINUX_INSTALL [VERSION_COMMAND...]
    name=$1; required=$2; mac=$3; lin=$4; shift 4
    case "$platform" in
        macos) install=$mac ;;
        *) install=$lin ;;
    esac
    if command -v "$name" >/dev/null 2>&1; then
        if [ "$#" -gt 0 ]; then ver=$(version_of "$@"); else ver=$(version_of "$name" --version); fi
        emit "$name" found "$required" "$ver" "$install" ""
    else
        if [ "$required" = true ]; then
            missing_required=1
            emit "$name" missing true "" "$install" "required: $install"
        else
            emit "$name" missing false "" "$install" "optional: $install"
        fi
    fi
}

[ "$json" -eq 1 ] && printf '{\n  "platform": "%s",\n  "tools": [\n' "$platform"
[ "$json" -eq 0 ] && printf '%-12s %-8s %-28s %s\n' TOOL STATUS VERSION NOTE

check_tool cc true "xcode-select --install" "apt install build-essential pkg-config" cc --version
check_tool git true "xcode-select --install" "apt install git"
check_tool rustup true "brew install rustup, then rustup toolchain install stable" "see https://rustup.rs"
check_tool cargo true "rustup toolchain install stable, and put \$(brew --prefix rustup)/bin on PATH in ~/.zprofile" "rustup toolchain install stable, and put ~/.cargo/bin on PATH"
check_tool uv true "brew install uv" "see https://docs.astral.sh/uv/getting-started/installation/"
check_tool just true "brew install just" "apt install just, or cargo install just"
check_tool cargo-nextest true "just bootstrap" "just bootstrap" cargo nextest --version
check_tool cargo-llvm-cov true "just bootstrap" "just bootstrap" cargo llvm-cov --version
check_tool cargo-deny true "just bootstrap" "just bootstrap" cargo deny --version
check_tool cargo-audit true "just bootstrap" "just bootstrap" cargo audit --version
check_tool cargo-fuzz false "just bootstrap-fuzz" "just bootstrap-fuzz" cargo fuzz --version
check_tool docker false "brew install --cask orbstack" "install Docker Engine or Podman"
check_tool podman false "brew install podman" "apt install podman"
check_tool limactl false "brew install lima" "not needed on Linux"
check_tool oha false "brew install oha" "cargo install oha"
check_tool h2spec false "just bootstrap-conformance" "just bootstrap-conformance"
check_tool gh false "brew install gh" "apt install gh"

[ "$json" -eq 1 ] && printf '\n  ],\n  "checks": [\n'
first=1

# Rust toolchain pinned by rust-toolchain.toml.
if command -v rustup >/dev/null 2>&1; then
    active=$(rustup show active-toolchain 2>/dev/null | head -n 1 | cut -d' ' -f1)
    emit toolchain found false "${active:-unknown}" "rustup show" "rust-toolchain.toml selects the toolchain"
fi

# Python interpreters managed by uv.
if command -v uv >/dev/null 2>&1; then
    for py in 3.11 3.12 3.13 3.14 3.14t 3.15 3.15t; do
        if uv python find "$py" >/dev/null 2>&1; then
            emit "python $py" found false "$(uv python find "$py" 2>/dev/null | cut -c1-28)" "uv python install $py" ""
        else
            emit "python $py" missing false "" "uv python install $py" "installed by just bootstrap"
        fi
    done
fi

# Descriptor limit: macOS often defaults to 256, which is too low for many connections.
nofile=$(ulimit -n 2>/dev/null || echo unknown)
if [ "$nofile" != unknown ] && [ "$nofile" != unlimited ] && [ "$nofile" -lt 4096 ]; then
    emit nofile warn false "$nofile" "ulimit -n 65536" "soft descriptor limit is low; muructl raises it at start-up"
else
    emit nofile found false "$nofile" "" ""
fi

# Temporary directory path length, for the 104-byte Unix socket path limit on macOS.
tmp=${TMPDIR:-/tmp}
if [ "${#tmp}" -gt 60 ]; then
    emit tmpdir warn false "${#tmp} chars" "" "long temporary directory; run directories default to a short location"
else
    emit tmpdir found false "${#tmp} chars" "" ""
fi

# Which pytest markers will be skipped here.
skipped="requires_root slow requires_network"
if ! command -v docker >/dev/null 2>&1 && ! command -v podman >/dev/null 2>&1; then
    skipped="$skipped requires_docker"
fi
[ "$platform" = linux ] || skipped="$skipped requires_linux"
emit markers info false "" "" "skipped by default: $skipped"

if [ "$json" -eq 1 ]; then
    printf '\n  ],\n  "ok": %s\n}\n' "$([ "$missing_required" -eq 0 ] && echo true || echo false)"
else
    echo
    if [ "$missing_required" -eq 0 ]; then
        echo "all required tools found"
    else
        echo "required tools are missing; install them and run 'just doctor' again" >&2
    fi
fi

exit "$missing_required"
