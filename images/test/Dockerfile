# Image for the container scenarios of the integration suite (design
# specification section 15.2, the requires_docker marker): muructl and the
# runtime library built from the working tree, with the fixture applications,
# on a plain Python image. tests/integration/test_containers.py builds it
# from the repository root; nothing else uses it.

FROM python:3.13-slim-bookworm AS build

ENV PATH=/root/.cargo/bin:$PATH \
    DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential pkg-config curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
    && pip install --no-cache-dir maturin

WORKDIR /src
COPY . .

# The toolchain the tree pins is installed on first use; a debug build is
# enough for the scenarios and takes half the time of a release build.
RUN cargo build -p muructl \
    && cd packages/muru \
    && maturin build --out /wheels

FROM python:3.13-slim-bookworm

# The daemon runs as root in a container and starts the edge and the
# applications as the service user, which must exist (ADR 0026).
RUN useradd --system --no-create-home --shell /usr/sbin/nologin muru

COPY --from=build /src/target/debug/muructl /usr/local/bin/muructl
COPY --from=build /wheels /wheels
RUN pip install --no-cache-dir /wheels/*.whl && rm -rf /wheels
COPY tests/fixtures/apps /apps
COPY tests/fixtures/projects /projects
WORKDIR /apps
