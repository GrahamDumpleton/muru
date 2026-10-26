# Python API

```{admonition} Status
:class: note

This is the runtime library's API as it exists today. The rest of the
planned API (`muru.loop()`, `muru.asgi`, `muru.uploads` and
`muru.request_id()`) arrives with the features each belongs to; section
6.9 of the design specification lists them.
```

The `muru` package is installed in the interpreter that runs your
application. It is safe to import anywhere, including outside Muru and in
tests, where every function returns development defaults.

## `muru.context()`

Returns a `muru.Context`, a frozen dataclass saying where the current code
runs:

| Field | Value |
| --- | --- |
| `app` | The application name: the top-level module of the entrypoint, so `shop.wsgi:application` is `shop` |
| `release` | The release identifier; `dev` in development mode |
| `group` | The worker group; `default` until worker groups arrive |
| `worker_id` | The worker's index from 0, or `None` in the host (and outside Muru) |
| `interface` | `wsgi`, `asgi` or `grpc`, detected from the callable or declared with `--interface` |
| `process_model` | `threaded`, `process-per-request` (one thread per worker) or `free-threaded` (on a free-threaded interpreter) for WSGI; `async` for ASGI |

Outside Muru the defaults are `app="default"`, `release="dev"`,
`group="default"`, `worker_id=None`, `interface="wsgi"` and
`process_model="threaded"`.

## Lifecycle hooks

A hooks module, named with `--hooks` or `hooks` in `[tool.muru]`, may define
any of these module-level functions. Each takes the `Context` and its
return value is ignored.

| Hook | Where and when |
| --- | --- |
| `on_load(context)` | In the host, once, after the application is imported |
| `pre_fork(context)` | In the host, once, before the first worker is forked |
| `post_fork(context)` | In each worker, right after the fork |
| `on_ready(context)` | In each worker, after `post_fork`, before it accepts requests |
| `on_drain(context)` | In each worker when it is told to stop, before requests in flight finish |
| `on_exit(context)` | In each worker after its last request, before it exits |
| `on_reopen_logs(context)` | In each worker that receives SIGUSR1 |

With per-worker imports (preload `false`, or the fallback when the import
check fails), `on_load` and `pre_fork` run in each worker before its
`post_fork`, so a hooks module behaves the same whichever way the
application was loaded. An exception in `on_load`, `pre_fork`, `post_fork`
or `on_ready` stops that process with a traceback in the log; exceptions in
the other three are logged and otherwise ignored. With `--single` there is
no fork, so `pre_fork` does not run.

[Preload and fork safety](../explanation/preload.md) explains what the
hooks are for.

## `muru.is_internal()`

`muru.is_internal(environ_or_scope)` returns whether the request arrived
by [internal redirect](../howto/internal-redirects.md): `True` when a
WSGI environ carries `muru.internal`, or an ASGI scope's
`extensions["muru"]` carries `internal`. Outside Muru it is `False`.

## `muru.uploads`

The files the edge stored for a request on an
[upload route](../howto/uploads.md). `uploads.files(environ_or_scope)`
lists them in the order they arrived, `uploads.get(environ_or_scope,
ref)` finds the one a form's `FIELD.ref` value names, and
`uploads.is_precheck(environ_or_scope)` says whether the request is a
precheck, sent with its headers alone. Each `Upload` has `field`,
`name`, `content_type`, `size`, `ref`, `sha256` and `digests`, `open()`
for a readable, seekable file, `save(dest, overwrite=False)` to give the
file a permanent name, and `path` for libraries that need one. Outside
Muru `files()` is empty, `get()` is `None` and `is_precheck()` is
`False`.

## `muru.RequestTimeout`

The exception the soft timeout raises in the thread of a request that
has run past it (see the [timeouts guide](../howto/timeouts.md)). It is
a `BaseException` subclass, so handlers that catch `Exception` do not
swallow it; catch it by name to answer the request yourself, and use
`finally` for clean-up. It is never raised outside Muru, or in an ASGI
application, whose task is cancelled instead.

## `muru.logging`

The platform's logging defaults (the [logging guide](../howto/logging.md)
says how they are set):

| Name | Purpose |
| --- | --- |
| `default_config()` | The defaults as a `logging.config.dictConfig` dictionary: the root logger at the level in `MURU_LOG_LEVEL`, with a handler on standard error in the format in `MURU_LOG_FORMAT`. Outside Muru they are `WARNING` and `standard` |
| `formatter(format)` | The `logging.Formatter` for a format name: `standard`, `json`, or a format string |
| `JsonFormatter` | The `json` format: one object per record with `time`, `level`, `logger`, `message`, `app`, `release`, `group`, `worker`, and `exception` and `stack` when present |
| `STANDARD_FORMAT` | The `standard` format string, `%(levelname)s %(name)s: %(message)s` |

## `muru.__version__`

The installed version of the runtime library, as a string, or `0+unknown`
when the package is imported from a source tree without being installed.

## `muru.host`

The host entry point (`python -m muru.host`), which `muructl` starts. It is
internal and its arguments change without notice.
