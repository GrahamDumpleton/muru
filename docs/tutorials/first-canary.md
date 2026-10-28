# Your first canary

```{admonition} Status
:class: note

This tutorial carries on from
[deploy with the daemon and roll back](deploy-and-roll-back.md), in the
same `site` directory with the same daemon.
```

A deploy moves all the traffic to a new release as soon as it is ready.
That catches a release that cannot start, but not one that starts and
then fails some of the requests it serves. A canary gives the new
release a small share of the requests first, compares how it does with
the release that serves, and only then gives it more. A bad release is
taken out of traffic by the daemon, having failed a few requests rather
than all of them.

In about fifteen minutes you will run a canary of a release with a bug
and watch the daemon roll it back, then run one of a good release,
check it yourself on the way, approve its second step and see it
promoted.

## Start where you left off

Start the daemon again if it is not running. It serves `r1`, the release
the last tutorial rolled back to:

```
muructl daemon start
curl http://127.0.0.1:8080/
```

```
Hello from hello@r1
```

## A release with a bug

Make release `v4`, which has a new greeting and a bug that fails one
request in four:

```
cp -R v1 v4
```

```{literalinclude} code/releases/v4/app.py
:language: python
:emphasize-lines: 1,5-6,10-11
```

Deploy it with `--hold`, which starts it and leaves it out of traffic:

```
muructl deploy hello v4 --hold
```

```
muru: release hello@r4 is ready and held: not serving
```

Every request still goes to `r1`.

## Traffic to judge it by

A canary is judged on requests, so the application needs some. In a
second terminal, start a loop that asks for the page as fast as it can:

```
while true; do curl -s http://127.0.0.1:8080/; done
```

Each `curl` is a new client to the daemon. The edge gives every client
an anonymous id in a cookie and hashes it to decide which release the
client gets during a canary, and since `curl` keeps no cookies, every
request is a new draw. A browser keeps its cookie, so a person stays on
the same release from one page to the next.

## Run the canary

```
muructl rollout canary hello@r4 --steps 10,50 --hold 30s
```

```
muru: rollout 1 of hello@r4 started against r1: step 1 of 2: 10% for 30s
```

The candidate, `r4`, now gets 10% of the requests, and the release that
serves, `r1`, is the baseline it is compared with. The plan is to hold
each step for 30 seconds and go from 10% to 50% and then to all of it.
Follow the rollout:

```
muructl rollout status hello --wait
```

Within seconds, the loop shows a few `Howdy from hello@r4` lines among
the `Hello from hello@r1` ones, and then the occasional error page:

```
500 Internal Server Error
reason: app_error
```

Once the candidate has served 100 requests in the step, the daemon
judges it every second. Its 5xx rate is about 25%, far over the default
limit of 1%, while the baseline's is 0%, so it rolls the candidate back
without waiting for the 30 seconds to end:

```
muru: rollout 1 rolled_back: gate failed at 10%: 5xx rate 24.53% over the limit of 1.00% (baseline 0.00%)
```

The loop is back to `Hello from hello@r1` alone. Of the requests the
loop made during the canary, only those that drew `r4` and then hit the
bug failed: a few dozen, where a plain deploy would have failed a quarter
of everything until someone noticed. The candidate is kept running out
of traffic, so its log (`muructl logs hello@r4`) can be read, and the
events have the whole story:

```
muructl events
```

```
    27 09:56:32 notice   rollout.started      hello@r4                 rollout 1 against r1: 10% for 30s, 50% for 30s
    28 09:56:38 warning  rollout.gate_failed  hello@r4                 step 1 at 10%: 5xx rate 24.53% over the limit of 1.00% (baseline 0.00%)
    29 09:56:38 warning  rollout.rolled_back  hello@r4                 rollout 1: gate failed at 10%: 5xx rate 24.53% over the limit of 1.00% (baseline 0.00%); the candidate is kept warm
```

A gate only fails when the candidate is worse than the baseline over the
same seconds, so a fault both releases share, such as a database that
has gone away, does not roll a candidate back.

## A good release

Fix the bug in a fifth release, keeping the new greeting:

```
cp -R v1 v5
```

```{literalinclude} code/releases/v5/app.py
:language: python
:emphasize-lines: 3
```

```
muructl deploy hello v5 --hold
```

This time, ask the daemon to wait for your approval before each step
after the first:

```
muructl rollout canary hello@r5 --steps 10,50 --hold 30s --approve-each-step
```

## Check it yourself

While the candidate has its 10%, you can reach it on purpose. The
`Muru-Preview` header names a release, and a request that carries it
goes to that release whatever share it has:

```
curl -H 'Muru-Preview: r5' http://127.0.0.1:8080/
```

```
Howdy from hello@r5
```

A `muru_preview=r5` cookie does the same in a browser.

## Approve the next step

After 30 seconds with its gates passing, the rollout stops before its
second step:

```
muructl rollout status hello
```

```
APP              ROLLOUT  CANDIDATE    BASELINE     STATE              TRAFFIC  MESSAGE
hello            1        r4           r1           rolled_back        0%       gate failed at 10%: 5xx rate 24.53% over the limit of 1.00% (baseline 0.00%)
hello            2        r5           r1           awaiting_approval  10%      step 2 of 2: 50% for 30s awaits approval: muructl rollout resume hello
```

The candidate keeps its 10% until you decide. Approve the step and
follow it:

```
muructl rollout resume hello
muructl rollout status hello --wait
```

```
muru: rollout 2 running step 2 of 2: 50% for 30s
muru: rollout 2 promoted: every step passed its gates
```

Half the loop's lines say `Howdy` for 30 seconds, and then all of them
do. The last step passing promoted `r5`: it serves every request, and
`r1` is kept warm, so `muructl rollout abort`, before the end, or
`muructl rollback hello`, after it, would still put things back at once.

## Clean up

Stop the loop with Ctrl-C. When you have finished with the daemon:

```
muructl daemon stop --shutdown
```

## From here

- [Rollouts](../howto/rollouts.md) lists every gate: the 95th and 99th
  percentile request times, worker crashes, the wait for a worker, and
  probe URLs the daemon requests itself, with the commands that pause,
  resume, promote and abort a rollout.

- [Traffic between releases](../howto/traffic.md) explains the buckets,
  sticky sessions and previews a canary is built on, and splits that
  stay, for A/B tests.

- [Virtual hosts and TLS](../howto/virtual-hosts.md) gives an application
  host names and routes, each of which a canary shares in the same way.
