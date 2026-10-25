# File hints

```{admonition} Status
:class: note

Both forms of file hint: by descriptor, for `wsgi.file_wrapper` and the
ASGI `pathsend` and `zerocopysend` extensions, and by path, for
`X-Muru-Sendfile` and a second header a mount names, from applications,
trusted upstreams and managed servers.
```

A file hint hands the body of a response to the edge, which sends the
file itself while the application thread or task is free. There are two
forms. By descriptor, the application's own open file is passed to the
edge, which sends exactly the range the application chose with the
status and headers it set. By path, a response header names a file, and
the edge serves it the way it serves a [static file](static-files.md).

## By descriptor

A WSGI application returns the object `wsgi.file_wrapper` gives it as
the response iterable:

```python
def application(environ, start_response):
    handle = open("/srv/media/report.pdf", "rb")
    start_response("200 OK", [("Content-Type", "application/pdf")])
    return environ["wsgi.file_wrapper"](handle)
```

When the wrapper itself is returned and the file is a regular file, a
duplicate of its descriptor goes to the edge, starting at the file's
current position and covering `Content-Length` bytes when that header
is set, or the rest of the file otherwise. The application's file is
closed as soon as the descriptor has been passed; the edge sends from
its own copy. An object without a regular file behind it, such as a
`BytesIO`, a wrapper that middleware has wrapped in something else, or a
wrapper returned after `write()` has sent output, is iterated in blocks
as PEP 3333 describes.

An ASGI application finds `http.response.pathsend` and
`http.response.zerocopysend` in `scope["extensions"]`. A `pathsend`
message names a file, which the worker opens as the application's user;
a `zerocopysend` message gives an open file with an optional `offset`
(the file's position when absent), `count` (the rest of the file when
absent) and `more_body`, so a file can sit between body chunks:

```python
await send({"type": "http.response.start", "status": 200, "headers": headers})
await send({"type": "http.response.pathsend", "path": "/srv/media/report.pdf"})
```

Starlette's `FileResponse` sends `pathsend` when the server advertises
it, so Starlette and FastAPI applications get this without changes.

The application has already decided the status and headers, so the edge
never applies a client's `Range` to a descriptor hint: a framework that
supports ranges computes the range itself and passes the matching offset
and length.

## By path

A response with an `X-Muru-Sendfile` header naming an absolute path has
its body replaced by that file, sent by the edge with static-file
behaviour: `Content-Length`, `ETag`, `Last-Modified`, `Accept-Ranges`,
conditional requests answered 304 and `Range` requests answered 206.
The response's own status and headers go with the file: a
`Content-Type` the application set replaces the guessed one, and a
status other than 200 sends the whole file with that status. The
`X-Muru-Sendfile` header itself never reaches the client.

The edge opens the path as its own user, and only within the
directories the mount's `sendfile_roots` lists, compared by canonical
path, so neither a relative path nor one that climbs out through a
symbolic link is served:

```toml
[app.media.release.r4]
source = '/srv/media'
sendfile_roots = ['/srv/media/files', '/var/cache/thumbnails']

[app.media.release.r4.mounts.archive]
path = '/archive'
entrypoint = 'media.archive:application'
sendfile_roots = ['/srv/archive']
```

The release's roots apply to its default mount and to every mount that
does not list its own; without any, every path hint is refused. A path
outside the roots is answered 500 `sendfile_refused` and logged with
the path; a path that names nothing, or not a regular file, is 404
`sendfile_missing`. `muructl deploy --sendfile-root DIR` sets the
release's roots from the command line, and `muructl run` reads
`sendfile_roots` from the project's `[tool.muru]` table.

Frameworks and middleware that already speak another server's header
keep working: `sendfile_header = 'X-Sendfile'` on a release or a mount
honours that header exactly as `X-Muru-Sendfile`, and removes it from
the response. Without the key, `X-Sendfile` is an ordinary header that
reaches the client. `muructl deploy --sendfile-header NAME` sets it from
the command line.

## Upstreams and managed servers

An [upstream](upstreams.md) with `trust_muru_headers = true` may give
path hints too, within its own `sendfile_roots` and with its own
`sendfile_header`:

```toml
[upstream.legacy]
addresses = ['127.0.0.1:8001']
trust_muru_headers = true
sendfile_roots = ['/srv/legacy/media']
sendfile_header = 'X-Sendfile'
```

A [managed HTTP server](managed-servers.md) release is trusted as an
upstream, and its release's `sendfile_roots` and `sendfile_header`
apply to the server's responses.
