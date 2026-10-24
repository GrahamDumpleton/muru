# 1. Overview

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

Muru serves Python WSGI and ASGI applications without Apache httpd or nginx, and without the restart-to-deploy model of servers like gunicorn. A Rust edge accepts client connections and routes requests over a private link to Python worker processes, which a Rust controller manages as immutable releases that can run side by side.

## 1.1 Problem

Python web applications are usually deployed in one of two ways. A general web server (Apache with mod_wsgi, or nginx in front of an application server) gives good routing but no notion of application releases. An application server with a built-in HTTP listener (gunicorn, uvicorn) is simple, but code changes mean restarting the server, and features such as canary releases, drain control or per-path process models need extra infrastructure. Muru combines a modern edge, a release-aware control plane and a Python runtime that understands preload, forking, threads and the GIL.

## 1.2 Goals

- Zero-downtime deploys: a new release, possibly on a different Python version, starts beside the old one, and traffic moves when it is ready.
- Draining, blue/green, weighted canary, A/B testing with deterministic bucketing, sticky sessions, preview releases and instant rollback.
- Process and thread models comparable to mod_wsgi daemon mode and gunicorn, including preload before fork with post-fork hooks and worker groups per path.
- WSGI and ASGI in one process: mounts by path, and requests handed across in code, so a WSGI view can hand a request to an ASGI application on the event loop, which gives WSGI applications WebSockets, and an ASGI application can hand one to a WSGI callable on the thread pool (sections 6.7 and 6.14).
- Python-aware elastic scaling, memory policies and request timeouts.
- Live reconfiguration through an API and CLI, with declarative files that export and apply round-trip.
- One command for development, the same model up to a large monolith in production.
- Strong observability: metrics, structured logs, an event stream, OpenTelemetry integration and a built-in dashboard.

## 1.3 Non-goals for the first versions

- Multi-node scheduling and orchestration. Interfaces are designed so nodes can be added later.
- General management of non-Python servers. Unmanaged HTTP upstreams are supported, and managed HTTP server releases exist in a limited form for migration cutovers (section 9.9).
- Windows. Production support is Linux. macOS is supported for development.
- Acting as a CDN, WAF or API gateway product.

## 1.4 Glossary

| Term | Meaning |
| --- | --- |
| Edge | Rust process that accepts client connections, terminates TLS and routes requests |
| Daemon | The muructl daemon process: controller, supervisor, log collector and admin API |
| Controller | The part of the daemon that holds desired state and reconciles it |
| Application | A named Python application, such as shop |
| Release | An immutable deployable unit: code (directory or image), interpreter, entrypoints and settings, such as shop@r42 |
| Host process | The Python process for a release that preloads the application and forks workers |
| Worker | A forked Python process that serves requests through the runtime library |
| Mount | A path within a release served by one interface (WSGI, ASGI or gRPC) and entrypoint |
| Worker group | A set of workers with its own process model and scaling that serves a subset of paths |
| Virtual host | A named host object with names, listeners, TLS and routes |
| Route | A host and path match mapped to a target: an application, static directory, upstream or redirect |
| Traffic policy | Weights, stickiness and overrides deciding which release serves a route |
| Edge pool | A group of edge processes owning a set of listeners |
| Runtime library | The muru Python package, with its Rust extension, that runs inside hosts and workers |
| Snapshot | A complete, versioned routing table pushed from controller to edge |

## 1.5 How to read this spec

Sections 2 and 3 describe the overall structure and the objects everything else refers to. Sections 4 to 13 specify behaviour by subsystem. Sections 14 to 18 cover how the project is built, tested, released and documented. Section 19 lists decisions and open questions, and the appendices are reference tables. Configuration examples use TOML. Normative words are used informally: "must" marks behaviour an implementation is required to have, "should" marks a strong default.
