# Reference

Technical descriptions of the command line, configuration, APIs and
environment. Most of these pages are generated from the same definitions the
code uses (clap commands, the muru-config schema, the protocol definitions and
the Python docstrings), so they cannot drift from the implementation. Until
the code exists, the initial key lists live in the appendices of the
[design specification](../project/design/appendices/index.md).

Planned:

- CLI (generated from clap).
- Configuration (generated from the muru-config schema, with the JSON Schema download).
- Admin API (generated from the .proto files, with the JSON gateway paths).
- Python API (autodoc).
- WSGI environ keys and ASGI scope extensions.
- Environment variables.
- Event types.
- Metrics.
- Error statuses and reasons.
- Supported platforms and Python versions.

```{toctree}
:maxdepth: 1
```
