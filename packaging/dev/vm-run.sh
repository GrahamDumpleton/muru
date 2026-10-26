#!/bin/bash
# Runs inside the Linux VM (design specification section 15.6): copies the
# source tree from the read-only host mount into the VM's own disk, builds
# muructl and the extension there, and runs the test suites or the benchmarks.
#
#   vm-run.sh SOURCE test [pytest args]
#   vm-run.sh SOURCE bench [bench args]
#
# Invoked by `just vm-test` and `just vm-bench`; SOURCE is the repository
# path on the host, which Lima mounts at the same path.

set -euo pipefail

source=$1
action=$2
shift 2
work=$HOME/muru

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

need_packages=()
command -v rsync >/dev/null || need_packages+=(rsync)
if [ "$action" = bench ] && { [ ! -e /usr/bin/apxs ] || [ ! -e /usr/sbin/apache2 ]; }; then
    need_packages+=(apache2-bin apache2-dev)
fi
if [ "${#need_packages[@]}" -gt 0 ]; then
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${need_packages[@]}"
fi

mkdir -p "$work"
rsync -a --delete \
    --exclude .git --exclude target --exclude .venv --exclude scratch \
    --exclude docs/_build --exclude bench/results --exclude '__pycache__' \
    --exclude '*.so' --exclude .nox --exclude .pytest_cache --exclude .mypy_cache \
    --exclude .ruff_cache \
    "$source/" "$work/"
cd "$work"

uv python install 3.15 3.15t
uv sync --only-group dev --only-group fixtures --only-group grpc --python 3.15
(cd packages/muru && uv run --project ../.. maturin develop)

case "$action" in
    test)
        cargo build -p muructl
        uv run pytest "$@"
        uv run pytest tests/integration tests/conformance "$@"
        # The same integration suite on the free-threaded build.
        rm -rf scratch/venv-315t scratch/wheels
        uv venv -q -p 3.15t scratch/venv-315t
        (cd packages/muru && uvx --from 'maturin>=1.8,<2' maturin build -i "$(uv python find 3.15t)" --out ../../scratch/wheels)
        # The same test and fixture packages as the lock file gives 3.15,
        # without the grpc group: grpcio has no free-threaded wheels, so
        # gRPC mounts are unavailable there (section 6.13).
        uv export -q --no-hashes --no-emit-project --only-group dev --only-group fixtures \
            --format requirements.txt -o scratch/requirements-315t.txt
        uv pip install -q --python scratch/venv-315t/bin/python \
            -r scratch/requirements-315t.txt scratch/wheels/muru-*cp315t*.whl
        scratch/venv-315t/bin/python -m pytest -p no:cacheprovider -o pythonpath=tests tests/integration tests/conformance "$@"
        # The scenarios that need root run a daemon through sudo; the
        # users it switches to must be able to read the source tree and
        # the interpreter, so the home directory opens to them.
        chmod o+rx "$HOME"
        uv run pytest -m requires_root tests/integration
        ;;
    bench)
        cargo build --release -p muructl
        command -v oha >/dev/null || cargo install oha --locked
        uv sync --group bench --python 3.15
        (cd packages/muru && uv run --project ../.. maturin develop)
        uv pip install mod_wsgi
        uv run --group bench python bench/run.py "$@"
        ;;
    *)
        echo "unknown action: $action" >&2
        exit 2
        ;;
esac
