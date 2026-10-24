"""Report block comments not separated from the code around them by blank lines.

AGENTS.md asks for a blank line between a block comment and the code below
it, and for such a comment to open a paragraph rather than sit against the
statements above. Comments inside an argument list, a method chain or a
match arm annotate one line and are left alone, as are pragma lines.

Usage: check-comment-spacing.py FILE...   (exit 1 when anything is reported)
"""

import re
import sys

PY = re.compile(r"^\s*#(?!!)")
RS = re.compile(r"^\s*//(?![/!])")
PRAGMA = re.compile(r"^\s*#\s*(type:|noqa|pragma|ruff:|fmt:)")
OPENER = re.compile(r".*(\{|\(|\[|:|=>|\belse)\s*$")
CONTINUATION = re.compile(r"^\s*(else\b|elif\b|except\b|finally\b|[)\]}]|\)|\),|\},|\};|\]\))")
CLOSERS = (")", "]", "}", "},", "),", "};")


def check(path: str) -> list[str]:
    """Return one message per misplaced comment block in the file."""

    comment = PY if path.endswith(".py") else RS
    lines = open(path, encoding="utf-8").read().split("\n")
    problems: list[str] = []

    for index, line in enumerate(lines):
        if not comment.match(line) or PRAGMA.match(line):
            continue

        # Find the extent of this comment block and what surrounds it.

        if index > 0 and comment.match(lines[index - 1]):
            first = False
        else:
            first = True
        end = index
        while end + 1 < len(lines) and comment.match(lines[end + 1]):
            end += 1
        after = lines[end + 1] if end + 1 < len(lines) else ""
        before = lines[index - 1] if index > 0 else ""

        # A comment followed by code with no blank line is reported from its
        # last line; a comment stuck to the code above is reported from its
        # first line, but only when a blank line below shows it is a block.

        if end == index and after.strip() and not comment.match(after):
            annotates_one_line = (
                after.lstrip().startswith(".")
                or " => " in after
                or (before.rstrip().endswith(",") and after.rstrip().endswith((",", ")")))
            )
            if (
                not CONTINUATION.match(after)
                and after.strip() not in CLOSERS
                and not annotates_one_line
            ):
                problems.append(f"{path}:{index + 1}: no blank line below block comment")
        if first and after.strip() == "" and before.strip():
            if not comment.match(before) and not OPENER.match(before):
                problems.append(f"{path}:{index + 1}: no blank line above block comment")

    return problems


def main() -> int:
    """Check every path given and print what is wrong."""

    problems = [problem for path in sys.argv[1:] for problem in check(path)]
    for problem in problems:
        print(problem)

    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
