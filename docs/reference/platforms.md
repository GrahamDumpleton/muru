# Supported platforms and Python versions

Muru is published as two packages on PyPI: `muructl`, the command, and
`muru`, the runtime library that runs inside your application's
interpreter (see [the runtime
library](../howto/interpreters.md#the-runtime-library)).

## Operating systems

| Operating system | Architectures | Wheels |
| --- | --- | --- |
| Linux with glibc 2.28 or later | x86_64, aarch64 | manylinux_2_28 |
| Linux with musl 1.2 or later, such as Alpine | x86_64, aarch64 | musllinux_1_2 |
| macOS | arm64 (Apple silicon), x86_64 (Intel) | macosx |

Windows is not supported.

## Python versions

| Interpreter | Versions |
| --- | --- |
| CPython | 3.11, 3.12, 3.13, 3.14, 3.15 |
| CPython, free-threaded | 3.14t, 3.15t |

`muru` has a wheel for each of these on each platform above. `muructl` is
a single binary with no Python dependencies, so one wheel per platform
installs with any Python's pip, uv or pipx, and the interpreter it was
installed with has no bearing on the interpreters it runs applications
on. PyPy and other implementations are not supported.

## Differences between Linux and macOS

Everything works on both, except where this table says otherwise:

| Feature | Linux | macOS |
| --- | --- | --- |
| [Worker memory](../howto/memory.md) | USS, PSS and RSS from the kernel's accounting | RSS, with the task's physical footprint standing in for USS and PSS |
| [The node's memory budget](../howto/scaling.md#the-nodes-budget) | Counts each host's own memory as well as its workers' | Counts the workers' memory |
| Subprocesses left by a killed worker | Collected by the host, given the release's shutdown timeout, then stopped | Not collected |
| [Stacks of a stuck worker](../howto/debugging.md) | Read from outside the worker with Python 3.14 or later (PEP 768) | Asked of the worker, with `faulthandler` when it cannot answer |
| [Running under systemd](../howto/systemd.md) | A shipped unit with readiness notification | Not applicable |
