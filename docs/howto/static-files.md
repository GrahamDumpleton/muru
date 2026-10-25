# Static files

```{admonition} Status
:class: note

Static routes to a directory, with index files, conditional requests,
ranges, precompressed variants, cache headers and a fallback to an
application or a file, `muructl serve-static`, and internal routes that
only an [internal redirect](internal-redirects.md) reaches. Directory
listings, static files that belong to a release, file hints from
applications (`X-Muru-Sendfile`) and the metadata cache are still to
come.
```

A route that names a directory instead of an application is served by
the edge: Python never sees the request.

```toml
[[route]]
host = 'site'
path = '/static/'
static = '/srv/shop/collected-static'
cache = { max_age = '1y', immutable = true }
```

| Key | Default | Meaning |
| --- | --- | --- |
| `static` | | the directory served |
| `index` | `['index.html']` | the files tried, in order, when a directory is requested |
| `fallback` | none | `{ app = 'name' }` sends what no file answers to the application; `{ file = 'index.html' }` serves that file instead, for a single-page application |
| `cache` | none | `max_age` (for example `1y` or `1h`), `immutable`, and `except`, patterns of files that get `no-cache` instead |
| `redirect_slash` | `true` | redirect a directory asked for without a trailing slash (301) to the slash form, so relative links resolve |
| `case` | `strict` | `insensitive` accepts names that differ from the file's on disk in case |
| `internal` | `false` | the route answers 404 to clients and is reached only through an [internal redirect](internal-redirects.md) |
| `accept_redirects_from` | none | applications other than the `fallback` one whose redirects may reach the route |

Every file is sent with `Content-Type` from its name, `ETag`,
`Last-Modified` and `Accept-Ranges`. `If-None-Match` and
`If-Modified-Since` are answered `304`, a single `Range` is answered
`206` (`416` when it lies past the end), and `HEAD` sends the headers
alone. Where a client accepts `br`, `zstd` or `gzip` and a file with
that suffix lies next to the file (`app.js.br`, `app.js.zst`,
`app.js.gz`), the variant is served with `Content-Encoding` and `Vary`;
otherwise a large text file is [compressed](compression.md) on the fly.

## Mixing files and an application

Routes match by longest path prefix, so a site can be files with the
application under a prefix, or an application with files under a prefix:

```toml
[[route]]
host = 'site'
path = '/'
static = '/srv/site/public'

[[route]]
host = 'site'
path = '/app/'
app = 'shop'
```

When files and application paths overlap, a fallback sends the
application what no file answers. For `GET` and `HEAD` the edge serves
the file when there is one, or the directory's index file; otherwise, and
for every other method, the request goes to the application, so a `POST`
to a path that also has a file still reaches it:

```toml
[[route]]
host = 'site'
path = '/'
static = '/srv/site/public'
fallback = { app = 'shop' }
```

A single-page application serves its shell for every path that has no
file, with the hashed assets cached for a year and the shell itself
always revalidated:

```toml
[[route]]
host = 'site'
path = '/'
static = '/srv/spa/dist'
fallback = { file = 'index.html' }
cache = { max_age = '1y', immutable = true, except = ['index.html'] }
```

## What is never served

Hidden files and directories (names starting with a dot) are never
served and fall through to the fallback. A path is percent-decoded and
normalised before the file is looked up, `..` never climbs out, and the
file's canonical path must lie inside the directory, so a symbolic link
pointing elsewhere is refused. Each name in the path must match the name
on disk exactly, in case and in Unicode form, so a request for
`logo.png` does not serve `Logo.PNG` on macOS the way the file system
would; `case = 'insensitive'` opts a route out of that check.

## Serving a directory in development

`muructl serve-static DIR` serves a directory with no configuration, for
checking a built front end or a documentation build, with the same code
as a static route:

```
muructl serve-static dist/ --port 3000
muructl serve-static dist/ --fallback index.html
muructl serve-static site/ --index index.htm --index index.html --no-compression
```

`--bind`, `--port` (or `auto`) and `--access-log` work as they do for
`muructl run`.
