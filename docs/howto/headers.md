# Request and response headers

A virtual host and a route can remove and set headers on the requests
they take, and remove, set and add headers on the responses, without
any change to the applications behind them. The tables go on the
`[host.NAME]` and `[[route]]` entries of an applied file (see [virtual
hosts](virtual-hosts.md)).

## Headers on every response

```toml
[host.shop]
names = ["shop.example.com"]

[host.shop.response_headers]
remove = ["Server", "X-Powered-By"]
add = { Link = ["</site.css>; rel=preload; as=style"] }

[host.shop.response_headers.set]
Strict-Transport-Security = "max-age=31536000; includeSubDomains"
X-Frame-Options = "DENY"
X-Content-Type-Options = "nosniff"
Referrer-Policy = "strict-origin-when-cross-origin"
```

| Key | Effect |
| --- | --- |
| `remove` | the named headers are taken off the response |
| `set` | each header is put on the response, in place of any it has |
| `add` | each header is put on the response beside any it has; the value is a string, or a list for a header sent more than once |

The names are removed first, then the headers set, then those added. The
rules apply to every response to the host's requests, whatever produced
it: an application, an [upstream](upstreams.md), a [static
file](static-files.md), a redirect, or an error the edge answers itself,
such as a 404 for a path with no route or a 503 while the workers are
busy. So the security headers above are on the error pages as well as
the application's pages.

## Headers on the request

```toml
[host.shop]
names = ["shop.example.com"]
request_headers = { remove = ["X-Debug", "Cookie2"], set = { X-Site = "shop" } }
```

| Key | Effect |
| --- | --- |
| `remove` | the named headers are taken off the request |
| `set` | each header is put on the request, in place of any the client sent |

The rules are applied once to a client's request, after the edge has
removed the headers it never passes on and before anything is asked to
serve it. What the application sees, as `HTTP_X_SITE` in a WSGI environ
or `x-site` in an ASGI scope's headers, is the value set here whatever
the client sent. An [authentication handler](auth-subrequests.md), an
upstream and the target of an [internal
redirect](internal-redirects.md) see the same headers. [Rate
limits](client-classes.md) keyed by a header read it as the client sent
it.

## A route's own rules

A route takes the same two tables. The host's rules are applied first
and the route's after them, so the route has the last word:

```toml
[[route]]
host = "shop"
path = "/embed"
app = "shop"
response_headers = { remove = ["X-Frame-Options"] }

[[route]]
host = "shop"
path = "/api"
app = "shop-api"
request_headers = { set = { X-Site = "shop-api" } }
response_headers = { set = { Cache-Control = "no-store" } }
```

Responses under `/embed` carry the host's other headers and no
`X-Frame-Options`. Requests under `/api` reach the application with
`X-Site: shop-api`, and their responses carry `Cache-Control: no-store`
beside the host's headers. The rules are those of the host and the route
the client's request matched: a request an application redirects
internally keeps them.

An [overlay](overlays.md) changes a route's tables for as long as it is
active, with `routes.NAME.request_headers` and
`routes.NAME.response_headers`, each in place of the route's own table.

## What cannot be named

Header names are matched without regard to case. A table that names one
of these is refused when the file is applied, since the edge handles
them itself:

- `Host`, `Content-Length` and `Transfer-Encoding`;
- the headers that describe one connection: `Connection`, `Keep-Alive`,
  `Proxy-Authenticate`, `Proxy-Authorization`, `Proxy-Connection`, `TE`,
  `Trailer` and `Upgrade`;
- any header starting with `X-Muru-`.

A request header whose name contains an underscore cannot be set, since
the edge passes no such header to an application. Values are printable
ASCII. The edge puts `X-Request-ID` on every response after the rules
are applied, so it is always there.

`muructl host list` and `muructl route list` show what each host and
route removes, sets and adds.
