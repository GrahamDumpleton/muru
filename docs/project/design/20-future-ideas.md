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
