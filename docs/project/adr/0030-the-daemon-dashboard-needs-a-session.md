---
status: accepted
date: 2026-10-27
---

# ADR 0030: The daemon's dashboard needs a session for every page

## Context and problem statement

Section 10.7 made the daemon's dashboard read-only by default and open to
anyone who can reach its address, relying on the address being loopback,
with sign-in only for the mutating actions, which configuration enables.
Loopback is not a boundary on a machine shared by users: any process on
the host can reach the address, and that includes the applications,
which run as users of their own (ADR 0026). One application's code could
read another's recent request paths with their query strings and the
tails of its log through the pages and the JSON gateway.

The dashboard is also wanted behind a proxy that serves it under a path
prefix, such as jupyter-server-proxy in a workshop, which the absolute
addresses the pages used and the action check's Origin comparison with
the Host header did not allow.

## Decision drivers

- Only whoever may use the admin socket should see what the dashboard
  shows, as only they may change it.

- Looking and acting are different powers, and a session should carry
  only the one asked for.

- Prometheus cannot sign in, and scripts reading the JSON gateway should
  not need a browser.

- The dashboard should work under a path prefix with no setting for the
  address it is reached at.

## Considered options

- Keep the pages open and sign in only to act.

- Require a session for every page, with the actions a property of the
  session asked for when signing in.

- Serve the dashboard through the edge on a public host behind sign-in.

## Decision outcome

Chosen option: a session for every page under the daemon. `muructl
dashboard login` gives a link for a session that may look, and with
`--actions` one that may act, which the `[dashboard]` table's `actions =
true` must allow; `--token` gives a session at once as a bearer token for
scripts. `/metrics` and the vendored assets need no session. `muructl
run`'s dashboard stays open, being for development on the developer's
own machine.

The pages use relative addresses, redirects are relative, and the session
cookie carries no Path, so the dashboard works under any prefix it is
served at. An action's same-origin check uses the browser's
Sec-Fetch-Site where it is sent, which a proxy does not change, falling
back to comparing the Origin with the Host. `--url` or
`MURU_DASHBOARD_URL` prints the login link under the address the
dashboard is reached at; the daemon is never told it.

Serving the dashboard through the edge is not offered: reaching it from
another machine is an SSH tunnel or a proxy of the operator's own.

### Consequences

- Good: the dashboard shows nothing to a process that cannot use the
  admin socket, whichever user it runs as.

- Good: a session for looking cannot act, whoever obtains its cookie.

- Good: a workshop or a tunnel can serve the dashboard under a path with
  nothing configured on the daemon.

- Bad: scripts that read the JSON gateway with plain requests need a
  token, which lasts twelve hours and ends with the daemon.

- Neutral: under another application's origin, such as Jupyter's, other
  pages of that origin are same-origin with the dashboard; the guidance
  stays an SSH tunnel for anything but a workshop.

## More information

Section 10.7 of the design specification; ADR 0026 on the users
applications run as.
