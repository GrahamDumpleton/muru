# Virtual hosts and TLS

```{admonition} Status
:class: note

Virtual hosts with exact and single-label wildcard names, a default host,
path-prefix routes to applications, TLS listeners with a certificate per
host chosen by SNI, and HTTPS redirects. Regular expression names,
host-level limits and error pages, and automatic certificates are still
to come.
```

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
The application's serving release takes the request; while the
application has no release serving, the answer is 503 `no_workers`.

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

A replaced certificate file is picked up the next time the daemon pushes
a snapshot: an apply that changes any host or route, or a deploy. Apply
the file again with a changed host to rotate a certificate at once.

With `https_redirect = true`, plain HTTP requests to the host are answered
with 307 to the same host, path and query on the first TLS listener's
port; other hosts are unaffected.

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
