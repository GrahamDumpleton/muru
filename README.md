# Muru

[![CI](https://github.com/muruhq/muru/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/muruhq/muru/actions/workflows/ci.yml)
[![Documentation](https://img.shields.io/readthedocs/muru?label=docs)](https://docs.muruhq.dev/)
[![PyPI muructl](https://img.shields.io/pypi/v/muructl?label=muructl)](https://pypi.org/project/muructl/)
[![PyPI muru](https://img.shields.io/pypi/v/muru?label=muru)](https://pypi.org/project/muru/)
[![Python versions](https://img.shields.io/pypi/pyversions/muru)](https://pypi.org/project/muru/)
[![Licence](https://img.shields.io/github/license/muruhq/muru)](https://github.com/muruhq/muru/blob/main/LICENSE)

Muru is a routing edge, control plane and Python runtime for serving WSGI and
ASGI applications, with zero-downtime releases, traffic management and
Python-aware process management.

A Rust edge accepts client connections and routes requests over a private link to
Python worker processes, which a Rust controller manages as immutable
releases that can run side by side. A new release, possibly on a different
Python version, starts beside the old one, and traffic moves when it is
ready. Blue/green, weighted canaries, A/B splits, previews and instant
rollback are policy changes, not restarts. The same model runs from
`muructl run` on a laptop, to `muructl daemon start` on a server, to
`muructl serve` in a container.

- **Deploy without downtime:** releases side by side, blue/green,
  canaries judged by gates, previews and instant rollback.
- **Process management that understands Python:** preload with an import
  check, worker groups, memory limits, soft and hard timeouts, scaling,
  and free-threaded Python.
- **No web server in front:** HTTP/1.1, HTTP/2, TLS, virtual hosts,
  static files, uploads and compression at the edge.
- **WSGI and ASGI together,** mounted side by side or handed across in
  code.
- **Made to work with AI coding agents:** an MCP server lets an agent
  deploy each change as a release, test it, read what Muru observes when
  something fails, and roll back.

```
uv tool install muructl
muructl run app:application      # http://127.0.0.1:8080
```

Muru is in alpha: the 0.1.0 pre-releases are on PyPI, and commands and
configuration may change between them.

**[Read the documentation](https://docs.muruhq.dev/)** for the tutorials,
a guide to each feature, and how Muru works.

**[Try the workshops](https://github.com/muruhq/muru-workshops)**: guided
JupyterLab sessions that run Muru in the browser, on Binder or in GitHub
Codespaces, with nothing to install. They exist to exercise Muru, and
are also a quick way to see what it does.

## Contributing

Muru takes contributions as requirements, not as code: open an issue
describing the problem and the desired behaviour, and the maintainer
implements accepted work. See [CONTRIBUTING.md](CONTRIBUTING.md) for the
process and the development setup (`just doctor`, `just bootstrap`).

Muru is developed with extensive use of AI coding agents;
[About](https://docs.muruhq.dev/en/latest/project/about.html) says how,
and who is behind it.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
