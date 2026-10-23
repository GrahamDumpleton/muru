"""nox sessions for the Python side of Muru.

Sessions use uv to create environments, so every supported interpreter,
including free-threaded builds, is available without a system Python.
"""

from __future__ import annotations

import nox

nox.options.default_venv_backend = "uv"
nox.options.sessions = ["lint", "tests"]

PYTHONS = ["3.11", "3.12", "3.13", "3.14", "3.14t", "3.15", "3.15t"]


@nox.session(python=PYTHONS)
def tests(session: nox.Session) -> None:
    """Run the Python unit tests on one interpreter."""

    session.run_install(
        "uv",
        "sync",
        "--only-group",
        "dev",
        env={"UV_PROJECT_ENVIRONMENT": session.virtualenv.location},
    )
    session.run("pytest", *session.posargs)


@nox.session(python="3.13")
def lint(session: nox.Session) -> None:
    """Run ruff and mypy."""

    session.run_install(
        "uv",
        "sync",
        "--only-group",
        "dev",
        env={"UV_PROJECT_ENVIRONMENT": session.virtualenv.location},
    )
    session.run("ruff", "format", "--check", ".")
    session.run("ruff", "check", ".")
    session.run("mypy")


@nox.session(python="3.13")
def docs(session: nox.Session) -> None:
    """Build the documentation with warnings as errors."""

    session.run_install(
        "uv",
        "sync",
        "--only-group",
        "docs",
        env={"UV_PROJECT_ENVIRONMENT": session.virtualenv.location},
    )
    session.run("sphinx-build", "-W", "--keep-going", "-b", "html", "docs", "docs/_build/html")
