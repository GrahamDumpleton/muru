---
status: accepted
date: 2026-11-07
---

# ADR 0055: An apply prepares its releases, then commits the rest at once

## Context and problem statement

Section 10.10 said an apply lands entirely or not at all, producing one
new desired-state version and one snapshot, even if the daemon restarts
partway. The daemon carried out the plan one change at a time, with no
transaction around the store's writes, and a change that failed left
the earlier ones in place. A deploy that moved traffic pushed a snapshot
of its own, as did a promotion, before the apply's final one (issue
#439). ADR 0054 and issue #449 had narrowed the gap: a release an apply
adds no longer takes traffic as it starts, and a promotion runs after
the hosts and routes are written. What was left were partial failures,
writes that a restart could leave half done, and more than one snapshot.

The question is how an apply makes the guarantee section 10.10 states.

## Decision drivers

- A route must not point at a release that has not started, so the
  releases an apply adds start before anything points at them.

- Starting a release takes seconds or minutes, and its processes cannot
  be part of a database transaction.

- The routes and the release that serves each application reach the
  edge together.

- A restart at any point leaves desired state that the next apply of the
  same files completes.

- Muru runs on one node, with one state store and one edge.

## Considered options

- Carry out the plan one change at a time, as before, and say in
  section 10.10 that an apply is not atomic.

- Two phases: start the new releases held, then write everything else in
  one transaction and push one snapshot.

- Desired-state versions, with a reconciler that converges the node on
  each version as a whole.

## Decision outcome

Chosen option: two phases, because they give the guarantees that matter
with the store and commands Muru has. Only the releases take time, and
they can start held without changing what anything serves, so they
start first and are removed again if one fails. Everything that points
at them is a row in the state store, so it is written in one SQLite
transaction. The daemon's command loop carries out the commit phase as
one command, so no other snapshot comes between the writes and the one
the edge gets. Desired-state versions would redesign how deploys,
rollouts and promotions work, which one node does not need.

The identity and priority of an application are written in the prepare
phase, since a release starts with them, and put back if the apply
fails. A release the daemon had stopped that a route the files write
names is started in the commit phase, before the snapshot, as the
reconciler starts any release a policy names; one that cannot start is
an event, as before, and does not stop the apply. `--no-wait` answers
once the releases are recorded, and the daemon finishes the apply
without the client. Removals by `--prune` of routes and hosts are in the
transaction, and of releases and applications after the snapshot, since
a running release is drained before its record goes.

### Consequences

- Good: routes and traffic change all at once or not at all, and a
  release the apply adds is ready before anything points at it.

- Good: a release that does not start stops the apply with nothing else
  changed, and the report and a `config.apply_failed` event name it.

- Good: a promotion's traffic switch is part of the apply's one
  snapshot, with the routes the same files write.

- Bad: an apply that a restart interrupts between the phases leaves its
  releases recorded and held until the files are applied again.

- Bad: with `--no-wait` the client does not learn the outcome, which an
  event records.

- Good: applies happen one after another, each planned against what
  the one before it left (issue #453).

## More information

Section 10.10 says what the apply guarantees and what it does not;
section 10.4 describes the plan. Issue #439 records the gap this
closes, and issue #452 the saved plans split out of it. ADR 0054
describes how files name the release that serves. Issue #453 ordered
applies, which this record first left unordered: one given while another
is under way waits for it, rather than being refused, so that jobs that
apply at the same moment all land without having to try again.
