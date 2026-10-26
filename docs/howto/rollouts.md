# Rollouts

A deploy moves traffic to the new release as soon as it is ready. A
rollout does it in stages: the release is started without traffic, gets a
share of the application's requests step by step, is judged at each step
against the release that still serves, and is promoted when every step
passes or rolled back when one fails. Rollouts need a route to the
application, since the shares are applied on the application's routes
(see [virtual hosts](virtual-hosts.md)).

## Holding a release

```console
$ muructl deploy shop /srv/shop --hold
muru: release shop@r2 is ready and held: not serving
```

With `--hold` the release is started and left running without traffic.
It can be checked through a preview, with the `Muru-Preview: r2` header
or a `muru_preview=r2` cookie, which routes a request to it whatever the
traffic policy says (see [traffic between releases](traffic.md)). A held
release stays until a rollout or a promotion takes it, or `muructl drain`
stops it.

## Blue/green

```console
$ muructl rollout bluegreen shop@r2
muru: release shop@r2 is ready and serving
```

All of the application's traffic moves to the release at once, and the
release that served drains and is kept warm, as after a deploy. This is
the deploy's own strategy, available separately so a held release can be
checked first.

## Canary

```console
$ muructl rollout canary shop@r2 --steps 10,50 --hold 5m --wait
muru: rollout 1 of shop@r2 started against r1: step 1 of 2: 10% for 5m
muru: rollout 1 running step 2 of 2: 50% for 5m
muru: rollout 1 promoted: every step passed its gates
```

The candidate takes each step's share of the application's requests in
turn, with clients spread as a traffic policy spreads them: hashed to
stable buckets by the anonymous id cookie the edge issues, so a client in
the canary at 10% is still in it at 50%. Each step holds for `--hold`,
and when the hold has passed with the gates satisfied the next step
begins. When the last step passes, the candidate is promoted: it takes all
the traffic, and the baseline drains and is kept warm. Without `--wait`
the command returns at once and `muructl rollout status` follows the
rollout.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--steps` | `10,50` | The candidate's shares, in percent, increasing |
| `--hold` | `2m` | How long each step lasts before its gates decide |
| `--max-error-rate` | `1` | The candidate's 5xx rate over the step, in percent |
| `--max-p95` | none | The candidate's 95th percentile request time over the step |
| `--max-p99` | none | The candidate's 99th percentile request time over the step |
| `--max-worker-faults` | `0` | The candidate's worker crashes and timeout kills over the step |
| `--max-queue-wait` | none | The candidate's 95th percentile wait for a worker thread over the step |
| `--probe` | none | A URL requested from the candidate and the baseline every 5 seconds; repeatable |
| `--min-requests` | `100` | Requests the candidate must serve in a step before the gates judge |
| `--approve-each-step` | off | Wait for `rollout resume` before each step after the first |

## Gates

A step is judged on the requests served under it alone, counted from
the moment its share took effect, and the candidate is compared with the
baseline over the same window. A gate fails only when the candidate is
both over its limit and worse than the baseline, so a fault both releases
share, such as a database outage, does not roll the candidate back. A
baseline with fewer than `--min-requests` requests in the window is not
compared, and the limits alone apply.

The gates are checked every second once the candidate has served
`--min-requests` requests in the step. A failing gate rolls back at once,
without waiting for the hold to end: the candidate leaves traffic and is
kept warm, so its logs and workers can be looked at, and the rollout
records why:

```console
$ muructl rollout status shop
APP              ROLLOUT  CANDIDATE    BASELINE     STATE              TRAFFIC  MESSAGE
shop             1        r2           r1           rolled_back        0%       gate failed at 10%: 5xx rate 4.00% over the limit of 1.00% (baseline 0.00%)
```

A step whose hold has passed with too few requests to judge holds on
until there are enough, saying so in its message. A quiet application can
be promoted by hand instead.

A worker that crashes, or that the [hard timeout](timeouts.md#the-hard-timeout)
or the loop stall timeout replaces, counts against its release at once,
whatever the step's request count: by default one more than the baseline
had over the step rolls the candidate back.

A probe is a URL the daemon requests through the edge every 5 seconds
while the rollout runs, once from the candidate and once from the
baseline, each by preview with the `Muru-Preview` header. A probe passes
with a 2xx or 3xx answer within 5 seconds. Like a crash, a failed probe
counts at once: one more failure on the candidate than on the baseline
over the step rolls it back, so a health check failing on both
releases, as during a database outage, does not. Give the URL of
a listener the edge serves the application's route on:

```console
$ muructl rollout canary shop@r2 --probe http://127.0.0.1:8080/health --probe http://127.0.0.1:8080/cart
```

Probe requests count as requests of their release, as any other request
does.

## Controlling a rollout

| Command | Effect |
| --- | --- |
| `muructl rollout status [APP] [--wait]` | The rollouts, or one application's; with `--wait`, follow the application's rollout until it ends |
| `muructl rollout pause APP` | Hold the current step and its share; the gates stop judging |
| `muructl rollout resume APP` | Continue a paused rollout, or approve a step that awaits approval; the step's hold and window start afresh |
| `muructl rollout promote APP` | Give the candidate all the traffic now and end the rollout |
| `muructl rollout abort APP` | Take the candidate out of traffic now, keep it warm and end the rollout |

With `--approve-each-step`, the rollout stops before each step after the
first with the state `awaiting_approval` and the previous step's share
still in force, until `rollout resume` approves it.

An application has at most one rollout under way. A rollout's candidate
is kept running while the rollout lasts, a deploy or rollback that would
drain it leaves it in place, and `muructl config export` does not include
its share, which is live state rather than declared configuration.

## Deploy freezes

While an overlay or a [schedule](scaling.md#scheduled-capacity) with
`freeze_deploys = true` is active for an application, its deploys and
rollouts are refused: `muructl deploy`, `muructl apply` when it would
deploy a release of the application, `muructl rollout canary`,
`bluegreen` and `promote`. A rollout already under way holds its step
until the freeze ends. Rolling back, pausing and aborting are never
frozen.

Each of those commands takes `--override-freeze` to go ahead anyway,
which is recorded as a `deploy.freeze_overridden` event at warning level,
saying what was done and which overlay froze it.

## Events

Every change is an event: `rollout.started`, `rollout.step` for each new
step or one awaiting approval, `rollout.paused` and `rollout.resumed`,
`rollout.gate_failed` with the gate and the numbers, `rollout.completed`
when the candidate is promoted and `rollout.rolled_back` when it is taken
out, by a gate or by hand.
