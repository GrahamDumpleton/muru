# 20. Future ideas

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

Recorded for later evaluation and not committed scope. Implementations must not foreclose them.

## 20.1 Managed HTTP server releases

Beyond the limited form in section 9.9: autoscaling by instances using the edge's request signals, socket activation (LISTEN_FDS) so instances restart without refusing connections, and more server profiles. Issues to weigh then: two process managers competing (the server's own master against Muru's scaling), weaker readiness than warm-up gating, and per-instance memory without shared preload.

## 20.2 gRPC service mounts

Beyond the gRPC mounts of section 6.13: servicers written for grpclib or betterproto, and broader coverage of grpc.ServicerContext.

## 20.3 Sub-interpreters

Sub-interpreters run inside a worker process and use that process's interpreter build, so they sit below variants and never replace them. Two uses are possible:

- **Isolation**, like mod_wsgi's application groups: interpreter = 'isolated' on a mount gives it its own sub-interpreter in each worker, with its own sys.modules and globals, so independent applications or settings modules can share processes.
- **Parallelism:** a group could run several sub-interpreters per worker, each with its own GIL (Python 3.12 and later), for example processes = 2, interpreters = 4, threads = 4, with the edge counting slots as interpreters times threads.

Caveats to weigh: with preload and fork, workers share the application's memory copy-on-write, whereas each sub-interpreter imports the application separately and shares no Python objects except immortal core objects, so per unit of parallelism they are likely to use more memory than forked workers. Their advantages are fewer processes and descriptors, faster communication between interpreters, and isolation. Every compiled extension an application imports must support per-interpreter isolation, and many do not yet. Muru's own runtime library would need sub-interpreter support too, which PyO3 does not yet provide as far as is known, so the bridge might drive sub-interpreters through the C API from Rust with a minimal per-interpreter module. Section 6.12 keeps the worker internals ready for this.

## 20.4 Uploads

Upload progress reporting by upload id, and resumable uploads using the tus protocol on the same spool.

## 20.5 Shadow traffic

Mirroring safe requests to a candidate release and discarding its responses, with comparison metrics.

## 20.6 Predictive scaling

Learning daily and weekly patterns from history and proposing or applying schedules.

## 20.7 Shared memory on the worker link

The link's frames are plain bytes, so a transport other than a Unix socket could carry them. Two forms have been considered and deferred: a memfd for individual large in-memory response bodies, passed like a FILE frame and sent by the edge from the shared pages, which saves one copy per byte and pays off only above a few hundred kilobytes; and a shared-memory ring per connection with futex or eventfd signalling in place of the socket altogether, which saves the kernel copies and syscalls per frame but needs its own flow control, dead-peer detection and a second signalling design on macOS. Neither is justified by the phase 0 numbers, where the link's cost per byte sits below any realistic network link. The frame format must stay transport-neutral so either can be tried later.

## 20.8 Raw streams after Upgrade or CONNECT

An ASGI extension carrying an unframed bidirectional byte stream over the existing worker stream, for custom protocols, keeping the edge in the path. Handing raw client sockets to workers was considered and rejected (TLS, HTTP/2 and HTTP/3 multiplexing, buffered bytes, loss of limits and drains).

## 20.9 Sync and async helpers

Muru-bound versions of sync_to_async and async_to_sync, running on the worker's thread pool and loop with the caller's context, could be added under muru.asgi if applications want them. They are not in scope: asgiref's work unchanged under Muru and are what Django uses, muru.loop() covers the raw need, and generic versions bring the calling-convention introspection problems that wrapt's markers exist to solve. Any version added would be a plain closure so that inspect.iscoroutinefunction is right by construction.

## 20.10 Sanitizer builds of the extension

ThreadSanitizer finds data races in native code, which on the free-threaded build are the bugs a stress test only sometimes provokes, and a debug CPython (--with-pydebug) asserts on thread state and reference counting mistakes in C API use. Neither interpreter is provided by uv, so a nightly job would build CPython from source with the sanitizer or in debug mode, build the extension against it with the matching Rust sanitizer flags, and run the integration suite. Deferred because PyO3's types already enforce GIL and thread-state discipline in the bridge, and the many-threads stress test of section 6.11 runs on every free-threaded test run; the job would be worth adding once the extension carries more native code of its own, or if a race is ever suspected.

## 20.11 Admin API client for applications

An application hosted by Muru could manage the instance it runs in: deploy and drain releases of other applications, apply routes, watch events and wait for releases. The motivating case is a front end that provisions a per-user instance of some other application on demand, for example a workshop site that deploys a containerised application per logged-in user with a memory limit and shutdown on idle, and tears it down afterwards. The operations already exist on the muru.admin.v1 service (section 10), so the work is a client and an access model, not new daemon operations.

**Client.** A typed facade over stubs generated from proto/muru/admin/v1/admin.proto, published as a separate distribution (for example muru-admin, importable as muru.admin) rather than inside the runtime library, because grpcio is a large compiled dependency and every host and worker imports muru. Both a sync and an asyncio client from the same stubs, since a WSGI front end wants blocking calls and an ASGI one wants awaitables, and the streaming operations (WaitRelease, WatchEvents, TailLogs) read best as async iterators. The client connects to the admin socket or, later, to the TCP listener with mTLS (phase 5); it never travels over the worker link, which remains the data plane. A client over the JSON gateway with no gRPC dependency is a possible lighter alternative and would share the facade.

**Access.** Section 12 and ADR 0026 make access off by default: the socket belongs to the daemon and applications run as the service user or their own account, so a worker cannot connect unless the operator grants it. The grant must be narrower than membership of the socket's group, because the full API includes deploying a release that runs as root and daemon-level operations such as Shutdown, UpgradeDaemon and ApplyConfig over every application. Before any application is given access the API needs principals and scopes, listed under roles in section 12 and not yet designed:

- A per-application grant in configuration naming the permitted operations and the applications they may act on, for example deploy, drain and watch over applications whose names match a prefix, and never the daemon-level operations.
- A credential identifying the release as the principal, minted by the daemon when the release starts and passed in the environment, or a per-application admin socket, rather than a Unix group shared by every grantee.
- The principal recorded in every event and audit entry, so the dashboard shows which application made each change.

The same mechanism would serve the dashboard's mutating actions (section 10) and TCP access in phase 5, so it should be designed once, in an ADR, before either is built.

**Dependencies and timing.** The workshop case also needs container releases and base images (section 8), memory limits through cgroups, and scale to zero or shutdown on idle, all phase 4 items; a front end that itself runs in a container reaches the admin socket only through a bind mount or the phase 5 TCP listener. The client and access model fit at the end of phase 3 or the start of phase 4 so they are ready when those arrive. Until then a separate process with permission on the admin socket can drive an instance with generated stubs or the JSON gateway, which needs no change.

## 20.12 PROXY protocol

Accepting the PROXY protocol on a listener, for a load balancer that passes TCP through without terminating HTTP and cannot preserve the client's source address (ADR 0029). The header is not authenticated, so the design would be strict: off by default and per listener, required from the listed sources and never parsed from any other, a connection from a listed source closed without a valid header, no optional mode, v2 parsed to a size cap within the header read timeout with its extensions ignored, and the address it gives standing for the client without making that client trusted for forwarded headers.
