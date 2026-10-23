"""Sphinx configuration for the Muru documentation."""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Local extensions and the runtime library, for autodoc.
sys.path.insert(0, str(Path(__file__).resolve().parent / "_ext"))
sys.path.insert(0, str(ROOT / "packages" / "muru" / "python"))

project = "Muru"
author = "Graham Dumpleton"
copyright = "2026, Graham Dumpleton"
# The project version lives in Cargo.toml and nowhere else.
with (ROOT / "Cargo.toml").open("rb") as cargo:
    release = tomllib.load(cargo)["workspace"]["package"]["version"]
version = release

extensions = [
    "myst_parser",
    "sphinx.ext.autodoc",
    "sphinx.ext.intersphinx",
    "sphinxcontrib.mermaid",
    "mermaid_fallback",
    "sphinx_copybutton",
    "sphinx_design",
    "adr_index",
]

source_suffix = {".md": "markdown"}
master_doc = "index"
exclude_patterns = ["_build", "Thumbs.db", ".DS_Store", "project/adr/template.md"]

# MyST: mermaid fences render as diagrams, and headings get anchors for cross references.
myst_enable_extensions = ["colon_fence", "deflist", "fieldlist", "tasklist", "attrs_inline"]
myst_fence_as_directive = ["mermaid"]
myst_heading_anchors = 3

intersphinx_mapping = {
    "python": ("https://docs.python.org/3", None),
}

html_theme = "furo"
html_title = "Muru"
html_static_path = ["_static"]
html_theme_options = {
    "source_repository": "https://github.com/GrahamDumpleton/muru/",
    "source_branch": "main",
    "source_directory": "docs/",
}

autodoc_typehints = "description"
copybutton_prompt_text = r">>> |\.\.\. |\$ "
copybutton_prompt_is_regexp = True

# PDF output, built on Read the Docs from the LaTeX builder. Mermaid diagrams
# appear as source listings there (see docs/_ext/mermaid_fallback.py).
latex_documents = [("index", "muru.tex", "Muru", author, "manual")]
latex_show_urls = "footnote"
latex_elements = {"papersize": "a4paper"}
