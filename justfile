# Task runner entry points for Muru. Run `just` to list them.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Crates that build without a Python interpreter (muru-py needs one).
default_crates := "--workspace --exclude muru-py"

# List the available recipes.
default:
    @just --list --unsorted

# Check every prerequisite and print install commands for what is missing.
doctor *args:
    sh scripts/doctor.sh {{args}}

# Install Rust components, cargo tools, Python interpreters and the dev environment.
bootstrap:
    rustup show active-toolchain >/dev/null
    rustup component add rustfmt clippy llvm-tools-preview
    just _cargo-tool cargo-nextest cargo-llvm-cov cargo-deny cargo-audit
    uv python install 3.11 3.12 3.13 3.14 3.14t 3.15 3.15t
    uv sync
    uv run pre-commit install
    uv run pre-commit install --hook-type commit-msg
    just dev

# Optional extras for the conformance suites (h2spec, Autobahn) and fuzzing.
bootstrap-conformance:
    bash packaging/dev/fetch-h2spec.sh

bootstrap-fuzz:
    rustup toolchain install nightly
    just _cargo-tool cargo-fuzz

# Build the extension into the development virtualenv. Run again after `uv sync`, which removes it.
dev:
    cd packages/muru && uv run --project ../.. maturin develop

# Formatting, lints, types and repository text checks.
lint: lint-text lint-rust lint-python

lint-text:
    sh scripts/check-no-em-dashes.sh

lint-rust:
    cargo fmt --all --check
    cargo clippy {{default_crates}} --all-targets -- -D warnings

lint-python:
    uv run ruff format --check .
    uv run ruff check .
    uv run mypy

# Apply formatters.
fmt:
    cargo fmt --all
    uv run ruff format .
    uv run ruff check --fix .

# Rust unit tests and Python unit tests.
test: test-rust test-python

test-rust:
    cargo nextest run {{default_crates}}

test-python:
    uv run pytest

# Integration scenarios that need no Docker or network (builds muructl and the extension first).
test-integration: dev
    cargo build -p muructl
    uv run pytest tests/integration

# WSGI, ASGI, HTTP and WebSocket conformance suites (builds muructl and the extension first).
conformance: dev
    cargo build -p muructl
    uv run pytest tests/conformance

# Container scenarios (needs a container runtime).
test-docker:
    uv run pytest tests/integration -m requires_docker

# Linux suites inside the development container.
test-linux:
    @echo "not yet implemented: runs the Linux suites in images/dev with the source bind-mounted"

# Run one fuzz target.
fuzz target:
    cargo +nightly fuzz run {{target}}

# Performance scenarios (Linux only for representative numbers).
bench *args:
    uv run --group bench python bench/run.py {{args}}

# Build the documentation with warnings as errors.
docs:
    uv run sphinx-build -W --keep-going -b html docs docs/_build/html

# Build the LaTeX source for the PDF, which Read the Docs turns into the PDF (no TeX needed here).
docs-latex:
    uv run sphinx-build -W --keep-going -b latex docs docs/_build/latex

# Check documentation links.
docs-linkcheck:
    uv run sphinx-build -b linkcheck docs docs/_build/linkcheck

# Build the documentation and open it in the browser.
docs-open: docs
    #!/usr/bin/env bash
    set -euo pipefail
    page="docs/_build/html/index.html"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        open "$page"
    elif command -v xdg-open >/dev/null 2>&1; then
        xdg-open "$page"
    else
        echo "open $page in a browser"
    fi

# Linux virtual machine for Linux-only tests.
vm-up:
    limactl start --name muru packaging/dev/lima.yaml

# Copy the source into the VM, build there, and run the unit and integration suites.
vm-test *args:
    limactl shell muru -- bash -lc "bash '{{justfile_directory()}}/packaging/dev/vm-run.sh' '{{justfile_directory()}}' test {{args}}"

# Copy the source into the VM, build there, and run the benchmarks (see bench/README.md).
vm-bench *args:
    limactl shell muru -- bash -lc "bash '{{justfile_directory()}}/packaging/dev/vm-run.sh' '{{justfile_directory()}}' bench {{args}}"

vm-shell:
    limactl shell muru

# Print the project version.
version:
    @grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2

# Set the project version (Cargo.toml is the only place it is stated).
bump version:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ "{{version}}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([a-z0-9.+-]*)?$ ]] || { echo "not a version: {{version}}" >&2; exit 1; }
    sed -i.bak -E 's/^version = "[^"]+"/version = "{{version}}"/' Cargo.toml && rm Cargo.toml.bak
    if command -v cargo >/dev/null 2>&1 && [[ -f Cargo.lock ]]; then cargo update --workspace --offline >/dev/null; fi
    echo "version is now $(just version)"

# Preview the changelog that the fragments would produce.
changelog-draft:
    uv run towncrier build --draft --version "$(just version-pep440)"

# Build the changelog from the fragments, for the release pull request.
changelog:
    uv run towncrier build --yes --version "$(just version-pep440)"

# Print the project version in PEP 440 form, as PyPI shows it.
version-pep440:
    @uv run python -c "from packaging.version import Version; print(Version('$(just version)'))"

# Remove build output.
clean:
    cargo clean
    rm -rf docs/_build .nox .pytest_cache .mypy_cache .ruff_cache

# Install a cargo tool with cargo-binstall when available, otherwise from source.
_cargo-tool +tools:
    for tool in {{tools}}; do \
        if command -v "$tool" >/dev/null 2>&1; then continue; fi; \
        if command -v cargo-binstall >/dev/null 2>&1; then cargo binstall -y "$tool"; else cargo install --locked "$tool"; fi; \
    done
