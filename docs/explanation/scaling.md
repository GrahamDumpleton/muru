# How scaling decides

A worker group's processes are fixed unless the group has a scaling
policy. With one, the daemon decides each second how many workers the
group should have, and the [scaling guide](../howto/scaling.md) has the
settings that shape the decision. This page explains what the daemon
looks at, how it turns that into a count, and what can override it.

Adding a worker costs a fork from the release's preloaded host, so a new
worker is serving within seconds. Removing one drains it: the edge stops
giving it requests, the ones it has finish, and it exits. The decisions
below are built around that difference: the daemon grows a group as soon
as the need holds for a few seconds, and shrinks it only after a long
quiet spell, one worker at a time.

## Capacity is counted in slots

A worker offers slots, the requests it can run at once: its threads for
WSGI, its event loop's concurrency for ASGI. The edge knows, for each
group and each second, how many slots are busy, how many requests are
waiting for one, and how long requests waited before a slot took them.
That is what drives scaling.

The daemon turns slots into worker equivalents: free slots divided by the
slots one worker offers. Workers still starting count as capacity on the
way, so a group that has just asked for two more does not ask again
while they fork, and requests waiting at the edge count as slots wanted,
so a group with a queue is short by at least that much.

## Growing

Each mode has its rule for when the group is short:

- **Spare mode** wants at least `min_spare` idle worker equivalents.
  When fewer remain for the whole of `scale_out_window`, five seconds by
  default, the group grows by what is missing.

- **Target mode** wants the share of busy slots at `slot_utilization`,
  and the 95th percentile of the queue wait under `queue_wait_p95` when
  one is set. When either has been exceeded for the window, the group
  grows to the count that would carry the load at the target.

A step is never more than `max_step_up` workers, four by default, and
never past `max_workers`. While requests are queuing the group is
saturated, and busy slots cannot say how much more is wanted, since
they cannot be more than all of them; so each step that follows soon
after the last doubles the one before, within the same bounds, until the
queue clears.

## Shrinking

A worker is drained only when every condition holds:

- over the whole `scale_in_window`, ten minutes by default, the group
  had more capacity than it needed at every moment: more than
  `max_spare` idle worker equivalents in spare mode, or a peak that fewer
  workers could have carried at the target in target mode; in spare mode,
  ten minutes with no request at all (`idle_reap_after`) also counts;

- no scale-out happened within `hold_after_growth`, five minutes;

- no other worker was drained within `reap_interval`, a minute;

- the group stays at or above `min_workers`;

- the worker drained is older than `min_lifetime`, fifteen minutes or
  thirty times the time a worker took to become ready, whichever is
  longer.

Judging on the peak over a long window, rather than the level now, means
a lull between bursts does not shrink a group that the next burst will
need again. The gap between `min_spare` and `max_spare`, at least two
workers, means the rule that grows a group and the rule that shrinks it
cannot chase each other.

Each release also has a churn budget, six drains per hour by default
across its groups. When it is spent, shrinking stops until the hour has
passed, a `churn_budget.exhausted` warning is raised, and the scale-in
window doubles until no drain remains in the last hour. Timeout kills and
recycling do not count against it.

## Floors, ceilings and overrides

The policy's `min_workers` and `max_workers` are the group's floor and
ceiling, and several things can change them for a while:

- **A schedule** raises the floor, and optionally the ceiling, for a
  window, with a warm-up lead so the workers are there before the load.
  When schedules or overlays overlap, the highest floor and the highest
  ceiling apply, and any of them can hold scale-in off for the window.
  The climb to a floor happens in batches of `max_step_up`.

- **A rollout or a split** gives each release its share of the floor,
  rounded up, so a canary at 10% does not hold a full floor of workers.

- **`muructl scale --min`** sets a temporary floor, until a time.

- **A pin**, `muructl scale --processes`, fixes the count and the
  autoscaler stops deciding for the group until it is unpinned.

- **Advisory mode** lets the autoscaler decide and report what it would
  do, as events, without acting.

## Sharing the node

The daemon file's `[daemon.budget]` caps what the releases on the node
hold between them: worker processes, memory and CPU cores. Every
scale-out, the climb to a schedule's floor among them, is admitted
against what is left, applications with a higher `priority` first, and
is given as many of the workers it asked for as still fit. A group given
fewer holds where it is and asks again after another scale-out window.
Workers already running are never stopped to make room for another
application. When the node is over its memory budget, retained releases
give way first: they drop to their hosts alone, then stop, oldest first.

## Scaling to zero

A group with `min_workers = 0` drains its last worker by the same slow
rules. The host stays, and the edge holds the next request for the group
as queued while the daemon forks one worker for it, so the first request
after a quiet spell costs a fork. With `host_idle_timeout`, the host also
stops once every group of the release has been at zero for that long,
and the next request waits through the application's import instead. A
request that waits longer than its queue timeout is answered 503.

## The diagnosis

The worker count is only one answer to a busy group. Every
`check_interval` the workers report how they spend their time: CPU,
garbage collection, waits for the GIL and event loop lag. The daemon
combines that with the slot figures into a diagnosis of what holds the
group back: memory, garbage collection, a blocked event loop, the GIL,
CPU, exhausted slots or a few heavy requests. These figures do not drive
the count; the slot and queue rules do. The diagnosis says what to change
in the release itself, which a new count cannot: more threads for a
group waiting on I/O, more processes with fewer threads for one held by
the GIL, a worker group of their own for slow paths. A release's threads
and processes per worker are fixed, so the change is made in the next
release.

Every decision is a `scaling.decision` event with its reasons and the
diagnosis, and `muructl scale status` shows the signals, the diagnosis,
the floor and ceiling in force and what sets them.
