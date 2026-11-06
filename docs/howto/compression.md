# Response compression

The edge compresses responses on behalf of applications, so Python never
spends time on it. A response is compressed when the client's
`Accept-Encoding` takes one of the encodings offered, its `Content-Type`
is one of the types listed, its body is at least `min_size` long (or of
unknown length, as a streamed body is), it carries no `Content-Encoding`
of its own, and the application has not opted it out. `Vary:
Accept-Encoding` is added and `Content-Length` removed, since the length
is no longer known in advance. Streamed responses are compressed chunk by
chunk with a flush after each, so event streams keep flowing.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | whether responses are compressed at all |
| `encodings` | `['br', 'zstd', 'gzip']` | offered in this preference order; the first the client accepts is used. An empty list is refused: `enabled = false` turns compression off |
| `min_size` | `1KB` | the smallest body worth compressing, at least 1 byte |
| `types` | `text/*`, `application/json`, `application/javascript`, `application/xml`, `image/svg+xml` | the content types compressed, exact or a family as `text/*`; an empty list is refused |
| `level` | `default` | `fast`, `default` or `best` |

Under the daemon the table goes in its configuration file and `muructl
daemon reload` applies a change; `muructl run` compresses with the
defaults, or not at all with `--no-compression`:

```toml
[compression]
encodings = ['br', 'gzip']
min_size = '2KB'
types = ['text/*', 'application/json']
level = 'fast'
```

A [virtual host](virtual-hosts.md#settings-for-a-hosts-routes) and a
route each take a `compression` table with the same keys. A key the
route sets is used, else the host's, else the daemon's table's, else the
default:

```toml
[host.api]
names = ["api.example.com"]
compression = { types = ["application/json"], level = "fast" }

[[route]]
host = "api"
app = "api"

[[route]]
host = "api"
path = "/exports"
app = "api"
compression = { enabled = false }
```

Responses for `api.example.com` are compressed when they are JSON, at
the fast level, with the encodings and the smallest size the daemon's
table gives. Under `/exports` nothing is compressed. The settings are
those of the route the client's request matched, and of the host alone
for a response no route gives, such as a 404 for a path with no route.

An application keeps one response uncompressed with the header
`X-Muru-Compress: off`, which the edge removes before sending, or with
`Cache-Control: no-transform`. Responses that mix secrets with input the
client controls, such as a CSRF token on a page that echoes a query
parameter, should opt out, or be served by a route with `compression =
{ enabled = false }`: compressing them exposes the BREACH class of
attacks, where an attacker learns a secret from the compressed length.
`HEAD` responses, `204`, `304` and responses already encoded are never
compressed.

The access log and the request histograms count what the application
produced, before compression.
