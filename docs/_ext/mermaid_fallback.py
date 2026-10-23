"""Show mermaid diagrams as source listings in builders that cannot draw them.

sphinxcontrib-mermaid renders diagrams in the browser for HTML output. For
other formats it shells out to the mermaid command line tool, which is not
installed where the PDF is built, and drops the diagram with a warning when
the tool is missing. This extension replaces each diagram with a literal
block of its mermaid source in every non-HTML builder, so the PDF keeps the
information and the build stays free of warnings.
"""

from __future__ import annotations

from docutils import nodes
from sphinx.application import Sphinx
from sphinxcontrib.mermaid import mermaid


def replace_diagrams(app: Sphinx, doctree: nodes.document, docname: str) -> None:
    """Replace mermaid nodes with literal blocks when the builder is not HTML."""

    if app.builder.format == "html":
        return
    for node in list(doctree.findall(mermaid)):
        code = node["code"]
        literal = nodes.literal_block(code, code)
        literal["language"] = "text"
        node.replace_self(literal)


def setup(app: Sphinx) -> dict[str, object]:
    """Register the event handler."""

    app.setup_extension("sphinxcontrib.mermaid")
    app.connect("doctree-resolved", replace_diagrams)
    return {"version": "1", "parallel_read_safe": True, "parallel_write_safe": True}
