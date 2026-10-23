# Release process

Two distributions are published to PyPI from one tag, with the same version
number: muructl carries the binary and muru is the runtime library. Releases
are built and published only by GitHub Actions using PyPI trusted publishing,
with attestations, so no long-lived tokens exist. The design is in
[section 17](design/17-packaging-and-release.md) of the specification.

## Checklist

1. Release pull request from `develop` to `main`: version bump with
   `just bump X.Y.Z` (Cargo.toml is the only place the version is stated),
   changelog built from the towncrier fragments, documentation updated.

2. Merge, then tag `vX.Y.Z` on `main`.

3. Watch the release workflow and approve the `pypi` environment after the
   TestPyPI verification.

4. Check the GitHub Release, the images and the documentation version.

5. Announce, including protocol and configuration compatibility notes.

## Versioning

Semantic versioning, shared by muructl and muru. Before 1.0, minor versions
may contain breaking changes, which are listed in the changelog. The worker
protocol and admin API carry their own versions and compatibility windows.
Configuration files carry a schema version, and older schema versions are
accepted with deprecation warnings for at least one minor release.
