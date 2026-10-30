# A slow request

```{admonition} Status
:class: note

This tutorial runs under the daemon and assumes you have done
[deploy with the daemon and roll back](deploy-and-roll-back.md). The same
steps work in `muructl run`, where the soft timeout's messages appear in
the terminal and the `muructl debug` commands find the run from the
project directory without a flag.
```

Sooner or later an application has a request that takes far longer than
it should. Muru has two kinds of help. A soft timeout stops a request
that runs too long, answers the client, and records exactly where the
request was when it was stopped. The `muructl debug` commands look into
running workers from outside: the stack of every thread right now, and a
profile of where the time goes over a period.

In about fifteen minutes you will deploy an application with a slow
report, catch its worst case with a soft timeout, see a request in
progress with `debug stacks`, and profile the release under load to find
where its time goes.

## A slow report

```
mkdir reports && cd reports
uv init --bare
uv add muru
```

Save this as `report.py`:

```{literalinclude} code/reports/report.py
:language: python
```

A report for some number of months fetches its rows, which waits on a
pretend database for a tenth of a second a month, and then summarises
them, which is pure Python arithmetic over eight million rows a month.
One month takes under half a second. Twelve take several seconds.

Add Muru's settings to `pyproject.toml`, with a soft timeout of three
seconds:

```{literalinclude} code/reports/pyproject.toml
:language: toml
:start-at: "[tool.muru]"
```

## Deploy it and time it

```
muructl daemon start
muructl deploy reports .
curl -w '%{time_total}s\n' 'http://127.0.0.1:8080/report?months=1'
curl -w '%{time_total}s\n' 'http://127.0.0.1:8080/report?months=3'
```

```
1 months: 15999998
0.394763s
3 months: 47999999
1.158561s
```

## Catch the worst case

Ask for a year:

```
curl -H 'Accept: application/json' 'http://127.0.0.1:8080/report?months=12'
```

Three seconds later:

```json
{"type":"about:blank","title":"Gateway Timeout","status":504,"detail":"The request ran past the soft timeout of 3 s.","reason":"soft_timeout","request_id":"01M611161EVA1Y37K66FTQF77W"}
```

When the request passed three seconds, the worker took its stack, then
raised `muru.RequestTimeout` in the request's thread, where the report
was running. The exception unwound the report like any other, and Muru
answered 504 with the reason `soft_timeout`. The worker carried on
serving: only the one request was stopped.

The event says where the request was:

```
muructl events
```

```
    16 10:31:31 warning  request.soft_timeout reports@r1               worker 0 (pid 41057): request 01M611161EVA1Y37K66FTQF77W GET /report?months=12 ran past the soft timeout of 3 s at report.py line 14 in summarise, called from report.py line 21 in build_report, called from report.py line 29 in application
```

and the release's log has the whole traceback, ending in the exception
that stopped it:

```
muructl logs reports
```

```
muru.worker[0]:   File "/home/you/reports/report.py", line 14, in summarise
muru.worker[0]:     total += row * row % 7
muru.worker[0]:     ^^^^^
muru.worker[0]: muru.RequestTimeout
```

An application that wants to answer for itself can catch
`muru.RequestTimeout` and return something else, such as a message that
the report will be emailed. A thread stuck where Python cannot interrupt
it, deep in a C extension, is what the hard timeout is for: it replaces
the whole worker.

## See a request in progress

A report of six months takes about two seconds and finishes inside the
timeout. Start one in the background and, while it runs, take the stacks
of the release's workers:

```
curl -s 'http://127.0.0.1:8080/report?months=6' &
muructl debug stacks reports
```

Among the threads, the one serving the request says which request it is,
how long it has been running, and where it is now:

```
muru-app-2 in worker 0:
  serving GET /report?months=6 for 1.5 s (request 01M6111F89VA1Y37K66FTQF77W)
  File "/home/you/reports/.venv/lib/python3.14/site-packages/muru/_pool.py", line 20, in serve_jobs
    serve()
  File "/home/you/reports/report.py", line 29, in application
    body = build_report(months)
  File "/home/you/reports/report.py", line 21, in build_report
    return f"{months} months: {summarise(rows)}\n"
  File "/home/you/reports/report.py", line 14, in summarise
    total += row * row % 7
```

This is what to reach for when a release seems stuck: it shows every
request in flight in every worker, and what each is doing, without
stopping anything.

## Profile it

A stack is one moment. A profile samples the stacks over a period and
counts where they were. Keep reports coming in another terminal:

```
while true; do curl -s 'http://127.0.0.1:8080/report?months=2' > /dev/null; done
```

and profile the release for ten seconds:

```
muructl debug profile reports --for 10s --output report.folded
```

```
630 samples of running Python code in 651 rounds across 1 worker
   64.8%  summarise (report.py:10)
   35.2%  fetch_rows (report.py:5)
muru: collapsed stacks written to report.folded
```

Two thirds of the time serving reports goes on summarising and a third
on waiting for the rows, so a faster summary is worth more than a faster
query. `report.folded` has each stack with its count, in the collapsed
format that flame graph tools read: `flamegraph.pl` draws it as an SVG,
and https://www.speedscope.app opens it in a browser.

Sampling every 10 milliseconds costs a worker little, so profiling a
release that serves real traffic is safe. Stop the loop with Ctrl-C.

## Clean up

```
muructl daemon stop --shutdown
```

## From here

- [Inspecting workers](../howto/debugging.md) covers profiling only the
  requests for one path, tracing every call, memory and allocation
  tracing, running code in a worker and attaching a debugger to a
  breakpoint.

- [Timeouts](../howto/timeouts.md) covers the hard timeout, the deadline
  a request has at the edge, and how a worker is stopped.

- The [dashboard](../howto/dashboard.md) shows the stacks and memory of a
  release's workers in a browser.
