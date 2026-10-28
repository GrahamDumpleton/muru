# 7. Process management

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

The daemon decides how many workers each group needs and the host carries it out by forking or draining. Scaling, recycling and timeouts all use the same replacement path: fork a replacement from the preloaded host, wait until it is ready, then drain the old worker.

## 7.1 Worker groups

A release can split its paths across named worker groups, all forked from one host and one preload. Each group has its own processes, threads, scaling, timeouts, memory policy and queues, which gives sizing (many threads for cheap I/O-bound paths, many processes for CPU-bound ones) and isolation (slow endpoints cannot take capacity from fast ones). Without groups there is one implicit group named default.

```toml
[app.shop.release.r42]
processes = 2
threads = 8                       # default group

[app.shop.release.r42.groups.hot]
paths = ['/api/status', '/api/ping']
processes = 1
threads = 50

[app.shop.release.r42.groups.reports]
paths = ['/reports/*']
mounts = ['reports']              # optional subset of mounts loaded by this group
processes = 6
threads = 1
timeouts = { hard_timeout = '300s' }
scaling = { mode = 'target', min_workers = 0, max_workers = 8 }
```

- Paths are prefixes with * globs, or match lists with host, method and header conditions. The most specific match wins. Duplicates are rejected.
- Groups can ship in [tool.muru.groups] and be resized by operators. MURU_GROUP and the hook context identify the group after fork.
- A release is ready when every required group has its minimum ready workers. Groups can be optional.
- Groups share the interpreter and import-time configuration. Differences must be applied after fork.
- **Details fixed by the implementation.** A group's `timeouts` table may set `soft_timeout`, `hard_timeout`, `hard_drain_grace`, `loop_stall_timeout` and `gil_stall_timeout`, each the release's when absent, and may not set `shutdown_timeout` or `websocket_drain_grace`, which bound the stop budget of every worker and so belong to the release; the hard timeout must exceed the soft by 5 s within the group as within the release. Its `memory` table sets any of the keys of section 7.7 over the release's policy. The host receives every group's resolved values and applies them per worker by its group, and `config export` writes a group's timeouts as set and its memory policy whole. `max_requests` and `max_lifetime` stay per release.

### Release variants

Some differences cannot be applied after fork, because they need a differently initialised interpreter: a free-threaded build, another Python version, or a different environment. A release can therefore declare variants. Each variant is its own host, with its own interpreter, environment and preload, and has its own worker groups. The release's top-level settings describe the implicit default variant, so releases without variants are unchanged.

```
release shop@r42
  variant default   Python 3.14, one host, one preload
    group default   paths not claimed elsewhere
    group hot       /api/status
  variant ft        Python 3.14t, its own host and preload
    group heavy     /api/heavy/
    group batch     /api/batch/
```

```toml
[app.shop.release.r42]
python = '3.14'                      # default variant
processes = 2
threads = 8

[app.shop.release.r42.groups.hot]
paths = ['/api/status']
threads = 50

[app.shop.release.r42.variants.ft]
python = '3.14t'
require_free_threading = true

[app.shop.release.r42.variants.ft.groups.heavy]
paths = ['/api/heavy/']
processes = 1
threads = 32
```

- **What a variant may change:** interpreter, environment build and environment variables, plus everything a group can set. The source is always the same; a different source is a different release.
- **Path selection:** paths are matched across all groups of all variants in the release, with the most specific match winning. Each path belongs to exactly one group, so the path selects both the variant and the group. Variants need no route entries of their own.
- **Mounts** are defined once at release level and can be loaded by groups in any variant.
- **Traffic, readiness and rollback** apply to the release as a whole. Bucketing picks the release, then the path picks variant and group, so a page and its API calls always come from the same code version. A release is ready only when the required groups of every variant are ready, canaries move all variants together, and rollback reverts them together.
- **Cost:** each variant has its own preload and environment, so memory grows by roughly one preloaded application per variant.

## 7.2 Scaling modes

| Mode | Behaviour |
| --- | --- |
| static | Fixed worker count |
| spare | Apache-style: keep between min_spare and max_spare idle worker equivalents, with a doubling spawn ramp while requests queue and slow reaping |
| target | Track targets for chosen signals (slot_utilization, queue_wait_p95), with smoothing and stabilisation |

## 7.3 Signals

| Signal | Source | Meaning |
| --- | --- | --- |
| queue_wait_p95 | edge | demand exceeds capacity, cause-agnostic |
| slot_utilization | edge and worker | busy slots over total slots |
| request rate, 503 rate | edge | demand and shed load |
| worker_cpu | worker | CPU seconds per wall second, normalised for free-threaded builds |
| gil_wait_p95 | worker, Rust extension | time from ready to holding the GIL at Rust to Python transitions |
| sched_latency, loop_lag | worker | overshoot of a probe timer or event loop timer |
| service time inflation | edge | p50 against the low-load baseline |
| handoff_queue_wait_p95 | worker | requests handed from ASGI to WSGI waiting for a thread (section 6.14), answered by more threads or a mount |
| uss, pss, rss | worker and host | memory, a constraint rather than a trigger |

A diagnosis table combining busy slots, CPU and GIL wait classifies bottlenecks as CPU or GIL bound, I/O bound with exhausted slots, a few heavy requests, idle, or memory bound. It is reported with every decision and suggests tuning (for example more threads for I/O-bound groups) for the next release.

- **Details fixed by the implementation.** A worker adds its CPU time (getrusage, user and system), whether its interpreter has a GIL, and two cumulative histograms to the status line it writes every memory `check_interval`: the wait from a thread having work, a WSGI job or an ASGI request or message for the event loop, to holding the GIL, and how much later than due the event loop's half-second heartbeat ran, over bounds of 0.1, 0.5, 1, 5, 10, 50, 100 and 500 ms, 1 s and 5 s. The host relays them, and the daemon takes the growth between two reports of the same pid: worker_cpu is CPU seconds per wall second averaged over the group's workers and divided by one core with a GIL or by the threads a worker runs Python on without one (its threads, or its sync threads and the loop's for ASGI), and the percentiles are the bound holding 95% of the observations. sched_latency, service time inflation and handoff_queue_wait are not measured. The diagnosis takes the first that applies: memory bound (a worker within 10% of the soft memory limit), event loop held (loop lag p95 of 100 ms or more), GIL bound (GIL wait p95 of 10 ms or more), CPU bound (worker_cpu of 0.8 or more), I/O bound with exhausted slots (80% of the slots busy), a few heavy requests (queue wait p95 of 100 ms or more with under half the slots busy), idle (under 10% busy and no queue), else within capacity. It is appended to every `scaling.decision` event and shown by `muructl scale status` with the signals; the signals do not drive scale-out, which the slot and queue rules of section 7.4 decide. `muru_worker_cpu_seconds_total` carries each worker's CPU time.

## 7.4 Scaling policy and churn avoidance

- Scale-out: any rule sustained for its window. Short cooldown, bounded by max_step_up, with starting workers counted as pending capacity.
- Scale-in: only when every scale-down condition holds over a long window, based on the peak busy level in that window, at most one worker per reap_interval, never within hold_after_growth of a scale-out, never below min_workers, and never a worker younger than min_lifetime.
- Spare mode defaults: max_spare = max(3, 25% of workers), scale_in_window = 10 min, hold_after_growth = 5 min, reap_interval = 60 s, idle_reap_after = 10 min, min_lifetime = max(15 min, 30 times measured time to ready), max_step_up = 4, churn_budget = 6 reaps per hour per release. The validator requires max_spare at least min_spare + 2.
- When the churn budget is exhausted, scale-in is suspended, a warning event is raised and windows lengthen. Recycling uses jitter and global staggering.
- A node budget manager (workers, memory, optional CPU) admits scale-ups across applications by priority. Scale to zero is supported per group, keeping the host warm by default (first request costs a fork) or stopping it too (host_idle_timeout).
- Every decision is an event with its reasons. An advisory mode recommends without acting, and a manual pin overrides the autoscaler.
- **Details fixed by the implementation.** The daemon file's `[daemon.budget]` table sets `workers` (worker processes and managed server instances), `memory` (a size) and `cpu` (cores), each off when omitted, read at start and on reload, and an application's `priority` (`[app.NAME] priority`, 0 by default, higher first) orders admission. Each second the daemon counts what the running releases hold: each group at the larger of its workers and the count its autoscaler is bringing it to (a retained release at its workers), each worker at its group's mean proportional set size and CPU cores as its workers report them, the soft memory limit standing in before any has reported and then the mean of the node's workers, and each host at its own proportional set size where /proc can be read (on Linux). Scale-outs, a schedule's raise toward its floor among them, are admitted against that, the highest priority first and in application, release and group order within one, each given as many of the workers it asked for as still fit. A group given fewer holds at what it was given (`held at` in `muructl scale status`), asks again once its rule has held for another scale-out window, and raises `scaling.denied_by_budget` once for each different shortfall. Workers already running are never stopped to make room, a pin is not checked, and an advisory group recommends without the budget. `muructl daemon status` shows what is held of each limit. While the node is over its memory budget, the retained release created first that still has workers drops to its host alone, and once none has, the retained release created first is stopped, one step a second (section 9.1).
- **Details fixed by the implementation.** Scale to zero: a spare or target group may have `min_workers = 0`. It drains its last worker once no request has come for `idle_reap_after` (spare mode) or none was busy over the scale-in window (target mode), by the usual scale-in rules, and at zero the spare rule asks for nothing. The snapshot names the groups of a running release that have no workers, and the edge holds a request for one, counted as queued, until a pool with workers arrives or its queue timeout passes, telling the daemon over the edge control service's WatchWakes stream at most twice a second; the daemon gives the group one worker at once, as a scale-out the budget admits, with the reason `a request is waiting for a group scaled to zero`, and a waiting request seen in the queue depth wakes it the same way. A group whose workers are on their way, after a crash or at a warm floor, holds requests the same way. `host_idle_timeout`, in the release's own scaling table and only when every group has `min_workers = 0`, stops the host once every group has been at zero for that long, with the event `release.idle`; the release stays in the snapshot, ready, and the next request starts its host again, `release.woken`, the request waiting through the import (a cold start) within its queue timeout. A gRPC mount is matched by the services its workers registered, so its calls are not held for a group at zero.

## 7.5 Scheduled capacity

Schedules on the application (optionally per group) raise the floor and optionally the ceiling for time windows, so capacity is warm before predictable load.

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
```

Recurring windows use cron plus duration. Overlapping schedules take the highest floor and ceiling. Warm-up ramps in staggered batches. After a window, scale-in follows the normal slow rules. During canaries the floor is divided by traffic share. Budgets are checked when a schedule is created and ahead of each window. freeze_deploys blocks releases and rollouts unless overridden. muructl scale --min N --until TIME creates a temporary schedule.

- **Details fixed by the implementation.** Each `[[app.NAME.schedule]]` entry, with `name` (a valid name, unique in the application), `start` and `end` or `cron` and `duration` with `timezone` (read as section 9.10 reads an overlay's schedule), `group` (the default group without it), `min_workers`, `max_workers`, `warm_up` (a lead), `scale_in` (`hold` or `normal`) and `freeze_deploys`, is recorded as the overlay `APP.NAME` setting those keys under `apps.APP`, which any overlay may also set; a schedule's activation and end are the events schedule.activated and schedule.expired, and export writes schedules back to their applications. Capacity never conflicts between overlays: the highest floor and ceiling apply, and any `hold` keeps the group from scaling in. A floor above the ceiling raises the ceiling, and a static group's count rises to it; it is reached in batches of max_step_up, each once the last has arrived and no sooner than a scale-out window after it, and applies through a warm-up's lead. A static group above its count, after a window or a lifted pin, returns to it one worker per reap interval. A release's share of the floor is its largest rollout or split share on its application's routes, the serving release taking what the others leave, rounded up. `muructl scale --min N [--max M] --until TIME` (or `--for`) records the overlay `APP:scale` (`APP:scale:GROUP` for another group), replacing one there, active until then and deleted when it ends; apply and export leave it out, and `scale status` shows the floor and ceiling in force and what sets them. Applying a schedule or an overlay that raises a floor warns when the floor does not fit the node's budget as it stands, and each window is checked again once ahead of it, from the start of its warm-up's lead or 15 minutes before it, whichever is longer, raising `schedule.over_budget` when it does not fit; at the window, the raise is admitted as any scale-out is (section 7.4).

## 7.6 Request timeouts

| Timeout | Measured from | Enforced by | Action |
| --- | --- | --- | --- |
| queue_timeout | accepted at edge | edge | 503 queue_full |
| request_deadline | request start at edge | edge | 504, stream cancelled |
| soft_timeout (opt-in) | start of execution | native watchdog in worker | inject muru.RequestTimeout into the thread |
| hard_timeout | start of execution | host, from worker reports, edge backstop | replace the worker |
| loop_stall_timeout | event loop stalled | native watchdog | hard path |
| gil_stall_timeout | no thread has run Python, or the worker has not reported | native watchdog, host | hard path |
| idle_timeout | no data moved | edge | for streaming routes and WebSockets, which are exempt from soft and hard timeouts |

- Soft timeout uses PyThreadState_SetAsyncExc with a BaseException subclass. Threads blocked in bridge calls are woken immediately; threads in C calls see it when the call returns. The bridge logs the traceback and returns 504 if no response has started. ASGI uses task cancellation. A pending injection that the request outruns is withdrawn before the thread serves anything else.
- A request becomes exempt from the soft and hard timeouts once its response head goes out with immediate flushing (text/event-stream, or X-Accel-Buffering: no) or its WebSocket is accepted; until then a streaming route's request is timed like any other.
- Hard timeout: the worker is removed from routing, a replacement is forked, other requests get hard_drain_grace, stacks are recorded (faulthandler if the GIL cannot be taken), then SIGTERM and SIGKILL after shutdown_timeout.
- Failsafe layers: native watchdog, host, edge backstop on dispatch age, daemon watching host heartbeats, SIGKILL. hard_timeout must exceed soft_timeout by at least 5 s. Timeout kills do not use the churn budget.
- Liveness of the interpreter (ADR 0031): a native thread in every worker attaches to the interpreter every 500 ms and records the time; a thread that holds the GIL without returning, in a C call or a deadlock, stops it. The native watchdog reports `worker.gil_stalled` once the record is older than gil_stall_timeout, and the host takes the hard path. The host also times each worker's reports, which the worker's native side sends every second, and takes the hard path for a worker silent for gil_stall_timeout: one stopped or frozen whole, whose native threads do not run either. A worker being debugged is pinned, as for every policy.
- A worker is replaced on request with `muructl restart` (Appendix A), through the same path as a recycle; with `--force` it leaves routing at once, its stacks are taken as for the hard timeout, and it is killed a second later without being asked to stop. The event is `worker.restarted`; manual restarts do not use the churn budget.
- **Details fixed by the implementation.** The worker's event loop calls a heartbeat every 500 ms, and a native thread, which needs no interpreter, looks every 250 ms; once the loop has not beaten for `loop_stall_timeout` (10 s; `--timeout loop_stall=` on run and deploy) it reports `worker.loop_stalled` (error) to the host, once a stall, and the host takes the hard path with the event `worker.timeout_killed`, the stacks recorded before the worker goes. The interpreter heartbeat (`gil_stall_timeout`, 30 s; `--timeout gil_stall=`) runs in every worker, WSGI or ASGI, and reports `worker.gil_stalled` the same way; on a free-threaded build attaching never waits for another thread, so it fires only when the process itself is stopped. One being debugged (section 11.7) is pinned; `breakpoint()` waiting for an attach beats for the loop it holds. `muru.deadline` in the environ and `deadline` in every ASGI scope's `extensions["muru"]` are the edge's request deadline in seconds since the epoch.

## 7.7 Memory policy and recycling

```toml
[app.shop.release.r42.memory]
measure = 'uss'          # uss, pss or rss
soft_limit = '800MB'
soft_for = '2m'
reclaim_first = true
hard_limit = '1200MB'
check_interval = '5s'
malloc_arena_max = 2
allocator = 'system'      # system, jemalloc or mimalloc via LD_PRELOAD when present
```

- Unique memory (USS) is measured by default, since preloaded pages are shared copy-on-write.
- Over the soft limit for soft_for: the worker first runs gc.collect() and malloc_trim(0) between requests (glibc only) and re-measures, then recycles gracefully if still over. Short spikes, which may be returned to the operating system on their own, never trigger recycling.
- Over the hard limit: removed from routing at once, replacement forked, short grace, then stopped. Cgroup or container limits remain the kernel backstop.
- Recycles are rate limited and use max_surge. Repeated recycles raise a leak warning. Per-request memory deltas identify the routes that grow memory.
- Other recycling: max_requests and max_lifetime, both with jitter, through the same replacement path.
