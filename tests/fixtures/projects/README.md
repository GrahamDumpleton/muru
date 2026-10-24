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

Run one directly with `muructl run tests/fixtures/projects/shop`.
