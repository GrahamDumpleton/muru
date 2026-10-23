# 2. Architecture

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

Muru has three layers. The edge carries traffic, the daemon decides what should run and keeps it running, and the Python runtime executes application code. The daemon is never on the request path, so traffic keeps flowing while it restarts or upgrades.

## 2.1 Components

```mermaid
flowchart LR
  CL[Clients] -->|HTTP and TLS| EDGE[Edge pool]
  OPS[muructl, dashboard, automation] -->|admin API| D
  subgraph CP [Daemon]
    D[Controller and supervisor]
    ST[(State store)]
    LC[Log collector]
    EV[Event stream]
    D --- ST
    D --- LC
    D --- EV
  end
  D -->|routing snapshots| EDGE
  D -->|spawn, control| H1
  D -->|spawn, control| H2
  subgraph R1 [Release shop r41, Python 3.12]
    H1[Host] -. fork .-> W1[Worker]
    H1 -. fork .-> W2[Worker]
  end
  subgraph R2 [Release shop r42, Python 3.13, in a container]
    H2[Host] -. fork .-> W3[Worker]
    H2 -. fork .-> W4[Worker]
  end
  EDGE -->|gRPC over Unix sockets| W1
  EDGE --> W2
  EDGE --> W3
  EDGE --> W4
  EDGE -->|HTTP| UP[Unmanaged upstreams]
```

| Component | Implementation | Responsibility | Effect of a restart |
| --- | --- | --- | --- |
| Edge (process title muru-edge) | muructl edge run, Rust, tokio, hyper, rustls | Listeners, TLS, virtual hosts, routing, load balancing, limits, buffering, static files, internal redirects, uploads, error pages | Replaced with listener handoff, no refused connections |
| Daemon (process title murud) | muructl daemon run, Rust | Desired state, reconciler, supervision, admin API, dashboard, log collection, events, schedules | None on traffic. Hosts are re-adopted |
| Host process | python -m muru.host, Python with the runtime library | Preload the application, fork and supervise workers, act as PID 1 in containers | Affects only its release |
| Worker | Forked from the host | Serve requests through the WSGI, ASGI and gRPC bridges | One worker, replaced from the host |
| muructl client | Same binary | CLI over the admin API, development mode | None |

## 2.2 Data path and control path

- **Data path:** client, edge, worker, application. Requests travel as typed gRPC streams between edge and worker. No HTTP is re-serialised on this hop.
- **Control path:** operator or automation, admin API, controller, then routing snapshots to the edge and lifecycle commands to hosts.
- **Side channels:** per-worker Unix socket channels pass file descriptors (file hints, uploads), and per-worker pipes carry stdout and stderr to the log collector.

## 2.3 Process topology

```mermaid
flowchart TB
  SYS[systemd or container runtime] --> D[muructl daemon run]
  D -->|spawns, same executable| E1[Edge process 1]
  D -->|spawns, same executable| E2[Edge process 2]
  D == listener sockets via SCM_RIGHTS ==> E1
  D == listener sockets via SCM_RIGHTS ==> E2
  D -->|spawns as application user, or via container API| H[Host per release]
  H -->|fork| W[Workers]
  E1 -- gRPC per worker socket --- W
  E2 -- gRPC per worker socket --- W
  E1 --- SHM[(Shared load table)]
  E2 --- SHM
```

- The daemon creates listening sockets and passes them to edge processes, so the edge can run unprivileged and be replaced without refusing connections.
- The daemon spawns edges from its own executable image (/proc/self/exe), so edge and controller always run the same version until an explicit upgrade.
- Hosts are spawned under per-application users, or inside containers through the Docker or Podman API.
- Each worker listens on its own Unix socket. The edge balances per request using exact in-flight counts held in a shared memory table when there are several edge processes.

## 2.4 Failure domains

| Failure | Effect | Recovery |
| --- | --- | --- |
| Daemon crash | Traffic unaffected. No deploys or scaling until it returns | Restarted by systemd, re-adopts hosts through the run directory and tokens |
| Edge crash | That edge's connections drop. Other edge processes continue | Daemon respawns it with the same listener sockets and last snapshot |
| Host crash | Its workers are fenced off and drained | Release restarted or marked failed |
| Worker crash | That worker's in-flight requests fail | Host forks a replacement with backoff. Edge retries only provably safe cases |
| Worker hang | Capacity loss | Soft and hard timeouts, stack capture, replacement |
| Node out of memory | Workers killed by cgroup or kernel | Memory policy, budgets and cgroup limits aim to prevent it |

Principles: fail static (the edge keeps its last good snapshot), small privileged core (only the daemon may need root), hosts own their workers, and every state change is an event.
