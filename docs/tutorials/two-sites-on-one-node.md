# Two sites on one node

```{admonition} Status
:class: note

This tutorial assumes you have done
[deploy with the daemon and roll back](deploy-and-roll-back.md). It uses
[mkcert](https://github.com/FiloSottile/mkcert) for certificates your
browser and `curl` trust.
```

One daemon can serve several sites, each chosen by the name a request
asks for, over plain HTTP and HTTPS, with a certificate per site. Not
every site has to be a Python application: the edge can serve a
directory of files itself, or pass requests to a server you run some
other way.

In about fifteen minutes you will serve three sites from one daemon: a
shop that is a Python application, a blog that is a directory of HTML,
and a notes site served by a separate HTTP server. Each will have its own
name and HTTPS, the shop will send plain HTTP visitors to HTTPS, and a
name the daemon does not know will reach none of them.

## The node

Make a directory for everything this node serves, with the runtime
library for the shop:

```
mkdir node && cd node
uv init --bare
uv add muru
mkdir shop blog notes certs
```

Save the shop as `shop/app.py`:

```{literalinclude} code/node/shop/app.py
:language: python
```

the blog as `blog/index.html`:

```{literalinclude} code/node/blog/index.html
:language: html
```

and the notes as `notes/index.html`:

```{literalinclude} code/node/notes/index.html
:language: html
```

## Listeners

Which addresses the daemon listens on, and which of them speak TLS, is
the daemon's own configuration. Save this as `node.toml`:

```{literalinclude} code/node/node.toml
:language: toml
```

The daemon reads its listeners when it starts, so stop the one from
the earlier tutorials and start it with this file:

```
muructl daemon stop --shutdown
muructl daemon start --config node.toml
```

```
muru: daemon started (pid 42791), serving on http://127.0.0.1:8080 and https://127.0.0.1:8443, log /Users/you/Library/Logs/muru/daemon.log
```

## Certificates

Install mkcert's local certificate authority once, if you have not, and
make one certificate for the three names:

```
mkcert -install
mkcert -cert-file certs/sites.pem -key-file certs/sites-key.pem shop.localhost blog.localhost notes.localhost
```

Names under `.localhost` mean this machine to `curl` and to most
browsers, so they reach the sites with no change to your system.

## The shop and the notes server

Deploy the shop:

```
muructl deploy shop shop --entrypoint app:application --python .venv/bin/python
```

and, in another terminal, start the notes server, standing in for any
HTTP service you already run:

```
uv run python -m http.server 9001 --bind 127.0.0.1 --directory notes
```

## Hosts and routes

Now tell the daemon which name goes where. Save this as `sites.toml`
in `node`:

```{literalinclude} code/node/sites.toml
:language: toml
```

Each `[host]` is a site: the names it answers to and its certificate.
Each `[[route]]` sends a host's requests somewhere: to an application, to
a directory the edge serves itself, or to an `[upstream]`, an HTTP server
Muru does not manage. Relative paths, the certificate's and the `blog`
directory's, are taken from the file's directory. Apply it:

```
muructl apply -f sites.toml
```

```
muru: applied
  create-host     host blog                created
  create-host     host notes               created
  create-host     host shop                created
  create-upstream upstream notes           created
  create-route    route shop /             created
  create-route    route blog /             created
  create-route    route notes /            created
```

## Visit the sites

```
curl https://shop.localhost:8443/
curl https://blog.localhost:8443/
curl https://notes.localhost:8443/
```

```
The shop, at https://shop.localhost
<!doctype html>
<title>The blog</title>
...
<!doctype html>
<title>Notes</title>
...
```

The edge picked each site's certificate by the name the client asked
for, then the route by the same name. The shop sees the name it was
reached by as `SERVER_NAME`, and the scheme as `https`. Open the three
addresses in a browser too: they are trusted, and spoken over HTTP/2.

Plain HTTP works for the blog and the notes, but the shop has
`https_redirect = true`:

```
curl -i http://shop.localhost:8080/cart
```

```
HTTP/1.1 307 Temporary Redirect
location: https://shop.localhost:8443/cart
```

A request for a name no site claims reaches none of them:

```
curl -i -H 'Host: other.example' http://127.0.0.1:8080/
```

```
HTTP/1.1 421 Misdirected Request
```

## When the notes server is down

Stop the notes server with Ctrl-C and ask again:

```
curl -i http://notes.localhost:8080/
```

```
HTTP/1.1 502 Bad Gateway

No address of the upstream could be reached: Connection refused (os error 61).
```

The other two sites carry on. An upstream can list several addresses,
which take requests in turn, and a `health` table probes them so a
failed one is left out until it answers again.

## What the daemon holds

```
muructl host list
muructl route list
muructl upstream list
```

```
HOST             TLS      LISTENERS  NAMES
blog             yes      all        blog.localhost
notes            yes      all        notes.localhost
shop             yes      all        shop.localhost
```

and `muructl config export` writes the hosts, routes and upstream beside
the shop's release, as one file to keep.

## Clean up

```
muructl daemon stop --shutdown
```

The next `muructl daemon start` without `--config` uses the default
listener, plain HTTP on port 8080, but keeps the hosts and routes you
applied. `muructl apply --prune` with a file that leaves them out
removes them.

## From here

- [Virtual hosts and TLS](../howto/virtual-hosts.md) covers wildcard
  names, a default host and certificate rotation.

- [Static files](../howto/static-files.md) covers cache headers, a
  fallback to an application and single-page applications.

- [HTTP upstreams](../howto/upstreams.md) covers several addresses,
  health checks, HTTP/2 and TLS to an upstream.

- [Behind a proxy](../howto/behind-a-proxy.md) is for a node behind a
  load balancer that terminates TLS itself.
