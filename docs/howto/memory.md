# Memory limits

```{admonition} Status
:class: note

Soft and hard memory limits per worker, measured as USS, PSS or RSS,
with reclaiming before a recycle and a leak warning. The node-wide
budget is in [scaling](scaling.md). Per-group limits, allocator settings
and per-request memory deltas are still to come.
```

A worker that grows too large is replaced through the same path as
[worker recycling](recycling.md), so no request is lost. Each worker
measures its own memory at `check_interval` and reports it to the host,
which holds it to two limits:

| Setting | Default | Meaning |
| --- | --- | --- |
| `measure` | `uss` | what is measured: `uss`, `pss` or `rss` |
| `soft_limit` | off | over this for `soft_for`, the worker reclaims and then, still over, is recycled |
| `soft_for` | `2m` | how long a worker may stay over the soft limit |
| `reclaim_first` | `true` | run a garbage collection and trim the heap before recycling |
| `hard_limit` | off | over this the worker leaves routing at once and is replaced |
| `check_interval` | `5s` | how often memory is measured |

```
muructl run app:application --memory soft_limit=800MB --memory hard_limit=1200MB
muructl deploy shop /srv/shop --memory soft_limit=800MB --memory measure=rss
```

```toml
[tool.muru.memory]
measure = "uss"
soft_limit = "800MB"
soft_for = "2m"
reclaim_first = true
hard_limit = "1200MB"
check_interval = "5s"
```

The same table goes in a declared release for `muructl apply`, and
`muructl run --print-config` writes it. Sizes are `800MB`, `1.5GB` or a
number of bytes, with KB, MB and GB as powers of 1024.

## What is measured

The unique set size (USS) counts only the pages no other process shares,
which is the memory a worker really adds, since the pages it inherited
from the preloaded host are shared copy-on-write until written. The
proportional set size (PSS) splits shared pages between the processes
that share them, and the resident set size (RSS) counts every resident
page. On Linux the figures come from the kernel's own accounting; on
macOS the resident size is reported, and the task's physical footprint
stands in for USS and PSS.

## What happens

A short spike over the soft limit does nothing: only a worker still over
it after `soft_for` is acted on. With `reclaim_first` the host asks the
worker to collect garbage and trim its heap (with `malloc_trim` on
glibc) and measures once more; a worker still over is recycled, with a
`worker.recycled` event that names the measure, the figure and the
limit. Over the hard limit a worker is treated as the hard timeout
treats a stuck one: it leaves routing at once, a replacement is forked,
and it is told to stop after `hard_drain_grace`.

A worker index recycled for memory three times within an hour raises a
`memory.leak_suspected` warning, once an hour. Container and cgroup
limits remain the kernel's backstop.

## Metrics

Under the daemon each worker's last report is served on `/metrics`:
`muru_worker_memory_bytes` by measure, `muru_worker_requests_total` and
`muru_worker_requests_in_flight`, labelled with the application, release,
group and worker index; the [metrics guide](metrics.md) lists them.
