# HTTPS in development

```{admonition} Status
:class: note

Development mode. Under the daemon, certificates belong to virtual hosts
and TLS listeners serve them by name: see
[virtual hosts and TLS](virtual-hosts.md).
```

```
muructl run app:application --https
```

serves HTTPS on port 8443 next to HTTP on 8080. The TLS connection also
carries HTTP/2, which browsers only use over TLS, so this is the way to
test HTTP/2, Secure cookies, `wsgi.url_scheme == "https"` and anything
that behaves differently behind TLS. Every flag on this page has a key in
the project's run table, `https`, `https_only`, `https_port`, `hosts`,
`tls_cert` and `tls_key`, so a project that is always served over TLS
needs no flag; [settings for muructl run](run-settings.md) has the table.

## Certificates

With no certificate given, muructl makes one for `localhost`, `127.0.0.1`
and `::1`, and keeps it in your user cache directory (`~/.cache/muru` on
Linux, `~/Library/Caches/muru` on macOS, or `MURU_CACHE_DIR`) as
`certs/dev.pem` and `certs/dev-key.pem`. It is reused until it is within
thirty days of expiry, the names change, or the authority changes.

**Trusted, with mkcert.** If [mkcert](https://github.com/FiloSottile/mkcert)
is installed and `mkcert -install` has been run, the certificate is issued
by its local authority and browsers and curl trust it with no warning. The
terminal says so:

```
muru: certificate for localhost, 127.0.0.1, ::1 issued by the mkcert authority in /Users/you/Library/Application Support/mkcert (created, /Users/you/Library/Caches/muru/certs/dev.pem)
```

muructl finds the authority through `CAROOT` in the environment, then
`mkcert -CAROOT`, then mkcert's default location.

**Self-signed, without.** Otherwise the certificate signs itself and the
browser warns until it is trusted; the terminal says how:

```
muru: self-signed certificate for localhost, 127.0.0.1, ::1 (created, ...); browsers will warn until it is trusted
muru: for a trusted certificate install mkcert (https://mkcert.dev), run mkcert -install and start again
```

For curl, `--cacert ~/.cache/muru/certs/dev.pem` trusts it directly.

**Extra names.** `--host shop.localhost` adds a name; repeat it for more.
Names ending in `.localhost` resolve to loopback in browsers without any
`/etc/hosts` entry, which is handy for testing subdomains and cookies.

**Your own certificate.** `--tls-cert cert.pem --tls-key key.pem` serves
those files instead, and implies `--https`. A relative path is taken from
the project (`--chdir` or the directory given), not from where `muructl`
runs.

## HTTPS only

```
muructl run app:application --https-only
```

keeps the HTTP listener but answers every plain request with a `307` to
the same path on the HTTPS port. The redirect is temporary and never
cached, so a later run without TLS on the same port is unaffected.

## Ports

`--https-port` changes the port, and `--port auto` picks free ports for
both and prints them.

## What the application sees

Over HTTPS the environ has `HTTPS = "on"`, `wsgi.url_scheme = "https"`,
and `muru.tls` with the protocol version, cipher, the name the client
asked for, the negotiated application protocol and the JA4 fingerprint
of the client's TLS ClientHello; the
[environ reference](../reference/wsgi-environ.md) lists them.

Do not send `Strict-Transport-Security` from the application in
development. Browsers apply it per host name, so an HSTS header on
`localhost` affects every other local server for its `max-age`. Muru warns
once if it sees one.
