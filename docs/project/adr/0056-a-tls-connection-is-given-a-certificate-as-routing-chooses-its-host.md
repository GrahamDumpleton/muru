---
status: accepted
date: 2026-11-07
---

# ADR 0056: A TLS connection is given a certificate as routing chooses its host

## Context and problem statement

Under the daemon one certificate resolver served every TLS listener,
filled with the certificate of every host. It chose by the name the
client asked for (SNI), then the default host's, then the first host's
(section 4.3). Routing chooses a request's host only among the hosts
that answer on the request's listener, so on a listener a host is not
on, a client could be given that host's certificate and then a 421. A
client connecting by IP address asks for no name, so a host that lists
the address among its names, and is chosen for the request by it, could
not have its certificate chosen for the connection unless it was the
default or the first (issues #456 and #458).

The question is what a TLS connection's certificate is chosen by.

## Decision drivers

- A connection is given the certificate of the host its requests will be
  routed to wherever that can be known before the handshake, so a client
  that checks the certificate is not refused for a host it will reach.

- A site reached by address over HTTPS needs no DNS and no SNI.

- The handshake stays cheap: no server configuration is built for each
  connection, and certificates still rotate live with each snapshot.

- `muructl run`, with its one certificate, is not changed.

## Considered options

- Keep the one resolver, which sees only the ClientHello.

- Read the ClientHello before choosing a configuration, so the choice
  can use the connection's listener and the address it arrived on.

- Per-listener default hosts in configuration.

## Decision outcome

Chosen option: read the ClientHello first, because it is the only one
that has the listener and the connection's own address at hand, and it
needs no new configuration. The edge accepts TLS under the daemon
through a lazy acceptor, after the JA4 fingerprint is read as before,
and chooses among the hosts with a certificate that answer on the
listener, in this order:

1. With a name: the exact name, then the longest wildcard, compared as
   routing compares them.

2. With no name: a host whose names include the address the connection
   arrived on (the socket's local address, which on a listener bound to
   `0.0.0.0` or `::` is the one the client used). Addresses are compared
   as addresses, so `[::1]` and an IPv4-mapped IPv6 address match.

3. The default host, if it answers on the listener.

4. The first host on the listener.

5. None, and the handshake fails.

A server configuration is built for each certificate when a snapshot
arrives, with the same protocols as before, and the snapshot's set
replaces the last whole, as the resolver's table did. There is still at
most one default host; per-listener defaults are left out.

### Consequences

- Good: a host that names an address serves HTTPS by that address with
  its own certificate, and a host limited to some listeners is never
  offered on the others.

- Good: the certificate a client is given is the one of the host its
  requests reach, unless a request's Host differs from the name the
  client asked for in its handshake.

- Bad: a listener none of whose hosts has a certificate now fails the
  handshake instead of presenting another listener's host's
  certificate, which a client accepting any certificate used to get
  through.

## More information

Section 4.3 of the specification states the order. Issue #456 fixed the
case of no name at all; issue #458 widened the choice to the listener
and the local address.
