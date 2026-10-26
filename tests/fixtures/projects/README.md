# Fixture projects

Whole projects, as opposed to the single applications in `../apps`, for the
tests of how `muructl run` finds an entrypoint (design specification
sections 6.8 and 13.1):

- `shop/`: a package with a `[tool.muru]` table in its pyproject.toml naming
  the entrypoint, the hooks module and the process and thread counts.
- `factory/`: a muru.toml at the top level with a factory entrypoint, plus
  ASGI callables for the phase 1 refusal.
- `grouped/`: a muru.toml declaring worker groups (section 7.1), whose
  application reports the group and worker that served each request.
- `mounted/`: a muru.toml with a WSGI default entrypoint and ASGI and WSGI
  mounts (section 6.7), each reporting the paths it saw.
- `legacy/`: a stdlib HTTP server, a WSGI and an ASGI application for the
  managed HTTP server releases of section 9.9.
- `files/`: a module whose WSGI default, ASGI, compatibility and Starlette
  mounts hand files to the edge (section 4.9); the tests write its
  muru.toml, since `sendfile_roots` names a directory they create.
- `uploads/`: a WSGI module and an ASGI callable that report the files the
  edge stored for upload routes (section 4.11), with the Flask, Starlette
  and Django recipes from the uploads guide as mounts; the tests write its
  muru.toml.
- `clients/`: a WSGI module and an ASGI callable that report the client
  class, site mode and method they were given (sections 4.14 and 4.15),
  and the release that served them, for the overlay tests (section 9.10);
  the tests write its muru.toml.

Run one directly with `muructl run tests/fixtures/projects/shop`.
