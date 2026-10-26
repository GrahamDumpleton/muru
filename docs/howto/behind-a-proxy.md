# Behind a load balancer or proxy

```{admonition} Status
:class: note

The `[proxy]` table in the daemon's configuration file, with Forwarded,
X-Forwarded-For, -Proto, -Host, -Port, -Prefix and X-Real-IP. Per-host
and per-listener settings and client certificate headers are still to
come.
```

When requests arrive through a load balancer, a CDN or another proxy, the
peer the edge sees is the proxy, not the client. The daemon's
configuration file names the proxies to trust and which of their headers
to believe:

```toml
[proxy]
trusted = ["10.0.0.0/8", "loopback"]
headers = ["x-forwarded-for", "x-forwarded-proto", "x-forwarded-host"]
recursive = true
forwarded_headers = "keep"
```

Nothing is trusted by default. A forwarding header from a peer that is
not trusted is removed before the request reaches the application, so a
client cannot claim another address by sending X-Forwarded-For itself.

`trusted` lists addresses, CIDR ranges (IPv4 or IPv6), or the words
`loopback`, `private` (RFC 1918, unique local and link-local addresses)
and `unix`. `headers` lists the header types to apply from a trusted
peer: `forwarded` (RFC 7239), `x-forwarded-for`, `x-forwarded-proto`,
`x-forwarded-host`, `x-forwarded-port`, `x-forwarded-prefix` and
`x-real-ip`; the first four are used when the key is omitted. The types
are read in the order listed, and where two name the same thing the first
wins.

## What the application sees

| Forwarded | WSGI | ASGI |
| --- | --- | --- |
| The client address | `REMOTE_ADDR` | `client` |
| The scheme | `wsgi.url_scheme`, `HTTPS` | `scheme` |
| The host | `HTTP_HOST`, and the virtual host chosen | the `host` header |
| The port | `SERVER_PORT` | `server` |
| The path prefix the proxy stripped | `SCRIPT_NAME`, with `PATH_INFO` the rest | `root_path`, with `path` including it |
| The proxy that connected | `muru.proxy_addr` | `extensions["muru"]["proxy_addr"]` |

A request that arrives with none of the headers is unchanged, and
`muru.proxy_addr` is absent.

**Chains.** X-Forwarded-For and Forwarded carry one address per proxy,
the client's first. With `recursive = true` (the default) the chain is
walked from the right past every trusted address to the first one that is
not trusted, which is the client; with `recursive = false` the rightmost
address is taken, which is right when exactly one proxy adds to the
chain. A value that is not an address ends the walk and is never taken as
the client.

**The headers afterwards.** With `forwarded_headers = "keep"` (the
default) the configured headers stay in the request for the application,
and every other forwarding header is removed; with `"consume"` all of
them are removed and the application sees only the resolved values.
Request headers whose names contain an underscore are dropped whatever
the setting, so `X_Forwarded_For` can never reach an application as
`HTTP_X_FORWARDED_FOR`.

**HTTPS.** A request forwarded with `X-Forwarded-Proto: https` counts as
secure: a virtual host that asks for HTTPS does not redirect it, and the
application sees `https`.

A `[proxy]` table in a file given to `muructl apply` is reported and not
applied; it belongs in the daemon's own file, and a daemon whose table
does not parse refuses to start and says which entry is wrong.
