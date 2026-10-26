# Scaling

```{admonition} Status
:class: note

The spare and target modes with their windows and churn budget, pins,
scheduled capacity, the workers' CPU, GIL wait and event loop lag with
a diagnosis of each group, `muructl scale status` and the node's budget
across applications, under the daemon. Scale to zero is still to come.
```

A worker group has a fixed number of processes unless it is given a
scaling policy. With one, the daemon watches what the edge reports for the
group each second, forks more workers when they are wanted and drains them
slowly when they are not. Workers fork from the preloaded host, so a new
one costs a fork rather than an import and is serving within seconds.

## Spare mode

Spare mode keeps some idle workers in hand, like Apache's prefork module:

```toml
[tool.muru]
entrypoint = "shop.wsgi:application"
processes = 2
threads = 4

[tool.muru.scaling]
mode = "spare"
min_workers = 2
max_workers = 12
min_spare = 1
max_spare = 4
```

The group starts with `processes` workers, never fewer than `min_workers`.
Idle workers are counted in worker equivalents: free slots (threads, or an
event loop's concurrency) over the slots a worker offers, with workers
still starting counted as capacity on the way and requests waiting at the
edge counted as slots wanted. When fewer than `min_spare` idle workers
remain for `scale_out_window` (5 seconds by default), the group grows by
what is missing, up to `max_step_up` (4) at a time and never past
`max_workers`. While requests queue at the edge the group is saturated and
the signals cannot say how much more is wanted, so each step soon after
the last doubles the previous one, still bounded by `max_step_up`.

Scale-in is slow on purpose. One worker is drained when, over the whole
`scale_in_window` (10 minutes), more than `max_spare` workers were idle at
every moment, or no request arrived for `idle_reap_after` (10 minutes).
Never within `hold_after_growth` (5 minutes) of a scale-out, never more
than one per `reap_interval` (60 seconds), never below `min_workers`, and
never a worker younger than `min_lifetime` (15 minutes, or 30 times the
measured time to ready, whichever is longer). `max_spare` must be at least
`min_spare + 2`, so the two thresholds cannot chase each other.

## Target mode

Target mode tracks how busy the group's slots are:

```toml
[tool.muru.scaling]
mode = "target"
min_workers = 1
max_workers = 8
slot_utilization = 0.6
queue_wait_p95 = "100ms"
```

The group grows when more than `slot_utilization` of its slots (70 percent
by default) have been busy for `scale_out_window`, or when the 95th
percentile of the time requests waited for a worker over that window is
above `queue_wait_p95`, to the count that would carry the load at the
target. It shrinks by one worker when the peak over `scale_in_window`
could be carried by fewer workers at the target, under the same slow
rules as spare mode.

## The churn budget

Each release has a `churn_budget` of reaps per hour (6 by default),
counted across its groups' scale-ins. When it is spent, scale-in is
suspended until the hour passes, a `churn_budget.exhausted` warning is
raised, and the scale-in window is doubled until no reap remains in the
last hour. Timeout kills and recycles do not count against it.

## Groups and the declared release

A policy on the release applies to its default group. Each named group has
its own, static unless given one:

```toml
[app.shop.release.r42.scaling]
mode = "spare"
max_workers = 12

[app.shop.release.r42.groups.reports]
paths = ["/reports/*"]
processes = 1
threads = 1
scaling = { mode = "target", max_workers = 6, slot_utilization = 0.8 }
```

A declared table replaces the source's `[tool.muru.scaling]`. The spare
and target modes need `max_workers`; `min_workers` defaults to 1 and
cannot be 0 yet; `processes` must not be over `max_workers` and is raised
to `min_workers` when below it. In static mode the table takes no other
key, so a key that does nothing is never written by mistake. `muructl
config export` writes the resolved policy with every key.

## Scheduled capacity

Schedules on an application keep capacity ready for load that is known
in advance:

```toml
[[app.shop.schedule]]
name = 'black-friday'
start = '2026-11-27T00:00'
end = '2026-11-30T23:59'
timezone = 'America/New_York'
min_workers = 24
max_workers = 48
warm_up = '30m'
scale_in = 'hold'
freeze_deploys = true

[[app.shop.schedule]]
name = 'weekday-mornings'
cron = '0 8 * * mon-fri'
duration = '4h'
timezone = 'Australia/Sydney'
min_workers = 6
```

| Key | Effect |
| --- | --- |
| `name` | The schedule's name, unique within the application |
| `start`, `end`, or `cron`, `duration`, with `timezone` | The window, one-off or recurring, read as an [overlay's schedule](overlays.md#activating) is |
| `group` | The worker group; the default group without it |
| `min_workers` | The floor: the group has at least this many workers during the window |
| `max_workers` | The ceiling: the autoscaler may grow the group this far during the window |
| `warm_up` | How long before each window the floor already applies, such as `30m` |
| `scale_in` | `hold` keeps the group from scaling in during the window; `normal`, the default, leaves it to the autoscaler |
| `freeze_deploys` | Refuse deploys and rollouts of the application during the window (see [deploy freezes](rollouts.md#deploy-freezes)) |

The group grows to the floor in batches of `max_step_up` workers, each
once the batch before it is ready. A floor above the group's
`max_workers` raises the ceiling with it, and a static group's count
rises to the floor. When schedules overlap, the highest floor and the
highest ceiling apply, and scale-in is held if any of them holds it.
After the window, the group scales in by the autoscaler's usual slow
rules, and a static group goes back to its count one worker per reap
interval. While a canary or a traffic split gives a release part of the
application's requests, the release gets that share of the floor,
rounded up.

Each schedule is an [overlay](overlays.md) named `APP.NAME`, so `muructl
overlay list` shows it with its next window, and `muructl overlay
activate` and `end` start or stop it by hand. The events are
`schedule.activated` and `schedule.expired`, and the floor's scale-outs
are `scaling.decision` events. An `[overlay.NAME]` table sets the same
capacity on an application with `apps.NAME = { min_workers = 24,
max_workers = 48 }`, beside its other changes.

A floor for a while, without declaring a schedule:

```console
$ muructl scale shop --min 8 --for 3h
muru: shop default kept at 8 workers or more until 2026-11-02T08:00:00Z
$ muructl scale shop --group reports --min 2 --max 4 --until 2026-11-02T18:00 --timezone Australia/Sydney
```

This makes a temporary overlay, `shop:scale` or `shop:scale:reports`,
which the next such command for the group replaces, `muructl overlay end`
ends early, and which is forgotten once it ends. `muructl apply` leaves it
alone and `muructl config export` does not write it.

## The node's budget

The `[daemon.budget]` table of the daemon's configuration file caps what
the releases on the node hold between them:

```toml
[daemon.budget]
workers = 32
memory = '12GB'
cpu = 6
```

| Key | Limit |
| --- | --- |
| `workers` | Worker processes, and managed server instances, across every release |
| `memory` | Memory for the hosts and their workers |
| `cpu` | CPU cores the workers use |

Each limit is off when it is left out, and `muructl daemon reload` picks
up a change. A worker counts at what its group's workers measure, its
proportional set size and the CPU it uses, and before any has reported at
the group's `memory.soft_limit`, else at the mean of the node's workers.
On Linux each host counts at its own memory too. `muructl daemon status`
shows what is held:

```console
$ muructl daemon status
...
budget   27 of 32 workers, 9.8GB of 12GB memory, 3.1 of 6 cores
```

A scale-out, including the climb to a schedule's floor, is given as many
of the workers it asks for as still fit. When several groups want more at
once, applications with a higher `priority` go first:

```toml
[app.shop]
priority = 10
```

The priority is 0 when it is not set, and may be negative. A group given
fewer workers than it asked for stays at what it was given, shown as
`held at` in `muructl scale status`, and asks again once its rule has held
for another scale-out window. Each new shortfall is a
`scaling.denied_by_budget` event. Workers already running are not stopped
to make room for another application, a pin is not limited by the budget,
and an advisory policy reports without it.

Before a release starts for a deploy, a rollout or a traffic policy, its
memory is estimated from the application's serving release, else from
its `memory.soft_limit`. When it fits only with the retained releases
gone, they are stopped, oldest first; when it does not fit even then, the
deploy is refused with a `scaling.denied_by_budget` event and no release
is recorded. A rollback, and the releases the daemon starts again after a
restart, are not held back. While the node is over its memory budget,
retained releases are stopped, oldest first.

`muructl apply` warns of a release, a schedule or an overlay whose floor
would not fit the budget as it stands, and each window that raises a
floor is checked again ahead of time, from the start of its `warm_up` or
15 minutes before it, whichever is earlier, with a `schedule.over_budget`
event when it would not fit.

## Watching and overriding

```console
$ muructl scale status shop
shop@r42
GROUP        WORKERS      SLOTS      IDLE    QUEUE P95  REAPS      POLICY
default      3 -> 5       12/12      -1.0    230 ms     0/6        spare 2-12 workers, 1-4 idle
  workers: cpu 12%, gil wait p95 1 ms, loop lag p95 -
  diagnosis: I/O bound with exhausted slots; for the next release, more threads per worker, or more concurrency for an ASGI application
  last: scale out from 3 to 5 workers: short by 1.0 workers for 5s (2s ago)
reports      1            0/1        1.0     -          0/6        target 1-6 workers, 80% of slots busy
  workers: cpu 2%, gil wait p95 1 ms, loop lag p95 -
  diagnosis: idle
```

`WORKERS` shows the workers the edge routes to and, while they differ, the
count the group is being brought to. The `workers` line is what the
group's workers report about themselves every `check_interval` of the
[memory policy](memory.md) (5 seconds by default), over the time since
their last report: their CPU time per second as a share of what a
worker can use (one core with the GIL, a core per thread on a
free-threaded interpreter without it), the 95th percentile wait of a
thread for the GIL when it has work to run, and the 95th percentile
lateness of an ASGI worker's event loop, which beats twice a second.

The `diagnosis` line names what holds the group back and what to change
in the next release:

| Diagnosis | When | Suggested |
| --- | --- | --- |
| memory bound | a worker within 10% of its soft memory limit | fewer threads per worker, or a higher memory limit |
| event loop held by blocking code | loop lag p95 of 100 ms or more | await blocking calls, or run them in a thread |
| GIL bound | GIL wait p95 of 10 ms or more | more processes with fewer threads each |
| CPU bound | CPU at 80% or more of what a worker can use | more processes, up to the node's cores |
| I/O bound with exhausted slots | 80% or more of the slots busy, CPU lower | more threads per worker, or more concurrency |
| a few heavy requests | queue wait p95 of 100 ms or more with under half the slots busy | a worker group of their own for the slow paths |
| idle | under 10% of the slots busy and no queue | |
| within capacity | anything else | |

The first that applies is the diagnosis. While a schedule sets capacity, a
line below the group's says so, as `scheduled: 24-48 workers, no
scale-in from shop.black-friday`. `IDLE` is the idle worker
equivalents, negative when the group is short. `REAPS` is the churn budget
used in the last hour, with `halt` while scale-in is suspended.

Every decision is a `scaling.decision` event with its reasons and the
diagnosis, and `muru_workers_desired`, `muru_scaling_decisions_total`
and `muru_churn_reaps_total` are in the metrics beside
`muru_workers_ready`, with each worker's CPU time as
`muru_worker_cpu_seconds_total`.

A pin overrides the autoscaler:

```console
$ muructl scale shop --processes 6
muru: shop@r42 default pinned to 6 workers
$ muructl scale shop --group reports --processes 2
muru: shop@r42 reports pinned to 2 workers
$ muructl scale shop --unpin
muru: shop@r42 default unpinned; the autoscaler decides again
```

The group is brought to the pinned count and left there, through daemon
restarts, until it is unpinned or the release stops. A pin on a scaled
group must not be over its `max_workers`; a static group can be pinned to
any count. With `advisory = true` in the policy the autoscaler decides and
reports as `advisory: would scale ...` events without acting, so windows
and thresholds can be tuned before they move anything.
