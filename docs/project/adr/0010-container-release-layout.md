---
status: accepted
date: 2026-10-23
---

# ADR 0010: Host and workers inside the container, daemon and edge outside

## Context and problem statement

A release can run from a container image. Which components live inside the
container decides how traffic, descriptors and supervision cross the boundary
(specification section 8.5).

## Decision drivers

- The daemon and edge must stay on the node, shared by every release.

- Descriptor passing (file hints, uploads, log pipes) must keep working.

- The container needs no separate init process.

## Considered options

- The whole of Muru inside each container, with HTTP between containers.

- Only the workers inside the container, the host outside.

- Host and workers inside the container as PID 1 and its children, the daemon
  and edge outside, communicating through a per-release run directory
  bind-mounted at /run/muru.

## Decision outcome

Chosen option: the third. Unix sockets and SCM_RIGHTS work across the
boundary because the kernel is shared. The host sets itself as subreaper,
reaps orphans and handles signals, so tini is not needed. The daemon creates
the container through the Docker Engine API (which Podman also serves) with
restart policy off and supervises the release itself.

### Consequences

- Good: file hints and uploads work unchanged; cgroup limits come from the
  container settings; the image contract is small.

- Bad: Linux only (macOS runs containers in a virtual machine); static
  directories must be copied out of the image or shared through a volume; a
  probe container is needed to read interpreter details.

## More information

Specification sections 6.10, 8.5 and 8.6.
