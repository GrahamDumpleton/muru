# Development image: pinned Rust toolchain, uv with every supported CPython
# (including free-threaded builds), maturin, nox, just and the external test
# tools. Used by .devcontainer and `just test-linux`. Skeleton only; the tool
# versions are pinned when the image is first built in CI.

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive \
    PATH=/root/.cargo/bin:/root/.local/bin:$PATH

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential pkg-config git curl ca-certificates just \
    && rm -rf /var/lib/apt/lists/*

# Rust toolchain, selected by rust-toolchain.toml at build time.
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
    && rustup component add rustfmt clippy llvm-tools-preview

# uv and the supported Python interpreters.
RUN curl -LsSf https://astral.sh/uv/install.sh | sh \
    && uv python install 3.11 3.12 3.13 3.14 3.14t 3.15 3.15t

WORKDIR /workspace
