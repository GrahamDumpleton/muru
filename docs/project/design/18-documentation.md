# 18. Documentation

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

User documentation is a Sphinx site hosted on Read the Docs, organised by the Diátaxis model (tutorials, how-to guides, reference, explanation). Reference pages are generated from the same definitions the code uses, so they cannot drift from the implementation.

## 18.1 Tooling

- Sphinx with MyST Markdown, the Furo theme, sphinxcontrib-mermaid for diagrams, sphinx-copybutton, sphinx-design, sphinx.ext.autodoc and intersphinx (Python standard library, Django, Flask, Starlette).
- .readthedocs.yaml builds with uv-installed requirements on Ubuntu with a pinned Python, with versions latest (main), stable (newest release) and one per release tag, and pull request previews.
- CI builds the docs with warnings as errors and runs a link check. Code samples marked as tests run in CI.
- Rust API documentation is internal and not published.

## 18.2 Site structure

| Part | Pages |
| --- | --- |
| Tutorials | Install muructl. Run your first WSGI app. Run an ASGI app with WebSockets. Deploy with the daemon and roll back. Your first canary |
| How-to guides | Django, Flask, FastAPI and Starlette. Migrating from mod_wsgi, gunicorn and uvicorn. Running under systemd. Virtual hosts and TLS. Behind a load balancer or CDN (trusted proxies). Worker groups and mounts. Uploads and authorised downloads (internal redirects), including recipes for Django, Flask and Starlette upload adapters. Custom error pages and maintenance mode. Scaling, schedules and memory limits. Timeouts. Logging and log rotation. Containers and images. uv and Python versions. Event webhooks and OpenTelemetry. Configuration in version control (export and apply). Database migrations during overlapping releases. Upgrading Muru |
| Reference | CLI (generated from clap). Configuration (generated from the muru-config schema, with the JSON Schema download). Admin API (generated from the .proto files, with the JSON gateway paths). Python API (autodoc). WSGI environ keys and ASGI scope extensions. Environment variables. Event types. Metrics. Error statuses and reasons. Supported platforms and Python versions |
| Explanation | Architecture. Releases and traffic policies. The worker protocol. Preload and fork safety. Process models, threads and the GIL. How scaling decides. Security model. Comparison with mod_wsgi, gunicorn, uvicorn and Granian |
| Project | Changelog. Contributing (building, testing, docs). Release process. Security policy. About the project and its creator, including the statement on AI use (section 18.6). Design specification (one page per major section). Architecture decision records. Roadmap. This part appears last in the navigation and is aimed at contributors (section 18.5) |

## 18.3 Generated references

| Reference | Source | Generator |
| --- | --- | --- |
| CLI | clap definitions in crates/muructl | a build step writes MyST pages and man pages |
| Configuration | serde and schemars types in muru-config | JSON Schema, then MyST tables with defaults and descriptions |
| Admin API | proto/muru/admin/v1 | protoc-gen-doc style output, plus gateway paths from annotations |
| Python API | python/muru with docstrings and stubs | sphinx.ext.autodoc |
| Events and metrics | registries in muru-controller and muru-edge | a build step lists names, labels and descriptions |

Generated pages are rebuilt in CI, and a check fails if the committed copies are stale, so reviews show reference changes alongside code changes.

## 18.4 Repository documents

README (overview, install, five-minute start), CONTRIBUTING.md (prerequisites, just doctor, just bootstrap, workflow), AGENTS.md and CLAUDE.md (instructions for AI coding agents, section 15.6), SECURITY.md, CODE_OF_CONDUCT.md, issue forms under .github/ISSUE_TEMPLATE (section 18.6), CHANGELOG.md (built from towncrier fragments), LICENSE (Apache-2.0), and the design specification and decision records under docs/project (section 18.5).

## 18.5 Design specification and decision records

The design specification and the architecture decision records (ADRs) are published on Read the Docs with the rest of the documentation, in the contributor-facing Project part of the site. They are searchable, versioned with each release and linkable from issues and pull requests, while staying out of the way of users reading tutorials and guides.

```
docs/project/
  design/
    index.md                status, version, how the specification is maintained
    01-overview.md
    02-architecture.md
    ...                     one page per major section of this specification
    appendices/
  adr/
    index.md                generated list with number, title, status and date
    template.md             MADR template
    0001-grpc-for-worker-link.md
    0002-separate-edge-processes.md
    ...
```

**The specification as a living document.** The copy in the repository is the source of truth for intended design and changes through pull requests like code; its history is the git history, so it carries no version number of its own. It is one page per major section, with the appendices under their own directory. docs/project/design/index.md explains how it is maintained. Each design page carries a short banner saying it describes intended design for contributors, and that the user documentation is authoritative for released behaviour.

**Decision records.** ADRs use the MADR format, are numbered sequentially, and each has a status (proposed, accepted, deprecated, or superseded by a later ADR) and a date. They are short: context, decision, alternatives considered, consequences. The initial set is seeded from the decisions in section 19.1, so the reasoning behind choices such as the gRPC worker link, separate edge processes, WSGI first, version-specific wheels and the htmx dashboard is recorded from the start. When implementation shows that the specification needs to change, the change is made in the same pull request as a new ADR explaining why, and the ADR index is regenerated by the docs build.

**For contributors and agents.** CONTRIBUTING.md and AGENTS.md point to docs/project/design and docs/project/adr, tell agents to read the relevant specification pages before working on a subsystem, and to propose an ADR rather than silently deviate from the specification.

## 18.6 Contribution model, issue templates and project background

**Contribution model.** Pull requests are limited to collaborators, using GitHub's repository setting that restricts pull request creation to collaborators. If that setting is unavailable for the repository, a workflow closes pull requests from non-collaborators with a polite comment pointing to the issue process. Everyone else contributes by describing requirements, not by sending code:

1. Search existing issues and discussions first.
2. Open a feature request describing the problem, the use case and the desired behaviour. Implementation suggestions are welcome but not required, and a requirement is more useful than a patch.
3. The maintainer discusses it in the issue, decides whether and how it fits the design, and records significant design decisions as ADRs (section 18.5).
4. Accepted work is implemented by the maintainer, with AI assistance, and the person who raised it is credited in the changelog and asked to test it.

Bug reports follow the same path. Questions and open-ended ideas go to GitHub Discussions, and security issues go through private security advisories (SECURITY.md). CONTRIBUTING.md explains all of this at the top, so nobody spends time on an unexpected pull request.

**Issue templates.** GitHub issue forms in .github/ISSUE_TEMPLATE/:

| Template | Fields |
| --- | --- |
| bug_report.yml | Summary; Muru version (muructl --version); Python version and build (regular or free-threaded); operating system and architecture; how Muru runs (muructl run, user daemon, system daemon, container release); interface (WSGI or ASGI) and framework; configuration excerpt with secrets removed; steps to reproduce; expected and actual behaviour; diagnostics from muructl debug report; confirmation that existing issues were searched |
| feature_request.yml | Problem or use case; desired behaviour stated as a requirement; current workaround; alternatives considered; who is affected and at what scale; related specification section if known; willingness to test |
| documentation.yml | Page or URL; what is wrong, missing or unclear; suggested correction |
| config.yml | Blank issues disabled. Contact links to GitHub Discussions for questions, the security advisory form, and the documentation |

To make bug reports useful, muructl debug report produces a redacted diagnostic bundle: versions, platform and resolved paths, effective configuration with secrets removed, release and worker states, recent events and recent log lines. It never includes request bodies, secret values or environment variable values.

**Statement on AI use.** The documentation home page, README and an About page state plainly that Muru is developed with extensive use of AI coding agents, directed and reviewed by its creator, under the process in this specification: a written design, decision records, comprehensive tests and CI, and review of everything that is merged. It also says, without apology, that people who prefer not to use or depend on AI-assisted software may find the project is not for them.

**About the creator.** An About page in the Project part of the site introduces the person driving the project and the experience behind it, so readers can judge it as a considered piece of engineering rather than a casual experiment. Draft outline, with details for Graham to confirm and complete:

- Graham Dumpleton, based in Sydney, Australia.
- Author and long-time maintainer of mod_wsgi, the Apache module for hosting Python WSGI applications, first released in 2007 and still widely used, including mod_wsgi-express. Much of Muru's design (daemon process groups, request timeouts, preloading, vertical partitioning, the trade-offs of process and thread models) comes directly from lessons learned maintaining it.
- Author of wrapt, the widely used Python library for decorators, wrappers and monkey patching, which underpins many observability and instrumentation tools.
- Decades of experience with Python web hosting, deployment and operations. Career details and other projects to be added by Graham.
- Why Muru exists, in his words, and how he uses AI to build it.

The site structure in section 18.2 gains an About page (with the AI statement) in the Project part, and the README links to it.
