# 12. Security model

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

The main assets are the applications' data and the node itself. The main threats are hostile clients, a compromised application trying to reach other applications or the daemon, and mistakes in configuration. Controls are layered so that no single component is trusted for everything.

| Concern | Control |
| --- | --- |
| Privilege | Only the daemon may run as root, to bind low ports, create cgroups and switch to per-application users. It parses no client traffic and drops capabilities it no longer needs. Edges run unprivileged with listening sockets passed in, as the service user (ADR 0026), which is also what an application that names no user runs as; a root daemon never runs either as root unless an application says `user = 'root'`, and refuses to start without the service user's account. Each application runs as its own user or in its own container |
| Hostile requests | Strict HTTP parsing in hyper, header and body limits, timeouts, slow-client isolation at the edge, per-pool admission control, 421 for unknown hosts and SNI mismatches |
| Request smuggling | Typed protocol between edge and workers. For HTTP upstreams, strict normalisation and re-framing (section 4.13) |
| Header spoofing | Forwarding headers only from configured proxies and types, underscore header names dropped, client X-Muru-* headers stripped, internal headers accepted only on internal requests |
| Cross-application access | Internal routes and redirect targets require explicit accept_redirects_from. Per-release socket directories owned by the application user with a group shared only with the edge. SO_PEERCRED checks on control and worker sockets |
| File access | Descriptor passing so the edge never needs application file access. Path hints restricted to sendfile_roots. Upload files unnamed until the application keeps them |
| Resource exhaustion | Node budgets, cgroup or container limits, spool quotas, max_body_size, churn budgets, rate-limited recycling |
| Secrets | Passed as environment variables or files with restrictive permissions, referenced rather than stored in exports, never in snapshots, events or logs |
| Admin access | Socket permissions, mTLS or tokens for TCP, roles, idempotency, optimistic concurrency, audit log. The dashboard needs a session from the admin socket for every page, may act only for a session asked for with the actions, is CSRF-protected, not on public listeners |
| Supply chain | Locked dependencies, cargo-deny and cargo-audit, pip-audit for development dependencies, trusted publishing to PyPI with attestations, signed container images (section 17) |
| Upgrades | Protocol versioning with a compatibility window, reversible state schema changes, binary upgrades without dropping traffic |

Security reports are handled through a SECURITY.md policy with a private reporting channel (GitHub security advisories).
