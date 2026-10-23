"""Muru runtime library.

This package runs inside every Muru host and worker process. It is also safe
to import outside Muru, where every function returns development defaults, so
applications can use it unconditionally.

The repository holds only the skeleton so far; see the design specification
(docs/project/design, section 6) for the planned API.
"""

from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version

__all__ = ["__version__"]

# The version is written into the wheel metadata by maturin from Cargo.toml.
# When the package is imported from the source tree without being installed,
# there is no metadata to read.
try:
    __version__ = version("muru")
except PackageNotFoundError:
    __version__ = "0+unknown"
