# Preload and fork safety

```{admonition} Status
:class: note

Everything on this page is implemented in development mode: the preload
modes, the import check and its fallback, the lifecycle hooks and
`muru.context()`. The daemon will use the same host code.
```

Muru imports your application once, in a host process, and forks workers
from it. This page explains why, what can go wrong when an import does too
much, and how Muru notices before it bites.

## Why import once

A worker that imports the application itself spends seconds on imports and
holds its own copy of every module. Forking from a host that has already
imported everything gives each worker those modules for the price of a
fork, shared through copy-on-write memory. Replacing a worker is therefore
cheap, and many workers do not cost many times the memory.

The catch is the fork itself. A fork copies memory but not the things that
live outside it: a socket is a kernel object shared by both processes, a
thread exists only in the process that started it, and a child process has
one parent. An import that opened a database connection, started a
background thread or launched a helper process leaves the workers with a
connection two processes think they own, a thread that silently does not
exist, or a child nobody reaps. These bugs are intermittent, and they show
up in production rather than on the first request.

## The import check

Muru does not take the advice on trust. In the default mode, `preload =
auto`, the host imports the application inside a recorder that notes every
file descriptor, thread and child process the import leaves behind, and
attributes each to the module whose import created it. If the import is
clean, the host forks from it. If it is not, the terminal says exactly what
was found and what to do:

```
muru.host[98353]: preload: the import of shop.db left a connected socket (fd 7, to 127.0.0.1:5432); close it in a pre_fork hook and reopen it in post_fork, or open it lazily on first use
muru.host[98353]: preload: falling back to per-worker imports, which costs slower worker replacement and no shared memory; set preload to false to silence this
```

and the host starts over with each worker importing the application
itself, which is always safe. The application runs either way; what you
lose in the fallback is the fast restart and the shared memory, and the
message tells you how to get them back.

The check looks for:

- sockets and pipes still open after the import, with the peer address
  when it can be read, which catches database connections, Redis clients,
  message queue consumers and the pipes of helper processes;
- threads started during the import, which catches schedulers, metrics
  exporters and connection pool reapers started at module level;
- child processes, which catches anything launched with `subprocess` or a
  multiprocessing pool at import time.

An event loop created at import time is caught through the descriptors it
opens.

## The three modes

| `--preload` | Behaviour |
| --- | --- |
| `auto` (default) | Preload when the import check passes; otherwise fall back to per-worker imports and say so |
| `true` | Preload, and fail to start when the check does not pass. For production, where a silent fallback would hide a regression |
| `false` | Import in every worker, without the check. For applications that cannot be made fork-safe, or to silence the message |

The same setting lives in a project's `[tool.muru]` table.

## Hooks: doing it at the right time

Most things that are wrong at import time are right a moment later. A hooks
module, named with `--hooks` or in `[tool.muru]`, can define any of these
functions, each taking the context described below:

| Hook | Runs | Use it for |
| --- | --- | --- |
| `on_load` | once in the host, after the import | checks and warm-up that are safe to share |
| `pre_fork` | once in the host, before the first fork | closing anything that must not cross the fork |
| `post_fork` | in each worker, right after the fork | opening connections, starting threads, seeding random state |
| `on_ready` | in each worker, before it takes requests | signalling readiness to something else |
| `on_drain` | in each worker when it is told to stop, before in-flight requests finish | stopping background work |
| `on_exit` | in each worker after its last request | flushing and closing |
| `on_reopen_logs` | in each worker on SIGUSR1 | log rotation |

So a connection pool created at import time becomes: create it lazily, or
close it in `pre_fork` and open it in `post_fork`. A background thread
becomes: start it in `post_fork`. When the fallback to per-worker imports is
in effect, `on_load` and `pre_fork` run in each worker before its
`post_fork`, so hooks behave the same in both modes. An exception in a hook
stops the worker, and the terminal says which hook and why.

## Knowing where you are

`muru.context()` returns a small object saying where the current code runs:
the application name, the release, the worker's index (or `None` in the
host), the interface and the process model. It is safe to call outside Muru
too, where it returns development defaults, so library code can call it
unconditionally. Hooks receive the same object.

## Factories

If your entrypoint is a factory, a function that builds the application,
`--factory` (or `factory = true` in `[tool.muru]`) calls it in each worker
after `post_fork`. That is the fork-safe default, since whatever the
factory opens then belongs to the worker. `factory_phase = "host"` calls it
once in the host instead, when the factory is known to be clean and the
build is expensive enough to be worth sharing.

## What the daemon adds

In production the same host code runs under the daemon, with releases that
can run side by side. The import check is the reason `auto` is the default
everywhere: an import that is not fork-safe is reported on the laptop, not
discovered on the server. The design specification's
[section 6.1 and 6.2](../project/design/06-python-runtime.md) describe the
host lifecycle and the fork sequence in full, including the garbage
collector handling that keeps forked memory shared.
