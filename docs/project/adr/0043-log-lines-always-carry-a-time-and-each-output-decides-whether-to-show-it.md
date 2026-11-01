---
status: accepted
date: 2026-11-02
---

# ADR 0043: Log lines always carry a time, and each output decides whether to show it

## Context and problem statement

Only two kinds of line carry a time today: the access log (a UTC clock on
the terminal, the full time in its JSON) and Python logging records in the
json format. Worker output, the host's lines and muructl's own `muru:`
lines have none, and the daemon's tracing lines use another layout, so a
release's log file has no times at all (issue #93). The same lines reach
very different readers: a developer watching a terminal, a log file read
after an incident, the dashboard, journald, and a container runtime whose
log collector stamps every line itself. The line format is something
people and tools parse, so it is settled here. Section 11.8 of the
specification holds the result.

## Decision drivers

- A time that was not recorded when a line was written cannot be shown
  later, by a flag, the dashboard or a collector.

- Records are read on other machines and by tools: they need one
  unambiguous, sortable form.

- A person watching their own output live does not want a time on every
  line, and a platform that stamps lines itself does not need a second
  one.

- Access log lines are request records, and their time (when the request
  arrived) is part of the record, as in other servers' access logs.

- `muructl serve` in a container writes to standard output, where the
  container runtime stamps each line and a collector may class standard
  error as errors.

## Considered options

- A time on every line everywhere, in one format.

- An `auto` mode that guesses from the destination, with a `[logs]
  timestamps` key to override it in files as well.

- Always record the time; files and records always carry it in full UTC;
  standard output leaves it off application lines by default, with a flag
  to add it; readers (`muructl logs`, the dashboard) show it.

## Decision outcome

Chosen option: always record the time, and let each output decide whether
to show it.

- **The time.** An access record's is when the request arrived; a json
  logging record's is its creation time; any other line's is when it
  reached the process that tags it. Durations come from the monotonic
  clock.

- **Two forms.** The full form is RFC 3339 in UTC with milliseconds and a
  `Z`. The short form is a clock in local time, used only on a terminal,
  since a clock with no zone is read as the reader's own. A short clock in
  UTC is never shown.

- **Order.** Time, then tag, then message, written in one write by one
  helper for Muru's own lines, with the tracing output in the same
  layout.

- **Standard output and standard error** of `muructl run`, `muructl serve`
  and `muructl daemon run`: no time in front of application and Muru
  lines; `--timestamps` or `[logs] timestamps = true` adds the full form to
  every line. Access lines keep their time: the short clock on a
  terminal, the full form otherwise.

- **Files** always carry the full form, with no setting to leave it out.
  A process decides by what its stream is: a regular file (a release's
  log, `edge.log`, `daemon.log`, or output redirected to a file) is
  stamped, a terminal or a pipe is not. The access log file is JSON
  lines only.

- **JSON records** carry the time as a `time` field, never as a prefix, in
  every kind of record.

- **journald** gets no time in front; it stamps lines itself.

- **Readers show it.** `muructl logs` shows the full form by default, with
  `--no-timestamps` to hide it. The dashboard shows a time column with the
  viewer's local clock and the full form on hover, and its buffer keeps a
  time with every line.

- **One stream under `serve`.** Application, Muru and access lines all go
  to standard output.

### Consequences

- Good: every line can be placed in time after the fact, and the time is
  in the file, so `grep`, `sort` and later `muructl logs --since` work on
  it.

- Good: live output stays readable, and output a platform stamps is not
  stamped twice.

- Good: one full form everywhere a record is kept, and one field name in
  JSON, so collectors need no per-kind parsing.

- Bad: the access line's terminal clock changes from UTC to local time,
  so it no longer matches the hour in the JSON on a machine not set to
  UTC; the `Z` on the full form is what tells them apart.

- Bad: a relayed line's time is when it reached the relay, not when the
  application wrote it, so block-buffered output arrives in bursts with
  one time. Line-buffered standard output keeps the gap small.

- Bad: an application that sets `%(asctime)s` in its own logging format
  gets two times in files.

## More information

- Specification sections 11.2, 11.4, 11.8 and 13.1.

- Issue #93 (times on log lines), #107 (logs for container log
  collectors) and #102 (`muructl serve`).
