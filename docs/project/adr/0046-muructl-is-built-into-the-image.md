---
status: accepted
date: 2026-11-03
---

# ADR 0046: muructl is installed into the image with uv, with no bundle and no official base images

## Context and problem statement

Step 3 of the containers goal was to publish Muru's pieces as one bundle:
a statically linked `muructl` and the runtime library for every supported
interpreter, added to a Dockerfile with one line or injected into an
existing image at deployment time through an init container, a Kubernetes
image volume, a Docker or Podman image mount, or a shared volume. Step 4
was to add official base images, `muru-python` per Python version with
free-threaded and distroless variants (specification sections 8.6 and
17.1). How should Muru get into an application's image?

## Decision drivers

- Teams add a server to an image when they build it. Nobody expects to
  inject one into a container at deployment time.

- `muructl build --runtime-dir` and `muructl runtime install` already
  fetch the runtime library for the image's own interpreter at build
  time, with its hash checked, so it always matches.

- Every artifact published is a matrix to build, test and pick from:
  Python version, free-threaded or not, libc and architecture.

- Kubernetes image volumes are likely mounted noexec, which would rule
  out running `muructl` or loading the extension module from one.

- The edge serves every request, so a `muructl` built for portability
  must not be slower, or hold more memory, than the build on PyPI.

## Considered options

- A bundle of `muructl` and every runtime library, copied in or injected
  at deployment, with official base images for those who want Muru
  preinstalled.

- A statically linked `muructl` (musl, with an allocator compiled in),
  published on GitHub Releases and as `ghcr.io/muruhq/muructl`, an image
  holding only the binary, copied into a Dockerfile with one `COPY
  --from` line as uv is.

- The `muructl` wheels already on PyPI, installed in the image's build
  stage with `uv tool install`, and the binary copied into the final
  stage.

## Decision outcome

Chosen option: the wheels on PyPI, installed with `uv tool install`. The
build stage, which has uv for the environment anyway, runs

```dockerfile
RUN UV_TOOL_BIN_DIR=/usr/local/bin uv tool install muructl
```

and the final stage copies `/usr/local/bin/muructl`, which Docker
dereferences from uv's link to the binary itself. uv picks the
manylinux or musllinux wheel for the base, so it works on Debian and
Alpine, needs no pip, and needs no Python in the base, since uv fetches
one for the tool's environment. The glibc build needs glibc 2.28 and
`libgcc_s`, which every image with a Python has, and the binary copied
into a distroless Python image runs there. The build stage then makes
the environment and fetches the runtime library for its interpreter under
`/opt/muru`, where `muructl serve` finds it. Nothing is injected at
deployment time, and no official base images are built; issue #129 is
parked in case teams ask for a ready-made base.

The static binary was measured first and rejected: no allocator gave it
both the glibc build's throughput and its memory use (below). With it go
the muructl image (#113) and the standalone tarballs it was built for
(#142), both parked.

### Consequences

- Good: nothing new to build or publish; the binary in the image is the
  one tested on PyPI; an image is complete when it is built, as people
  expect; no dependence on how a platform mounts volumes.

- Good: a container release under the daemon (section 8.5) uses the
  image's runtime library, or the daemon mounts its own
  (`runtime_source = 'daemon'`), as before.

- Bad: an existing image cannot be run under Muru without rebuilding it.

- Bad: the build stage needs uv and network access to PyPI for `muructl`
  and the runtime library, or a wheel directory for the library; a
  private package index is not yet followed for it (issue #312).

- Bad: no single binary for a base too old for the manylinux_2_28 wheel
  or without a C library.

## More information

### Measurements of the static binary

`muructl serve` with 4 processes of 4 threads, on a 4-CPU arm64 Linux
virtual machine, driven by `oha` with 64 connections, the median of three
10-second runs per scenario. The scenarios are a WSGI response of 14
bytes and of 1 MiB, and a static file of 14 bytes and of 1 MiB. Each cell
gives requests per second and the peak resident memory of the `muructl`
process, which holds the edge and the controller under `muructl serve`.
Under the daemon the edge is a `muructl` process of its own, with the same
allocator. The Python host and workers held 156 MB together in every
configuration: their allocator is the interpreter's, not muructl's.

| Build | WSGI 14 B | WSGI 1 MiB | Static 14 B | Static 1 MiB | Idle after 20 s |
| --- | --- | --- | --- | --- | --- |
| glibc (the PyPI build) | 79,512 · 23 MB | 2,613 · 100 MB | 48,081 · 45 MB | 6,501 · 70 MB | 68 MB |
| glibc, cap 16 | 81,849 · 24 MB | 2,650 · 98 MB | 57,529 · 72 MB | 7,132 · 102 MB | 102 MB |
| static, musl's allocator | 59,415 · 16 MB | 1,886 · 37 MB | 41,141 · 19 MB | 6,618 · 29 MB | 15 MB |
| static, musl's allocator, cap 16 | 59,710 · 17 MB | 2,013 · 39 MB | 38,877 · 18 MB | 6,704 · 26 MB | 15 MB |
| static, mimalloc | 89,411 · 41 MB | 2,564 · 213 MB | 48,353 · 188 MB | 6,508 · 250 MB | 257 MB |
| static, mimalloc, `MIMALLOC_PURGE_DELAY=0` | 89,430 · 39 MB | 679 · 127 MB | 48,348 · 62 MB | 5,846 · 155 MB | 43 MB |
| the same, cap 64 | 90,156 · 38 MB | 712 · 126 MB | 52,624 · 58 MB | 6,829 · 90 MB | 44 MB |
| the same, cap 16 | 87,800 · 39 MB | 698 · 116 MB | 71,415 · 44 MB | 7,032 · 51 MB | 37 MB |

"Cap" limits tokio's blocking threads, which read static files, from its
default of 512.

- musl's own allocator is 25 to 28 percent slower than glibc on WSGI
  responses, though it holds the least memory.

- mimalloc is 12 percent faster than glibc on small WSGI responses, but
  holds two to four times the memory and keeps it when idle, after large
  WSGI responses as well as static files.

- `MIMALLOC_PURGE_DELAY=0` returns the memory, but cuts 1 MiB WSGI
  responses to about 700 a second, a quarter of glibc's rate.

- jemalloc did not build for the musl target with the distribution's
  `musl-gcc`, and needs a musl cross toolchain; it was not measured.

- Capping the blocking threads at 16 serves small static files faster
  with every allocator, by 20 percent with glibc, and is worth doing on
  its own.

Specification sections 8.6, 16.1, 17.1 and 17.3; issues #247 (step 3),
#113, #142, #129 and #128.
