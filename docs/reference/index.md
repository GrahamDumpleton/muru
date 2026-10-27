# Reference

Technical descriptions of the command line, configuration, APIs and
environment. Most of these pages are generated from the same definitions the
code uses (clap commands, the muru-config schema, the protocol definitions and
the Python docstrings), so they cannot drift from the implementation. Until
those pages are generated, the key lists live in the appendices of the
[design specification](../project/design/appendices/index.md).

Still to be written, as their generators are built:

- CLI (generated from clap); until then `muructl --help` and each
  command's `--help` are complete.
- Configuration (generated from the muru-config schema, with the JSON Schema download).
- Admin API (generated from the .proto files, with the JSON gateway paths).
- Environment variables.
- Event types.
- Metrics.
- Supported platforms and Python versions.

```{toctree}
:maxdepth: 1

wsgi-environ
asgi-scope
errors
python-api
```
