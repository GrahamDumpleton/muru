"""Sphinx extension that lists architecture decision records in a table.

The ``adr-list`` directive scans the directory of the document that uses it
for files named ``NNNN-slug.md``, reads the ``status`` and ``date`` fields
from their YAML front matter and the title from their first heading, and
renders a Markdown table linking to each record.
"""

from __future__ import annotations

import re
from pathlib import Path
from typing import Any

from docutils import nodes
from sphinx.application import Sphinx
from sphinx.util.docutils import SphinxDirective

FRONT_MATTER = re.compile(r"\A---\n(.*?)\n---\n", re.DOTALL)
TITLE = re.compile(r"^# (?:ADR \d{4}: )?(.+)$", re.MULTILINE)
RECORD = "[0-9][0-9][0-9][0-9]-*.md"


def read_record(path: Path) -> dict[str, str]:
    """Return the number, title, status and date of one decision record."""

    text = path.read_text(encoding="utf-8")

    meta: dict[str, str] = {}
    match = FRONT_MATTER.match(text)

    if match:
        for line in match.group(1).splitlines():
            key, _, value = line.partition(":")
            meta[key.strip()] = value.strip().strip("'\"")

    title_match = TITLE.search(text)
    title = title_match.group(1).strip() if title_match else path.stem

    return {
        "number": path.stem[:4],
        "title": title,
        "status": meta.get("status", "unknown"),
        "date": meta.get("date", "unknown"),
        "file": path.name,
    }


class AdrList(SphinxDirective):
    """Render the table of decision records found beside the current document."""

    has_content = False

    def run(self) -> list[nodes.Node]:
        """Build the table and parse it as MyST so links resolve normally."""

        directory = Path(self.env.doc2path(self.env.docname)).parent
        records = [read_record(path) for path in sorted(directory.glob(RECORD))]
        for record in records:
            self.env.note_dependency(str(directory / record["file"]))

        if not records:
            return self.parse_text_to_nodes("No decision records yet.")

        lines = ["| Number | Title | Status | Date |", "| --- | --- | --- | --- |"]

        for record in records:
            lines.append(
                f"| {record['number']} | [{record['title']}]({record['file']}) "
                f"| {record['status']} | {record['date']} |"
            )

        return self.parse_text_to_nodes("\n".join(lines))


def setup(app: Sphinx) -> dict[str, Any]:
    """Register the directive with Sphinx."""

    app.add_directive("adr-list", AdrList)
    return {"version": "0.1", "parallel_read_safe": True, "parallel_write_safe": True}
