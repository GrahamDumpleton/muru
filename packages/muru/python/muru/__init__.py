"""Muru runtime library.

This package runs inside every Muru host and worker process. It is also safe
to import outside Muru, where every function returns development defaults, so
applications can use it unconditionally.

``muru.context()`` says where the code runs; the rest of the API in section
6.9 of the design specification arrives with the features it belongs to.
"""

from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version

from muru._context import Context, context

__all__ = ["Context", "__version__", "context"]

# The version is written into the wheel metadata by maturin from Cargo.toml.
# When the package is imported from the source tree without being installed,
# there is no metadata to read.
try:
    __version__ = version("muru")
except PackageNotFoundError:
    __version__ = "0+unknown"
