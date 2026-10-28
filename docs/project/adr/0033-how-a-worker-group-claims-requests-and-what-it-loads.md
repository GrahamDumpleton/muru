---
status: accepted
date: 2026-10-28
---

# ADR 0033: How a worker group claims requests, when a release may be ready without it, and what it loads

## Context and problem statement

Section 7.1 gives worker groups three things the first alphas did not
build: match lists with host, method and header conditions beside the
path prefixes, optional groups a release is ready without, and a
`mounts` subset so a group's workers load only some of the release's
mounts. Groups today take paths and, under the daemon, client classes
(section 4.14), every group must have its workers listening before the
release is ready, and every worker loads every mount. The three share
one question, how a group claims a request and what happens when the
group it claims has nothing to serve it with, so they are decided
together.

## Decision drivers

- A request must have exactly one group, found by a rule an operator can
  predict from the configuration alone.

- A group exists for sizing and isolation, so a request it claims must
  not quietly go elsewhere when the group is short of workers: that
  would hide the shortage behind the default group's capacity.

- What a request's headers mean is decided in one place, the client
  classifier of section 4.14; a second rule language on groups would
  drift from it.

- Mounts share one preload (section 6.7), and the host's import is what
  fails early when an entrypoint is broken; a group's subset must not
  move imports into the workers.

- Nothing a group loads or leaves out may be found out at the first
  request: what can be checked when the configuration is read, is.

## Considered options

For the match conditions:

- Host, method and header conditions, as section 7.1 listed.

- Host and method conditions only, with headers expressed as a client
  class and matched by `client_class`.

For an optional group with no workers:

- Hold its requests as for a group at zero workers (section 7.4), until
  a worker arrives or the queue timeout passes.

- Send its requests to the default group meanwhile.

For the `mounts` subset:

- Leave the unlisted mounts out of the worker entirely, importing only
  the group's own after the fork.

- Import every mount in the host as now, and have a group's workers
  realise, start and serve only the listed ones, with the default mount
  always among them.

## Decision outcome

**Match conditions: host and method, not headers.** A group's `match`
table takes `client_class`, `host` and `method` lists; a request matches
a group when its path is under one of the group's paths, or any path for
a group with none, and every list the group gives names the request's
class, host or method. `host` names a virtual host or one of the names
it answers to, or in development mode the name the request carries;
`method` is compared uppercased. Header conditions are not offered: a
header rule is written once as a client class and the group names the
class. Groups rank by how much they ask of a request: a group with more
match lists comes before one with fewer, and among groups asking the
same, the longer path prefix wins, which keeps today's rule that a
class's group comes ahead of the groups for every class. Two groups
asking the same of the same path are refused.

**Optional groups: hold, never fall through.** `optional = true` marks a
group the release is ready without: `release.ready` needs the default
group and every required group listening, and an optional group's
workers join the release's sockets as each listens, as a scale-out's
do. While it has none, its requests wait as for a group at zero
workers, counted as queued, and fail with 503 at the queue timeout.
They are not sent to the default group: the paths were given to the
group for its sizing or isolation, and serving them from elsewhere
would hide that the group is down. The events of a worker that fails
to start say so, and `muructl release show` and the dashboard name the
group optional and its workers.

**The `mounts` subset: served by the workers, imported by the host.** A
group's `mounts` lists the named mounts its workers serve; the default
mount, which is the release's entrypoint and carries its hooks, is
always served. The host imports every mount before the fork as now, so
a broken entrypoint still fails the release early and the preload stays
shared; a worker of the group realises, starts the lifespan of, and
declares in HELLO only its own mounts. With per-worker imports, a
worker imports only its own. A group's paths must lie within mounts it
serves: for each path of the group and each named mount it leaves out,
the configuration is refused when the mount's path is under the group's
path or the group's path under the mount's, since a request for that
path could reach a worker without its mount. A worker asked for a mount
it does not serve, which the check makes impossible from the
configuration, answers 503 `mount_not_loaded` so that the edge's
reason names the fault.

### Consequences

- Good: one rule orders groups whatever they ask, and an operator can
  read it off the configuration.

- Good: a group's shortage shows as queueing and 503s on its own paths,
  in its own metrics, rather than as load on the default group.

- Good: the host's preload and early failure are unchanged by the
  subset, and an unlisted mount costs a worker nothing after the fork.

- Bad: a group cannot condition on a header without a client class
  being declared for it, which is a second table to write.

- Bad: an optional group's paths are unavailable while it starts, which
  a client sees as slow then failing requests rather than as the default
  group's answer; that is the trade for the isolation.

- Neutral: a group that lists mounts must claim paths inside them, so
  `paths = ['/']` with a subset is refused; a group for every path loads
  every mount.

## More information

Sections 4.14, 6.7, 7.1 and 7.4 of the design specification, amended by
the records of each of the three as it is built: optional groups first,
then the match conditions, then the subset.
