---
status: accepted
date: 2026-10-25
---

# ADR 0026: Nothing but the daemon runs as root

## Context and problem statement

Section 12 says that only the daemon may run as root, that edges run
unprivileged with their listening sockets passed in, and that each
application runs as its own user. Section 10.9 gives applications a
`user`, and the first implementation of it (phase 2) switched a host to
that user when one was named. Nothing else switched: a daemon started as
root, by the shipped systemd unit for one, spawned the edge as root and
ran every application that named no user as root. The specification
stated the property and the implementation plan (section 14) never
listed the work, and no default unprivileged account was named anywhere.
Running a root daemon out of the box must not put client parsing or
application code in a root process.

## Decision drivers

- A root daemon is a supported way to run (systemd, low ports,
  per-application users), so its defaults decide what most installations
  get.

- The edge parses hostile traffic and the workers run application code;
  neither has any need of root once the daemon has bound the listeners.

- Nothing magic: Muru creates no accounts and guesses no users. What
  runs as whom is stated and visible.

- Someone who installs by hand, without the packages, must find out at
  once what is missing and what to do, not discover it in a log later.

## Considered options

- Keep root as the default for anything that names no user, and document
  `user` as the remedy.

- Fall back to `nobody` when no account is configured.

- Create a `muru` account at first start when missing.

- Require a service user, `muru` unless another is named, run the edge
  and every application that names no user as it, and refuse to start
  without it.

## Considered options for `user = 'root'`

- Refuse it.

- Allow it, warning each time a release starts.

## Decision outcome

Chosen: the service user, required. A daemon that is root looks the
service user up at start, from `--user`, else the `[daemon]` table's
`user`, else `muru`. If the account does not exist, or is root, the
daemon does not start, and the message says the three ways out: create
the account (`useradd --system --no-create-home muru`, or the sysusers.d
entry the repository ships for the packages), name another account, or
run the daemon as that account in user mode. Muru never creates the
account.

The edge runs as the service user. It binds its control and
listener-return sockets in a directory of its own under the runtime
directory, which the daemon gives to that user, and writes the access
log, which the daemon creates and gives to that user, and recreates with
the same owner after each rotation. Certificates reach the edge as
contents in the snapshot, not as files, so it reads nothing as root.

An application that names no user runs as the service user. One that
names a user runs as that user, as before. Each release's socket
directory belongs to the application's user with the service user's
group, set-group-id so the sockets bound in it carry that group, and
opens to the owner and the group alone; a worker sets its socket's mode
to open to the group whatever the application's umask leaves. The edge
reaches the sockets through the group, and nothing else does.

`user = 'root'` is allowed, because an operator may have a reason and
the setting is explicit, and it is warned about: an `app.user_is_root`
warning event is recorded each time such a release starts, saying that a
compromised application would have the node.

`nobody` was rejected as a fallback because it is shared with other
software, so it isolates the edge and the applications from nothing.
Creating the account was rejected because software that creates
accounts at run time is unusual, distribution policy frowns on it, and
it hides from the operator what runs as whom.

### Consequences

- Good: a root daemon never runs client parsing or application code as
  root unless an application says `user = 'root'`, and then it says so
  in the events.

- Good: an installation that is missing the account fails at start with
  the remedy in the message, not later.

- Bad: a hand installation under a root unit needs one `useradd` before
  it starts, where before it started at once.

- Bad: the service user must be able to read each application's source
  and interpreter, and the directories static routes serve; an
  application whose files are private to its own user needs `user` set
  to that user.

- The daemon itself keeps root and all of its capabilities for now;
  dropping those it does not need (binding low ports, changing user and
  group, giving files away, signalling) is the next step on the same
  path, listed in the roadmap.

## More information

Sections 10.9, 12 and 13.3 of the specification; the [per-application
users guide](../../howto/users.md) and the [systemd
guide](../../howto/systemd.md); `packaging/sysusers.d/muru.conf`.
