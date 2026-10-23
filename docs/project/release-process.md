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

2. Merge, then tag `vX.Y.Z` on `main` (`git tag vX.Y.Z && git push origin
   vX.Y.Z`). The workflow refuses a tag that does not match Cargo.toml.

3. Watch the release workflow: it builds wheels and sdists for every
   platform and interpreter, installs them in clean environments and checks
   the version, then waits. Approve the `pypi` environment to publish to
   PyPI; the GitHub Release with notes from the changelog and checksums
   follows.

4. Check the GitHub Release, the images and the documentation version.

5. Announce, including protocol and configuration compatibility notes.

## Publishing setup

Releases are published only by the release workflow, using PyPI trusted
publishing (OpenID Connect), so no API tokens exist anywhere. The one-time
setup, for each of the two distributions, is to register the workflow as a
publisher on PyPI: project name `muru` (then `muructl`), owner
`GrahamDumpleton`, repository `muru`, workflow `release.yml`, environment
`pypi`. Before the first release this is done as a "pending publisher",
which creates the project on first publish. PyPI may refuse a second
pending publisher for the same workflow while the first is still pending;
in that case publish the first distribution, register the second project's
publisher (now that the first is a real publisher), and re-run the failed
publish job. The publish step skips files that already exist on PyPI, so
the re-run uploads only what is missing. The GitHub side is the `pypi`
environment, limited to release tags and requiring a maintainer's approval.

TestPyPI is not used: the workflow installs every built wheel in a clean
environment before publishing, and an upload-level problem on a pre-release
costs only a version number.

## Pre-releases

Until the first usable version, every release is a pre-release of the 0.1.0
line: `0.1.0-alpha.1` in Cargo.toml and the tag (`v0.1.0-alpha.1`), which
is `0.1.0a1` on PyPI (maturin converts the version to PEP 440 form, and the
release workflow checks the result). pip installs pre-releases only with
`--pre` or an explicit version, while uv installs them by default when no
final release exists, so nobody gets a non-functional package by accident.
The first final release, `0.1.0`, is the first version that serves requests.

## Versioning

Semantic versioning, shared by muructl and muru. Before 1.0, minor versions
may contain breaking changes, which are listed in the changelog. The worker
protocol and admin API carry their own versions and compatibility windows.
Configuration files carry a schema version, and older schema versions are
accepted with deprecation warnings for at least one minor release.
