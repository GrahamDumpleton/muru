"""Check that text users see names no part of the design documents.

Messages, warnings, errors and help are written for users, who have no
reason to know the specification's sections, the implementation phases
or the decision records, and a reference to them goes stale. This lists
string literals in the Rust crates and the runtime library that name a
section, a phase or an ADR by number; comments and docstrings, which are
for the code's readers, are left alone. The help muructl renders from doc
comments is checked by a unit test in muructl, since clap assembles it.

Usage: check-user-text.py [FILE...]

With no files, every Rust source under crates/ and every module of the
runtime library is checked. Exits 1 when any string names one.
"""

from __future__ import annotations

import ast
import re
import subprocess
import sys

DESIGN = re.compile(r"\b(?:sections? [0-9]|phase [0-9]|ADR [0-9])", re.IGNORECASE)
RAW = re.compile(r'r(#*)"')
CHARACTER = re.compile(r"'(?:\\.|[^\\'])'")


def rust_strings(text: str) -> list[tuple[int, str]]:
    """Find the string literals of Rust source, with their offsets, skipping comments."""

    found = []
    index = 0

    while index < len(text):
        if text.startswith("//", index):
            end = text.find("\n", index)
            index = len(text) if end < 0 else end
            continue

        if text.startswith("/*", index):
            end = text.find("*/", index)
            index = len(text) if end < 0 else end + 2
            continue

        previous = text[index - 1] if index else " "

        if text[index] == "r" and not (previous.isalnum() or previous == "_"):
            raw = RAW.match(text, index)

            if raw:
                close = '"' + raw.group(1)
                end = text.find(close, raw.end())
                end = len(text) if end < 0 else end + len(close)
                found.append((index, text[index:end]))
                index = end
                continue

        if text[index] == "'":
            character = CHARACTER.match(text, index)
            index = character.end() if character else index + 1
            continue

        if text[index] == '"':
            end = index + 1

            while end < len(text) and text[end] != '"':
                end += 2 if text[end] == "\\" else 1

            found.append((index, text[index : end + 1]))
            index = end + 1
            continue

        index += 1

    return found


def python_strings(text: str) -> list[tuple[int, str]]:
    """Find the string constants of Python source that are not docstrings, with their lines."""

    tree = ast.parse(text)
    docstrings = set()

    for node in ast.walk(tree):
        if (
            isinstance(node, ast.Module | ast.FunctionDef | ast.AsyncFunctionDef | ast.ClassDef)
            and node.body
            and isinstance(node.body[0], ast.Expr)
        ):
            docstrings.add(id(node.body[0].value))

    return [
        (node.lineno, node.value)
        for node in ast.walk(tree)
        if isinstance(node, ast.Constant)
        and isinstance(node.value, str)
        and id(node) not in docstrings
    ]


def default_files() -> list[str]:
    """List the Rust sources and the runtime library's modules git knows."""

    listed = subprocess.run(
        ["git", "ls-files", "crates/*.rs", "packages/muru/python/muru/*.py"],
        capture_output=True,
        text=True,
        check=True,
    )
    return [name for name in listed.stdout.split() if "/tests/" not in name]


def main(arguments: list[str]) -> int:
    """Check the files named, or every one, and say which strings name the design."""

    files = [name for name in arguments if name.endswith((".rs", ".py"))] or default_files()
    problems = []

    for name in files:
        with open(name, encoding="utf-8") as source:
            text = source.read()

        if name.endswith(".rs"):
            strings = [(text.count("\n", 0, at) + 1, value) for at, value in rust_strings(text)]
        else:
            strings = python_strings(text)

        for line, value in strings:
            if DESIGN.search(value):
                problems.append(f"{name}:{line}: {' '.join(value.split())[:120]}")

    for problem in problems:
        print(problem)

    if problems:
        print("text users see names a design section, phase or ADR; describe it instead")

    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
