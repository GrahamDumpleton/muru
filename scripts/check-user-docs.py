"""Check that the user documentation describes what Muru does, not what is to come.

The tutorials, how-to guides, reference and explanation pages, and the home
page, say how Muru works and how to use it. What is still to come belongs
in the roadmap and the issue tracker, which stay current; a promise in a
user page goes stale the day the work ships. This lists status notes and
phrases that announce future work in those pages. A limit is stated as it
is ("trailers are not sent"), which this leaves alone.

Usage: check-user-docs.py [FILE...]

With no files, every user page under docs/ is checked; files outside the
user pages are skipped. Exits 1 when any page has one.
"""

from __future__ import annotations

import re
import subprocess
import sys

USER_PAGES = re.compile(r"^docs/(?:index\.md|(?:tutorials|howto|reference|explanation)/.*\.md)$")

FUTURE = re.compile(
    r"\{admonition\} Status"
    r"|still to come|coming soon"
    r"|not yet (?:supported|implemented|available|built)"
    r"|in a (?:later|future) (?:release|version)"
    r"|(?:is|are) planned|planned for|will be added",
    re.IGNORECASE,
)


def pages(arguments: list[str]) -> list[str]:
    """Pick the user pages to check: those named, or every one in the repository."""

    if not arguments:
        arguments = subprocess.run(
            ["git", "ls-files", "docs"], capture_output=True, text=True, check=True
        ).stdout.split()

    return [path for path in arguments if USER_PAGES.match(path)]


def main() -> int:
    """Report each line that announces future work, and fail if there is one."""

    found = False

    for path in pages(sys.argv[1:]):
        with open(path, encoding="utf-8") as handle:
            for number, line in enumerate(handle, start=1):
                if FUTURE.search(line):
                    print(f"{path}:{number}: {line.strip()}")
                    found = True

    if found:
        print(
            "user pages describe what Muru does; work still to come belongs in the roadmap "
            "and the issue tracker, and a limit is stated as it is",
            file=sys.stderr,
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
