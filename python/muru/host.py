"""Host process entry point, run as ``python -m muru.host``.

The host preloads the application, forks and supervises workers, and acts as
PID 1 in containers (design specification sections 6.1, 6.2 and 6.10). This
is a placeholder created with the project skeleton.
"""

from __future__ import annotations

import sys


def main(argv: list[str] | None = None) -> int:
    """Run the host. Not implemented yet; returns a non-zero exit status."""

    del argv
    print("muru.host is not implemented yet; see docs/project/roadmap.md", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
