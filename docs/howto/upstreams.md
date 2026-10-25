# HTTP upstreams

```{admonition} Status
:class: note

`[upstream.NAME]` tables: routes, static fallbacks, traffic shares and
authentication handlers that are unmanaged HTTP servers, over HTTP/1.1,
h2c or HTTP/2 with TLS, by TCP or Unix socket, with active probes and
passive ejection. Servers Muru starts and supervises itself are
[managed HTTP servers](managed-servers.md); `X-Muru-Sendfile` from an
upstream is still to come.
```

An upstream is an HTTP server Muru does not run: an existing deployment
behind another server, a service in another language, or anything else
that answers HTTP. A route can send its requests there instead of to an
application, and the rest of the edge works as it does for applications:
virtual hosts, static files, internal redirects, error pages,
compression, and traffic policies that share a route between an
application's releases and an upstream.

## Naming an upstream

```toml
[upstream.legacy]
addresses = ['10.0.0.5:8080', '10.0.0.6:8080']
protocol = 'http1'
health = { path = '/healthz', interval = '10s' }

[upstream.reports]
addresses = ['unix:/run/reports/http.sock']

[[route]]
host = 'shop'
path = '/legacy/'
upstream = 'legacy'

[[route]]
host = 'shop'
path = '/reports/'
upstream = 'reports'
```

| Key | Default | Meaning |
| --- | --- | --- |
| `addresses` | required | where the server listens: `host:port` entries, or `unix:/path` for a Unix socket; requests take the addresses in turn |
| `protocol` | `http1` | `http1`, `h2c` (HTTP/2 in the clear, by prior knowledge) or `h2` (HTTP/2 over TLS) |
| `tls` | none | connect with TLS; `h2` implies it. `ca` is a PEM file of the certificates trusted (the public roots without it), `server_name` the name checked and sent as SNI (the address's host without it), and `verify = false` skips the check |
| `health` | none | probes and ejection, below |
| `trust_muru_headers` | `false` | honour `X-Muru-Redirect` from the upstream, as from an application; its `X-Muru-*` response headers are otherwise removed |
| `connect_timeout` | `5s` | how long a connection may take to open |

The request reaches the upstream re-serialised: the client's headers,
less hop-by-hop ones, names with underscores and `X-Muru-*` headers, with
`Host` as the client sent it, `X-Forwarded-For`, `X-Forwarded-Proto`,
`X-Forwarded-Host`, `X-Real-IP` and `X-Request-ID` added. A body without
a declared length is read in full first and sent with a `Content-Length`;
a declared length is streamed. A message carrying both `Content-Length`
and `Transfer-Encoding` is read by its `Transfer-Encoding` and re-framed
([ADR 0027](../project/adr/0027-ambiguous-framing-is-reframed-not-rejected.md)),
and an invalid `Content-Length` is refused with 400 before routing. The
path arrives as the client sent it, prefix included.

The upstream's response goes back with its headers, less hop-by-hop
ones. A WebSocket upgrade is passed to an `http1` upstream and the bytes
are copied both ways once it answers 101. Limits apply as for
applications: `max_body_size`, the request deadline (504 `deadline` when
the response head does not arrive in time) and the response idle
timeout.

## Addresses, probes and ejection

Requests are spread over the addresses in turn. An address whose
connection fails is skipped for the next address in the same request,
and a request that was never sent moves on with its body intact; one
that went out in part is answered 502 `upstream_failed`. When no address
answers at all the response is 502 `upstream_failed` too.

```toml
[upstream.legacy]
addresses = ['10.0.0.5:8080', '10.0.0.6:8080']

[upstream.legacy.health]
path = '/healthz'
interval = '10s'
timeout = '2s'
unhealthy_after = 3
healthy_after = 2
eject_after = 3
eject_for = '30s'
```

| Key | Default | Meaning |
| --- | --- | --- |
| `path` | none | probed with a GET every `interval`; 2xx or 3xx is healthy. Without it, a connection that opens is healthy |
| `interval`, `timeout` | `10s`, `2s` | how often each address is probed and how long a probe may take |
| `unhealthy_after`, `healthy_after` | 3, 2 | failed probes in a row before an address is out of service, and successful ones before it is back |
| `eject_after` | 3 | failed requests in a row before an address is ejected; 0 never ejects |
| `eject_for` | `30s` | how long an ejected address is left out. One request then tries it: success puts it back, failure ejects it again for twice as long, up to ten times `eject_for` |

Probes run only with a `health` table; ejection applies always. When
every address is unhealthy or ejected the request still tries them, in
turn, which beats answering nothing. The gauge
`muru_upstream_health{upstream, address}` is 1 for an address in service
and 0 for one unhealthy or ejected, and the edge's log records each
change.

## HTTP/2 and TLS

```toml
[upstream.api]
addresses = ['api.internal:8443']
protocol = 'h2'
tls = { ca = '/etc/muru/internal-ca.pem', server_name = 'api.internal' }

[upstream.grpc_web]
addresses = ['127.0.0.1:9000']
protocol = 'h2c'
```

`h2c` sends HTTP/2 over a plain connection without an upgrade, for
servers that speak it by prior knowledge. `h2` negotiates HTTP/2 through
ALPN over TLS. Both multiplex every request to an address over one
connection. A Unix socket address with TLS needs `server_name`, since
there is no host to check the certificate against.

## Behind a static route, in a traffic policy, as a handler

A static route can fall back to an upstream for what no file answers:

```toml
[[route]]
host = 'shop'
path = '/'
static = '/srv/shop/public'
fallback = { upstream = 'legacy' }
```

A route to an application can give an upstream a share of its requests,
named `upstream:NAME` in the policy's `split`, with the bucketing,
stickiness and preview of the [traffic guide](traffic.md). The sticky
and preview cookies name it the same way:

```toml
[[route]]
host = 'shop'
path = '/'
app = 'shop'
traffic = { split = { 'upstream:legacy' = 90 }, sticky = true }
```

An upstream can be the handler of an [authentication
sub-request](auth-subrequests.md), and an upstream route can be guarded
by one. The handler sees the original request in the `X-Original-Method`,
`X-Original-URI` and `X-Original-Host` headers:

```toml
[[route]]
host = 'shop'
path = '/media/'
static = '/srv/shop/media'
auth = { upstream = 'sso', path = '/check', copy_headers = ['X-Auth-User'] }
```

## Redirects from an upstream

With `trust_muru_headers = true`, an `X-Muru-Redirect` response header
from the upstream is an [internal redirect](internal-redirects.md), and
the target route must list the upstream in `accept_redirects_from`
unless the upstream is the route's own target. Without it, every
`X-Muru-*` header the upstream sends is removed and the response is
served as it is.

## Seeing what is applied

`muructl upstream list` shows each upstream's protocol, TLS, addresses,
probe path and trust; `muructl route list` names upstream targets,
fallbacks and handlers. `muructl config export` writes the tables back.
The access log records a request to an upstream under the upstream's
name as its application, `upstream` as its group and the address that
answered as its mount.
