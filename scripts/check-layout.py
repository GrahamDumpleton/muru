"""Check, or fix, the paragraph layout rules of AGENTS.md ("Code layout").

A compound statement longer than two lines has a blank line before it
unless it opens its block or follows a comment; the statement after a
block ends has a blank line before it; a block comment has blank lines
around it; a docstring is followed by a blank line. Python is read through
the tokenizer, Rust through a small scanner that follows braces and skips
strings and comments.

Usage: check-layout.py [--fix] FILE...

Without --fix the script lists each missing blank line and exits 1 when
there are any; with --fix it inserts them, which the formatters then keep.
"""

from __future__ import annotations

import io
import re
import sys
import tokenize
from dataclasses import dataclass

PYTHON_COMPOUND = {"if", "for", "while", "try", "with", "match", "async"}
PYTHON_CONTINUATION = {"else", "elif", "except", "finally", "case"}
RUST_CONTINUATION = ("else", "}", ")", "]", ".", "?")
PRAGMA = re.compile(r"^\s*#\s*(type:|noqa|pragma|ruff:|fmt:)")


@dataclass
class Problem:
    """A missing blank line before `line` (1-based)."""

    line: int
    message: str


def is_comment(text: str, rust: bool) -> bool:
    """Say whether a line is a comment line (a pragma is not)."""

    stripped = text.lstrip()
    if rust:
        return stripped.startswith("//")
    return stripped.startswith("#") and not PRAGMA.match(text)


def check_python(source: str) -> list[Problem]:
    """Apply the rules to a Python module, through its tokens."""

    lines = source.split("\n")
    problems: list[Problem] = []
    tokens = list(tokenize.generate_tokens(io.StringIO(source).readline))

    # Logical lines, each with its first token, its physical span, and what
    # came before it: an INDENT (first in its block) or DEDENTs (after a
    # block ended).

    logical: list[tuple[tokenize.TokenInfo, int, int, bool, bool]] = []
    first: tokenize.TokenInfo | None = None
    after_indent = False
    after_dedent = False

    for token in tokens:
        if token.type == tokenize.INDENT:
            after_indent = True
            continue

        if token.type == tokenize.DEDENT:
            after_dedent = True
            continue

        if token.type in (tokenize.NL, tokenize.COMMENT, tokenize.ENCODING):
            continue

        if token.type == tokenize.NEWLINE:
            if first is not None:
                logical.append((first, first.start[0], token.start[0], after_indent, after_dedent))
            first = None
            after_indent = after_dedent = False
            continue

        if first is None:
            first = token

    def compound_end(index: int) -> int:
        """Find the last physical line of the compound statement at `index`."""

        start_token, _, _, _, _ = logical[index]
        indent = start_token.start[1]
        end = logical[index][2]

        for later_token, _, later_end, _, _ in logical[index + 1 :]:
            if later_token.start[1] < indent:
                break
            if later_token.start[1] == indent and later_token.string not in PYTHON_CONTINUATION:
                break
            end = later_end

        return end

    def needs_blank(row: int, why: str) -> None:
        above = lines[row - 2] if row >= 2 else ""
        if above.strip() == "" or is_comment(above, rust=False):
            return
        problems.append(Problem(row, why))

    # The span of the compound statement that most recently opened a block
    # at each indent, so the statement after a block ends knows whether
    # that block was a short guard.

    spans: dict[int, int] = {}

    for index, (token, start, _end, first_in_block, after_block) in enumerate(logical):
        word = token.string
        indent = token.start[1]

        # A docstring opens a definition's body and is followed by a blank line.

        if first_in_block and token.type == tokenize.STRING and index + 1 < len(logical):
            following = logical[index + 1][1]
            if lines[_end].strip() != "" and following == _end + 1:
                problems.append(Problem(_end + 1, "no blank line after the docstring"))

        if after_block and word not in PYTHON_CONTINUATION and spans.get(indent, 3) > 2:
            needs_blank(start, "no blank line after the block above ends")

        if word in PYTHON_COMPOUND and not lines[start - 1].lstrip().startswith("async def"):
            span = compound_end(index) - start + 1
            spans[indent] = span
            if not first_in_block and span > 2:
                needs_blank(start, f"no blank line before the {word} block")

    problems.extend(comment_problems(lines, rust=False))
    return problems


RUST_COMPOUND = re.compile(
    r"^\s*(?:(?:if|for|while|loop|match|unsafe)\b|let\b[^=]*=\s*(?:match|if|unsafe|loop)\b|"
    r"[a-z_:]+!\s*\{)"
)


def rust_lines(source: str) -> list[tuple[str, bool]]:
    """Pair each line with whether it starts inside a string or block comment.

    No rule fires inside a literal.
    """

    result: list[tuple[str, bool]] = []
    in_string: str | None = None
    in_block_comment = 0

    for line in source.split("\n"):
        result.append((line, in_string is not None or in_block_comment > 0))
        index = 0

        while index < len(line):
            char = line[index]

            if in_string is not None:
                if in_string == '"' and char == "\\":
                    index += 2
                    continue

                if line.startswith(in_string, index):
                    index += len(in_string)
                    in_string = None
                    continue

                index += 1
                continue

            if in_block_comment:
                if line.startswith("*/", index):
                    in_block_comment -= 1
                    index += 2
                    continue

                if line.startswith("/*", index):
                    in_block_comment += 1
                    index += 2
                    continue

                index += 1
                continue

            if line.startswith("//", index):
                break

            if line.startswith("/*", index):
                in_block_comment += 1
                index += 2
                continue

            if char == '"':
                in_string = '"'
                index += 1
                continue

            raw = re.match(r'b?r(#*)"', line[index:])

            if raw:
                in_string = '"' + raw.group(1)
                index += len(raw.group(0))
                continue

            if char == "'":
                # A char literal or a lifetime; a char literal closes within a few bytes.

                closing = re.match(r"'(?:\\.|[^'\\])'", line[index:])
                index += len(closing.group(0)) if closing else 1
                continue

            index += 1

    return result


def rust_code(line: str) -> str:
    """Blank the strings, chars and comments of a line, for brace counting."""

    out = []
    index = 0

    while index < len(line):
        if line.startswith("//", index):
            break

        if line[index] == '"':
            end = index + 1
            while end < len(line) and line[end] != '"':
                end += 2 if line[end] == "\\" else 1
            index = end + 1
            continue

        if line[index] == "'":
            closing = re.match(r"'(?:\\.|[^'\\])'", line[index:])

            if closing:
                index += len(closing.group(0))
                continue

        out.append(line[index])
        index += 1

    return "".join(out)


def check_rust(source: str) -> list[Problem]:
    """Apply the rules to a Rust file, following braces."""

    annotated = rust_lines(source)
    lines = [line for line, _ in annotated]
    problems: list[Problem] = []

    # Every block: the line of its opening brace, the line of its closing
    # brace, and the indent of the line that opened it.

    stack: list[tuple[int, int]] = []
    blocks: dict[int, tuple[int, int]] = {}

    for number, (line, literal) in enumerate(annotated, start=1):
        if literal:
            continue
        code = rust_code(line)
        indent = len(line) - len(line.lstrip())

        for char in code:
            if char == "{":
                stack.append((number, indent))
            elif char == "}" and stack:
                opened, opened_indent = stack.pop()
                if opened != number:
                    blocks[number] = (opened, opened_indent)

    def needs_blank(row: int, why: str) -> None:
        above = lines[row - 2] if row >= 2 else ""
        if above.strip() == "" or is_comment(above, rust=True):
            return
        problems.append(Problem(row, why))

    def previous_code(number: int) -> str:
        for earlier in range(number - 2, -1, -1):
            if lines[earlier].strip() != "" and not is_comment(lines[earlier], rust=True):
                return lines[earlier]

        return ""

    def next_code(number: int) -> int:
        for later in range(number, len(lines)):
            if lines[later].strip() != "":
                return later + 1

        return 0

    for number, (line, literal) in enumerate(annotated, start=1):
        if literal or line.strip() == "":
            continue
        stripped = line.lstrip()
        indent = len(line) - len(stripped)

        # A compound statement longer than two lines, not first in its block.

        if RUST_COMPOUND.match(line) and not stripped.startswith("else"):
            closing = next(
                (close for close, (opened, _) in blocks.items() if opened == number), None
            )

            if closing is not None and closing - number + 1 > 2:
                before = previous_code(number)
                opens_block = before.rstrip().endswith(("{", "(", "[", ",", "=>", "="))
                if not opens_block:
                    needs_blank(number, "no blank line before the block")

        # The statement after a compound block closes at the same indent.

        if stripped.startswith("}") and number in blocks:
            opened, opened_indent = blocks[number]
            opener = lines[opened - 1].lstrip()
            compound = bool(RUST_COMPOUND.match(opener)) or opener.startswith("else")
            closes_statement = stripped.rstrip() in ("}", "};")
            following = next_code(number)

            if compound and closes_statement and following and number - opened + 1 > 2:
                after = lines[following - 1]
                after_indent = len(after) - len(after.lstrip())
                after_stripped = after.lstrip()

                if (
                    after_indent == indent
                    and not after_stripped.startswith(RUST_CONTINUATION)
                    and not is_comment(after, rust=True)
                    and following == number + 1
                ):
                    problems.append(Problem(following, "no blank line after the block above ends"))

    problems.extend(comment_problems(lines, rust=True))
    return problems


def comment_problems(lines: list[str], rust: bool) -> list[Problem]:
    """Check that block comments have blank lines below and, unless opening a block, above."""

    problems: list[Problem] = []
    opener = re.compile(r".*(\{|\(|\[|:|=>|\belse)\s*$")
    continuation = re.compile(r"^\s*(else\b|elif\b|except\b|finally\b|[)\]}])")

    for index, line in enumerate(lines):
        if not is_comment(line, rust) or line.lstrip().startswith(("///", "//!")):
            continue
        first = index == 0 or not is_comment(lines[index - 1], rust)
        end = index
        while end + 1 < len(lines) and is_comment(lines[end + 1], rust):
            end += 1
        after = lines[end + 1] if end + 1 < len(lines) else ""
        before = lines[index - 1] if index > 0 else ""

        if end == index and after.strip() and not is_comment(after, rust):
            annotates_one_line = (
                after.lstrip().startswith(".")
                or " => " in after
                or (before.rstrip().endswith(",") and after.rstrip().endswith((",", ")")))
            )
            if not continuation.match(after) and not annotates_one_line:
                problems.append(Problem(index + 2, "no blank line below the block comment"))

        if first and after.strip() == "" and before.strip():
            if not is_comment(before, rust) and not opener.match(before):
                problems.append(Problem(index + 1, "no blank line above the block comment"))

    return problems


def check(path: str) -> tuple[list[str], list[Problem]]:
    """Check one file, returning its lines and the problems found."""

    source = open(path, encoding="utf-8").read()
    rust = path.endswith(".rs")
    problems = check_rust(source) if rust else check_python(source)
    problems.sort(key=lambda problem: problem.line)
    return source.split("\n"), problems


def main() -> int:
    """Check or fix every path given."""

    arguments = sys.argv[1:]
    fix = "--fix" in arguments
    paths = [argument for argument in arguments if argument != "--fix"]
    total = 0

    for path in paths:
        lines, problems = check(path)
        if not problems:
            continue
        total += len(problems)

        if fix:
            # Insert from the bottom so earlier line numbers stay valid; one
            # blank line per flagged line, however many rules flagged it.

            for row in sorted({problem.line for problem in problems}, reverse=True):
                lines.insert(row - 1, "")
            open(path, "w", encoding="utf-8").write("\n".join(lines))
            print(f"{path}: {len(problems)} fixed")
        else:
            for problem in problems:
                print(f"{path}:{problem.line}: {problem.message}")

    return 1 if total and not fix else 0


if __name__ == "__main__":
    sys.exit(main())
