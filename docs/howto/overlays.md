# Overlays

```{admonition} Status
:class: note

Overlays activated by hand or by a one-off or recurring schedule, over
routes and applications: site modes, method restrictions, traffic pins
and splits, route targets, rate limits and maintenance mode, with
preconditions, warm-up, advisory and latched schedules, previews and
promotion. Scheduled capacity and deploy freezes, the external trigger,
and overlays that activate on conditions are still to come.
```

An overlay is a named piece of configuration laid over the routes and
applications while it is active. The configuration underneath never
changes: when the overlay ends, the routes are as they were, including
any change made to them in the meantime. One mechanism covers a timed
switch to a new release, a read-only period while a database is backed
up, or a maintenance window.

```toml
[[route]]
name = 'shop-root'
host = 'shop'
path = '/'
app = 'shop'

[overlay.nightly-backup]
routes.shop-root = { mode = 'read-only', deny = ['POST', 'PUT', 'PATCH', 'DELETE'] }
trigger = { schedule = { cron = '0 2 * * *', duration = '1h', timezone = 'Australia/Sydney' } }

[overlay.cutover-r43]
routes.shop-root.traffic = { pin = 'r43' }
trigger = { schedule = { start = '2026-11-02T09:00', timezone = 'Australia/Sydney' } }
warm_up = { start_release = 'r43', lead = '30m' }
preconditions = { release_ready = 'r43' }
```

Overlays are declared beside routes, in the files `muructl apply` reads,
and reach routes by the route's `name`.

## What an overlay changes

| Key | Effect |
| --- | --- |
| `routes.NAME.mode` | The [site mode](client-classes.md#method-restrictions-and-site-modes) the application sees as `muru.mode` |
| `routes.NAME.deny`, `answer` | Methods refused, with 503 `read_only` or, with `answer = 'method_not_allowed'`, 405 |
| `routes.NAME.traffic` | `{ pin = 'r43' }` sends every request to one release of the route's application; `{ split = { r43 = 20 } }` replaces the route's shares |
| `routes.NAME.target` | What serves the route instead: `{ app = 'NAME' }`, `{ static = '/srv/snapshot' }` or `{ upstream = 'NAME' }` |
| `routes.NAME.rate_limits` | The [rate limits](client-classes.md) in place of the route's |
| `apps.NAME.maintenance` | Maintenance mode on (`true`) or off (`false`) for the application |

While an overlay pins or splits a route's traffic, a rollout of that
route's application waits, and carries on when the overlay ends.

Two overlays that set the same field cannot both be applied unless each
has a different `priority`; the higher one's value stands while both are
active. `muructl apply` refuses overlays that would conflict, or that
name a route, application, upstream, directory or rate limit that does
not exist.

## Activating

By hand:

```
muructl overlay activate nightly-backup
muructl overlay activate nightly-backup --for 2h
muructl overlay activate nightly-backup --until 2026-11-02T06:00 --timezone Australia/Sydney
muructl overlay end nightly-backup
```

By schedule: `trigger.schedule` gives either a `start` and an optional
`end`, for one window, or a `cron` expression and a `duration`, for a
window that recurs. Times are local times in `timezone` (an IANA name
such as `Australia/Sydney`, the daemon's own zone without one); a `start`
or `end` may instead be a time with an offset. Daylight saving is
handled: a local time the clocks skip is read as the time after the
change, and one they repeat as the first of the two. An overlay with a
start and no end stays active until it is ended or promoted. Ending a
scheduled overlay by hand keeps it ended until its window is over.

A cron expression has five fields, minute, hour, day of month, month and
day of week, each a number, `*`, a range such as `1-5`, a step such as
`*/15` and lists of these, with month and day names (`jan`, `mon`) and
Sunday as 0 or 7. `@hourly`, `@daily`, `@weekly`, `@monthly` and
`@yearly` stand for the usual expressions. When both the day of month
and the day of week are given, a day matching either matches, as in
cron. When recurring windows overlap, the overlay stays active from the
first to the end of the last.

The daemon works out which overlays should be active from the time and
what it has recorded, every second, so one restarted in the middle of a
window carries on with it.

## Advisory and latched schedules

With `advisory = true`, the schedule only raises an
`overlay.would_activate` event as each window begins, saying whether the
preconditions held, and the overlay is activated by hand only. It suits
trying out a schedule before letting it act.

With `latch = true`, an overlay its schedule activates stays active after
the window ends, until a person ends it.

## Warm-up

`warm_up` starts a release ahead of each window of the schedule, so a
switch to it lands on warm workers:

```toml
warm_up = { start_release = 'r43', lead = '30m' }
```

For `lead` before the window begins, the release runs, started again if
it was stopped or retained, without taking traffic; the event
`overlay.warming` says so. The release is named as for `release_ready`.
If the overlay does not activate, nothing else keeps the release
running, and it is retained as a superseded release is.

## Preconditions

`preconditions.release_ready` names a release that must be ready, or
retained and warm, for the overlay to activate: `app@release`, or just
the release when the overlay changes routes of one application. When it
does not hold, an activation by hand is refused, and a scheduled one is
skipped for the rest of its window with the event `overlay.skipped`, so a
timed switch never moves traffic to something broken.

## Previewing and promoting

```
muructl overlay list
muructl overlay preview cutover-r43
muructl overlay promote cutover-r43
```

`list` shows each overlay's state, its trigger and what it changes.
`preview` shows what activating it now would change, field by field.
`promote` folds an active overlay's changes into the base configuration,
the routes and maintenance settings as the overlay leaves them, and ends
it for good, which is how a timed switch becomes the new normal. Remove a
promoted overlay from the applied files afterwards.

Activations, ends, skips, advisory windows, warm-ups and promotions are
the events `overlay.activated`, `overlay.ended`, `overlay.skipped`,
`overlay.would_activate`, `overlay.warming` and `overlay.promoted`.
