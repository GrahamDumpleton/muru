# Security policy

## Reporting a vulnerability

Please do not report security problems through public issues, discussions or
pull requests.

Report them privately through GitHub security advisories for this repository
(the "Report a vulnerability" button under the Security tab). Include the
Muru version, how Muru is run (development mode, user daemon, system daemon or
container releases), the platform, and steps to reproduce.

You should receive an acknowledgement within seven days. Confirmed problems
are fixed in a new release, with credit to the reporter unless they prefer
otherwise, and announced through a security advisory.

## Supported versions

Until 1.0, only the newest release receives security fixes.

## Scope

The security model is described in the design specification under
[docs/project/design](docs/project/design/index.md), section 12. Reports
about behaviour that the specification describes as a known limitation (for
example, that Muru cannot make two overlapping releases safe against shared
state) are welcome as discussions rather than advisories.
