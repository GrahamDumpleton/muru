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

## 7.4 Scaling policy and churn avoidance

- Scale-out: any rule sustained for its window. Short cooldown, bounded by max_step_up, with starting workers counted as pending capacity.
- Scale-in: only when every scale-down condition holds over a long window, based on the peak busy level in that window, at most one worker per reap_interval, never within hold_after_growth of a scale-out, never below min_workers, and never a worker younger than min_lifetime.
- Spare mode defaults: max_spare = max(3, 25% of workers), scale_in_window = 10 min, hold_after_growth = 5 min, reap_interval = 60 s, idle_reap_after = 10 min, min_lifetime = max(15 min, 30 times measured time to ready), max_step_up = 4, churn_budget = 6 reaps per hour per release. The validator requires max_spare at least min_spare + 2.
- When the churn budget is exhausted, scale-in is suspended, a warning event is raised and windows lengthen. Recycling uses jitter and global staggering.
- A node budget manager (workers, memory, optional CPU) admits scale-ups across applications by priority. Scale to zero is supported per group, keeping the host warm by default (first request costs a fork) or stopping it too (host_idle_timeout).
- Every decision is an event with its reasons. An advisory mode recommends without acting, and a manual pin overrides the autoscaler.

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

## 7.6 Request timeouts

| Timeout | Measured from | Enforced by | Action |
| --- | --- | --- | --- |
| queue_timeout | accepted at edge | edge | 503 queue_full |
| request_deadline | request start at edge | edge | 504, stream cancelled |
| soft_timeout (opt-in) | start of execution | native watchdog in worker | inject muru.RequestTimeout into the thread |
| hard_timeout | start of execution | host, from worker reports, edge backstop | replace the worker |
| loop_stall_timeout | event loop stalled | native watchdog | hard path |
| idle_timeout | no data moved | edge | for streaming routes and WebSockets, which are exempt from soft and hard timeouts |

- Soft timeout uses PyThreadState_SetAsyncExc with a BaseException subclass. Threads blocked in bridge calls are woken immediately; threads in C calls see it when the call returns. The bridge logs the traceback and returns 504 if no response has started. ASGI uses task cancellation.
- Hard timeout: the worker is removed from routing, a replacement is forked, other requests get hard_drain_grace, stacks are recorded (faulthandler if the GIL cannot be taken), then SIGTERM and SIGKILL after shutdown_timeout.
- Failsafe layers: native watchdog, host, edge backstop on dispatch age, daemon watching host heartbeats, SIGKILL. hard_timeout must exceed soft_timeout by at least 5 s. Timeout kills do not use the churn budget.

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
