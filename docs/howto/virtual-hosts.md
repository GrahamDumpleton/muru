# Virtual hosts and TLS

The daemon serves several applications on one node by name. Virtual
hosts and routes are declared in files and applied with `muructl apply`,
alongside the applications they route to (see
[the daemon](daemon.md) for applying files).

## Listeners

The daemon's own configuration file names the addresses to serve, each
with or without TLS:

```toml
[[listener]]
address = "0.0.0.0:80"

[[listener]]
address = "0.0.0.0:443"
tls = true
```

Every host answers on every listener unless its `listeners` key names
some. A TLS listener has no certificate of its own: it serves the
certificates of the virtual hosts, chosen by the name the client asks
for. Without any listener, the daemon serves plain HTTP on 127.0.0.1:8080,
and `muructl daemon run --listen` serves plain HTTP on one address alone.

## Hosts and routes

```toml
[host.shop]
names = ["shop.example.com", "*.shop.example.com"]
tls = { certificate = "certs/shop.pem", key = "certs/shop-key.pem" }
https_redirect = true

[host.blog]
names = ["blog.example.com"]
tls = { certificate = "certs/blog.pem", key = "certs/blog-key.pem" }

[host.catchall]
default = true

[[route]]
host = "shop"
app = "shop"

[[route]]
host = "shop"
path = "/api"
app = "shop-api"

[[route]]
host = "blog"
app = "blog"

[[route]]
host = "catchall"
app = "landing"
```

A host's `names` are exact names or `*.` wildcards matching one label.
Names are compared without regard to case, port or a trailing dot, and
the same name cannot belong to two hosts. A request's host selects the
virtual host by exact name first, then by the wildcard with the longest
suffix, then the listener's default host (the one with `default = true`,
at most one). A request whose name no host claims is refused with 421
Misdirected Request, so a forged Host header reaches no application. The
same happens to a request with no Host header at all. A host with no
`names` must be the default.

Routes belong to a host and name a path prefix and an application. The
longest prefix wins, matched on segment boundaries: `/api` covers `/api`
and `/api/users` and not `/apiary`; `/` covers everything. A request
whose host has no route for its path gets 404 with the reason `no_route`.
The application's serving release takes the request, unless the route
has a `traffic` table sharing its requests between releases (see
[traffic between releases](traffic.md)); while the application has no
release serving, the answer is 503 `no_workers`. A route may carry a
`name`, unique among routes and made of lower case letters, digits and
hyphens, by which [overlays](overlays.md) change it.

The application sees the name the request matched with as `SERVER_NAME`
(ASGI: the `server` tuple), or the host's `canonical` name when
`use_canonical_name` is true; `HTTP_HOST` is always what the client sent.

Without any host declared, every request on every listener goes to the
application traffic was last switched to, which is how a daemon serving
one application works.

## TLS

A host's `tls` table names its certificate chain (leaf first) and private
key as PEM files; relative paths are taken from the file's directory. The
daemon reads them when the host is applied, refuses the apply when they
cannot be read or do not go together, and sends them to the edge with its
routing snapshot, so no private key is read by the edge itself. A TLS
listener presents the certificate of the host whose name the client asks
for (exact, then wildcard); a client asking for no name, or a name no
host with a certificate claims, is given the default host's certificate,
or the first host's. A host without a `tls` table still answers on a TLS
listener under another host's certificate when the client accepts it.

A replaced certificate file is picked up by `muructl daemon reload` (or
SIGHUP), by an apply of a file that declares the host, changed or not,
and by anything else that sends the edge a snapshot, such as a deploy.
The daemon reads every TLS host's files again, keeps each pair that is
whole and goes together, and sends the edge those that changed, which new
connections get at once. A file that cannot be read, or a key that does
not go with its certificate, leaves the host on the certificate it had,
which the reload or apply says and a `certificate.unreadable` event
records. So a renewal that runs a hook needs one command, certbot's
deploy hook for one:

```
certbot renew --deploy-hook 'muructl daemon reload'
```

Under systemd, `systemctl reload muru` does the same; the [systemd
guide](systemd.md) has the unit.

With `https_redirect = true`, plain HTTP requests to the host are answered
with 307 to the same host, path and query on the first TLS listener's
port; other hosts are unaffected.

## Settings for a host's routes

A host carries settings for every route it has, written once:

```toml
[host.api]
names = ["api.example.com"]
max_body_size = "1MB"
timeouts = { request_deadline = "30s", queue_timeout = "2s" }
access_log = "errors"
rate_limits = ["crawlers"]

[host.api.error_pages]
503 = { file = "pages/busy.html" }

[[route]]
host = "api"
app = "api"

[[route]]
host = "api"
path = "/import"
app = "api"
max_body_size = "200MB"
timeouts = { request_deadline = "120s" }
```

| Key | Sets for the host's requests | A route's own |
| --- | --- | --- |
| `max_body_size` | the largest request body | replaces it |
| `timeouts` | `queue_timeout`, `request_deadline`, `body_idle_timeout` and `response_idle_timeout`, as [timeouts](timeouts.md#under-the-daemon) describes | replaces it key by key |
| `access_log`, `slow`, `access_log_exclude` | which requests the [access log](logging.md#what-the-daemons-access-log-writes) writes | |
| `access_log_format`, `access_log_file` | the [format of the host's records and the file](logging.md#the-format-and-a-file-for-a-host) that takes them | |
| `compression` | how [responses are compressed](compression.md) | replaces it key by key |
| `request_headers`, `response_headers` | the [headers](headers.md) removed, set and added | are applied after them |
| `error_pages`, `error_override` | the [error pages](error-pages.md) | is consulted first |
| `rate_limits` | the [rate limits](client-classes.md) every request draws from | are drawn from as well |
| `maintenance` | what the host's [maintenance mode](error-pages.md#a-whole-virtual-host) lets through | |

A host's limits are a site's defaults: an application that [sets its
own](timeouts.md#with-the-application) in its release keeps them, and a
route replaces both. Here every request for `api.example.com` may carry
1MB and take 30 seconds, except under `/import`, where the route allows 200MB and 120
seconds and keeps the host's 2 second queue timeout. A key the host
leaves out is the daemon's own: its `[limits]` and `[logs]` tables, or
the default. `muructl host list` shows what each host sets, and
`muructl config show --effective` lists every route's limits with the
level each came from:

```
#   route api /: max_body_size = 1MB (host api)
#   route api /import: max_body_size = 200MB (the route)
#   route api /import: timeouts.queue_timeout = 2s (host api)
```

## The canonical name

A host with several names can send every request to one of them:

```toml
[host.shop]
names = ["example.com", "www.example.com"]
canonical = "www.example.com"
canonical_redirect = true
https_redirect = true
```

With `canonical_redirect = true`, a request for any name other than the
`canonical` one is answered with 307 to the canonical name, with the
same path and query, and the port the request came to when it is not
the scheme's own. A request that must also move to HTTPS is sent there
in the same redirect:

| Request | Answer |
| --- | --- |
| `https://example.com/a?b=1` | 307 to `https://www.example.com/a?b=1` |
| `http://example.com/a?b=1` | 307 to `https://www.example.com/a?b=1` |
| `https://www.example.com/a?b=1` | served |

Every name the host matches is redirected, a wildcard's included, so a
host that serves a different site under each name leaves it off. The
`canonical` name need not be one of the host's `names`: a host for a
retired name redirects to wherever the canonical name is served.

## Commands

```
muructl apply -f config/
muructl host list
muructl route list
muructl config export
```

`host list` and `route list` show what is applied. `config export`
writes the hosts and routes with the applications, and `--app NAME`
writes only the routes to that application and their hosts. `apply
--prune` removes hosts and routes the files no longer declare, routes
first.
