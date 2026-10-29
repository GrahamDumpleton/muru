# Scaling under load

```{admonition} Status
:class: note

This tutorial assumes you have done
[deploy with the daemon and roll back](deploy-and-roll-back.md).
```

A release has a fixed number of worker processes unless it is given a
scaling policy. With one, the daemon watches how busy the release is
every second, forks more workers when requests start to wait, and drains
the extras, slowly, when they are no longer needed. A new worker is a
fork of the host that already imported the application, so it is serving
within milliseconds.

In about fifteen minutes you will deploy an application with a scaling
policy, put load on it and watch it grow, take the load away and watch it
shrink, and then let it scale to no workers at all and wake on the next
request.

## A slow application

```
mkdir busy && cd busy
uv init --bare
uv add muru
```

Save this as `app.py`:

```{literalinclude} code/busy/app.py
:language: python
```

Each request takes a fifth of a second, as if it waited on a database.
A worker with two threads can therefore serve about ten requests a
second, whatever the machine.

Then add Muru's settings and a scaling policy to `pyproject.toml`:

```{literalinclude} code/busy/pyproject.toml
:language: toml
:start-at: "[tool.muru]"
```

The release starts with one worker of two threads. The policy is spare
mode: keep between one and three idle workers in hand, with between one
and eight workers in all. The last five keys shorten windows that are
minutes long by default, so that you can watch the policy act in a few
seconds. They are explained as they come up.

## Deploy it

```
muructl daemon start
muructl deploy busy .
muructl scale status busy
```

```
busy@r1
GROUP        WORKERS      SLOTS      IDLE    QUEUE P95  REAPS      POLICY
default      1            0/2        1.0     -          0/6        spare 1-8 workers, 1-3 idle
  workers: cpu -, gil wait p95 -, loop lag p95 -
  diagnosis: idle
```

One worker, none of its two slots busy, one idle worker in hand.

## Put load on it

Save this as `load.py`. It runs a number of clients, each asking for the
page again as soon as it has the last answer, for a number of seconds:

```{literalinclude} code/busy/load.py
:language: python
```

Eight clients need about 40 requests a second, four workers' worth. Run
them for a minute:

```
uv run load.py 8 60
```

and in another terminal, look at the group after ten seconds:

```
muructl scale status busy
```

```
busy@r1
GROUP        WORKERS      SLOTS      IDLE    QUEUE P95  REAPS      POLICY
default      5            7/10       1.0     1000 ms    0/6        spare 1-8 workers, 1-3 idle
  workers: cpu 1%, gil wait p95 0 ms, loop lag p95 -
  diagnosis: within capacity
  last: scaled out to 5: short by 3.0 workers for 5996ms (3s ago)
```

When the load arrived, requests waited at the edge for a free thread,
the group was short of workers for the 5 seconds of the scale-out window,
and the daemon forked four more: four to carry the load and one to keep
in hand. Seven or eight of the ten slots are busy, depending on the
moment, one worker is idle, and the queue wait has fallen from the
second the first requests spent waiting to a millisecond. The load
script's lines climb from about ten requests a second to about forty,
and none fails.

The diagnosis line describes the latest second. With five workers the
group is just within capacity, so it may also read I/O bound when all
eight clients are waiting at once. Why it grew is in the decision, which
is an event with its reasons:

```
muructl events
```

```
    5 10:14:07 info     scaling.decision     busy@r1                  default: scale out from 1 to 5 workers: short by 3.0 workers for 5996ms; diagnosis: I/O bound with exhausted slots; ...
```

The diagnosis when it decided says what held the group back: its
threads were busy but its CPU was not, which is what waiting on a
database looks like. More threads per worker would carry the same load
with fewer processes, and the daemon suggests it for the next release
rather than changing a running one. On a slower machine the clients can
arrive over more than one decision, and the group grows to five in
steps, each with its own event.

## Take the load away

When the load script finishes, the workers go idle, and the daemon takes
them away again, but slowly and one at a time. Watch
`muructl scale status busy` for a minute, or `muructl events --follow`:

```
default: scale in from 5 to 4 workers: at least 5.0 idle workers over the last 20s, over max_spare 3; diagnosis: idle
default: scale in from 4 to 3 workers: at least 4.0 idle workers over the last 20s, over max_spare 3; diagnosis: idle
default: scale in from 3 to 2 workers: no request for 30s; diagnosis: idle
default: scale in from 2 to 1 workers: no request for 30s; diagnosis: idle
```

Two rules drained them. Over `scale_in_window` (20 s here, 10 minutes by
default) more than `max_spare` workers were idle all the time, so the
group came down to three; then no request came for `idle_reap_after` (30
s here, 10 minutes by default), so it came down to `min_workers`. The
daemon waited `hold_after_growth` after the scale-out before draining
anything, drained one worker per `reap_interval`, and drained no worker
younger than `min_lifetime`. The defaults are slow on purpose: traffic
that dips and comes back should find its workers still there.

`REAPS` in the status counts the drains in the last hour against the
release's churn budget, six an hour by default. A release that spends
it stops scaling in for the rest of the hour, so a policy that is tuned
badly cannot make the group swing up and down all day.

## Scale to zero

An application that is idle most of the time need not keep a worker at
all. Change `min_workers` in `pyproject.toml`:

```toml
min_workers = 0
```

and deploy the change as a new release:

```
muructl deploy busy .
```

After 30 seconds without a request, the group drains its last worker,
and `muructl scale status busy` shows `0` workers. The host stays, with
the application imported. Ask for the page:

```
curl -w '%{time_total}s\n' http://127.0.0.1:8080/
```

```
done
0.212236s
```

The request waited at the edge while the daemon forked a worker from the
host, then was served: the fifth of a second is the application's own.
The event says what happened:

```
default: scale out from 0 to 1 workers: a request is waiting for a group scaled to zero
```

With `host_idle_timeout` in the policy, the host stops too after that
long at zero, and the next request waits for the application's import as
well.

## Pin it by hand

The autoscaler can be overruled. Pin the group to three workers:

```
muructl scale busy --processes 3
```

```
muru: busy@r2 default pinned to 3 workers
```

`muructl scale status busy` shows `3 pinned`, and the policy leaves the
group alone, through daemon restarts, until the pin is removed:

```
muructl scale busy --unpin
```

```
muru: busy@r2 default unpinned; the autoscaler decides again
```

## Clean up

```
muructl daemon stop --shutdown
```

## From here

- [Scaling](../howto/scaling.md) covers target mode, which tracks how busy
  the slots are, scaling worker groups separately, scheduled capacity
  for known busy hours, and the node's budget shared by every
  application.

- [Worker groups](../howto/worker-groups.md) gives slow paths workers of
  their own, so they do not take the threads of fast ones.

- [Process models](../explanation/process-models.md) explains processes,
  threads and the GIL, behind the diagnosis lines.
