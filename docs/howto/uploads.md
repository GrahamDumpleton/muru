# Uploads

```{admonition} Status
:class: note

Upload routes under the daemon: multipart forms and raw bodies stored by
the edge, `muru.uploads` for WSGI and ASGI applications, digests, the
limits and quotas, and the precheck. `muructl run` has no routes, so
there uploads reach the application in the body as usual.
```

A route with `upload` set makes the edge receive the request body before
the application sees the request, and store each file it carries. The
application's thread or task is not held while a large file arrives, and
the file is already on disk, with its digest computed, when the request
reaches it.

```toml
[[route]]
host = 'site'
path = '/media/upload'
app = 'media'
upload = 'multipart'
max_body_size = '2GB'
max_file_size = '1GB'
max_files = 10
```

`upload = 'multipart'` takes `multipart/form-data` forms; a body of any
other type goes to the application unchanged. `upload = 'raw'` stores the
whole body as one file, whatever its type.

## What the application receives

A multipart form keeps its ordinary fields, and each file field is
replaced, where it stood in the form, by text fields that describe the
stored file. For a file field named `avatar`:

| Field | Value |
| --- | --- |
| `avatar.name` | The file name the client gave, which may be empty |
| `avatar.content_type` | The content type the client declared, which may be empty |
| `avatar.size` | The bytes received |
| `avatar.sha256` | The SHA-256 digest in hex, and a field for each other algorithm in `hash` |
| `avatar.ref` | The reference to the stored file, unique within the request |

The body is the rewritten form, with `CONTENT_LENGTH` to match, so the
application's usual form parsing works and no file content passes
through it. A raw upload arrives with an empty body and `CONTENT_LENGTH`
of 0, and the stored file stands for the whole original body, named by
the `filename` of a `Content-Disposition` header when the client sends
one.

## Reaching the files

`muru.uploads` gives the stored files of the current request, from a
WSGI environ or an ASGI scope:

```python
from muru import uploads

def application(environ, start_response):
    for upload in uploads.files(environ):
        upload.save(f"/srv/media/incoming/{upload.sha256}")
    ...
```

`uploads.files(request)` lists every file in the order it arrived, and
`uploads.get(request, ref)` finds the one a form's `FIELD.ref` value
names. Outside Muru, and on a route without `upload`, `files()` returns
an empty list and `get()` returns `None`, so the same code runs under any
server. Each upload has:

| Attribute | Purpose |
| --- | --- |
| `field`, `name`, `content_type`, `size`, `ref` | As in the form fields; `field` is `None` for a raw upload |
| `sha256`, `digests` | The SHA-256 digest, or `None` without it, and every digest by algorithm |
| `open()` | A readable, seekable binary file over the upload, from its start; each call gives an independent one |
| `save(dest, overwrite=False)` | Gives the file a permanent name and returns it |
| `path` | A path that opens the file, for libraries that need one, valid only during the request |

`save()` links the stored file into place when the destination is on the
same file system as the spool and the worker may link it, which takes no
time whatever the size; otherwise it copies. An existing destination
raises `FileExistsError` unless `overwrite=True`, which replaces it in
one step. Under a daemon running as root, files are stored by the edge's
service user, so an application running as a user of its own copies
rather than links.

Upload objects belong to their request. The files are closed when the
request ends, and a file that was not saved disappears, including when
the worker crashes; do not keep upload objects beyond the request.

## Digests

`hash` lists the digests computed while each file arrives: `sha256` (the
default), `sha384`, `sha512` and `sha1`. Each becomes a form field and an
entry in `digests`. `hash = []` computes none.

## Asking the application first

`precheck = true` sends the application the request's headers alone
before the body is read. The application sees `muru.precheck` set in the
environ, or `precheck` in the scope's `extensions["muru"]`, and
`uploads.is_precheck(request)` reads either:

```python
def application(environ, start_response):
    if uploads.is_precheck(environ):
        if not allowed(environ):
            start_response("403 Forbidden", [("Content-Length", "0")])
            return []

        start_response("200 OK", [("Content-Length", "0")])
        return []
    ...
```

A 2xx answer lets the edge tell a client waiting with `Expect:
100-continue` to send the body, receive it, and send the request again
with the files. Any other answer, or a redirect, is the client's
response, and the body is never read. A precheck that allows the request
is logged as part of it rather than as a line of its own.

## Limits and quotas

| Key | Where | Default | Answer when exceeded |
| --- | --- | --- | --- |
| `max_body_size` | route | the daemon's `[limits]` value, 10MB | 413 `body_too_large` |
| `max_file_size` | route | no limit | 413 `upload_too_large` |
| `max_files` | route | 100, at most 500 | 413 `too_many_files` |
| `spool_quota` | route | 10GB | 507 `upload_spool_full` |
| `spool_quota` | the daemon's `[limits]` table | no limit | 507 `upload_spool_full` |

On an upload route `max_body_size` bounds the body as received, files
included, so a route that takes large files raises it. A route's
`spool_quota` bounds what its application's stored files hold at once,
across the requests in progress, when a request on the route arrives;
the `[limits]` table's bounds every application's together. A full disk
is also 507. The form's ordinary fields stay in memory and together may
hold 10MB. A form that cannot be parsed is 400 `upload_malformed`.

The edge stores files in `spool/` under the daemon's state directory, in
a directory for each application that only the edge's user can enter.
The request deadline starts once the upload has arrived, and the body
idle timeout bounds each pause while it arrives.
`muru_upload_spool_bytes` reports what each application's uploads hold,
and `muructl route list` marks upload routes.

## Framework recipes

Most applications expect their framework's own upload objects. Each
recipe below falls back to the framework's normal parsing when
`uploads.files()` is empty, so the same code runs everywhere.

**Flask and Werkzeug** build `FileStorage` objects shaped like
`request.files`:

```python
from werkzeug.datastructures import FileStorage, MultiDict
from muru import uploads

def offloaded_files(request):
    found = uploads.files(request.environ)

    if not found:
        return request.files

    return MultiDict(
        (upload.field, FileStorage(upload.open(), upload.name, upload.field, upload.content_type))
        for upload in found
    )
```

**Starlette and FastAPI** wrap each upload in an `UploadFile`:

```python
from starlette.datastructures import Headers, UploadFile
from muru import uploads

def offloaded_files(request):
    return {
        upload.field: UploadFile(
            upload.open(),
            size=upload.size,
            filename=upload.name,
            headers=Headers({"content-type": upload.content_type}),
        )
        for upload in uploads.files(request.scope)
        if upload.field is not None
    }
```

**Django** builds `UploadedFile` objects in a small middleware that runs
before the views, so that `request.FILES` and forms with a `FileField`
work unchanged. Add it to `MIDDLEWARE` ahead of anything that reads the
request's form:

```python
from django.core.files.uploadedfile import UploadedFile
from django.utils.datastructures import MultiValueDict
from muru import uploads

def offloaded_uploads(get_response):
    def middleware(request):
        found = uploads.files(request.META)

        if found:
            # The rewritten form first: Django parses it on first use and
            # would replace FILES with what it found there.

            _ = request.POST
            files = MultiValueDict()

            for upload in found:
                files.appendlist(
                    upload.field,
                    UploadedFile(upload.open(), upload.name, upload.content_type, upload.size),
                )

            request._files = files

        return get_response(request)

    return middleware
```

A storage backend whose directory is on the spool's file system can call
`upload.save()` instead of copying the file object.
