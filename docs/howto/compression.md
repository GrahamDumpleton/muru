# Response compression

```{admonition} Status
:class: note

Responses are compressed at the edge, from the daemon's `[compression]`
table or on by default. Per-host and per-route settings, precompressed
static variants and request decompression are still to come.
```

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
| `encodings` | `['br', 'zstd', 'gzip']` | offered in this preference order; the first the client accepts is used |
| `min_size` | `1KB` | the smallest body worth compressing |
| `types` | `text/*`, `application/json`, `application/javascript`, `application/xml`, `image/svg+xml` | the content types compressed, exact or a family as `text/*` |
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

An application keeps one response uncompressed with the header
`X-Muru-Compress: off`, which the edge removes before sending, or with
`Cache-Control: no-transform`. Responses that mix secrets with input the
client controls, such as a CSRF token on a page that echoes a query
parameter, should opt out: compressing them exposes the BREACH class of
attacks, where an attacker learns a secret from the compressed length.
`HEAD` responses, `204`, `304` and responses already encoded are never
compressed.

The access log and the request histograms count what the application
produced, before compression.
