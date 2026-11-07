---
status: accepted
date: 2026-11-07
---

# ADR 0054: An application names the release that serves

## Context and problem statement

`muructl apply` deployed each release a file declared that the daemon did
not have, and a deploy moved the application's traffic to it, so the last
declared release in id order served (section 10.4). Nothing in a file
could say that a new release is not to serve. A file that declared a new
release beside the serving one, with a route split giving it 0%, had it
take every request for a moment: the deploy switched traffic and drained
the old release before the apply wrote the route (issue #420).
`--hold` held every release an apply added, in every application, and is
a flag, so a job applying whatever the repository holds could not stage
one release and promote another. Nor could files promote a release the
daemon already had. With `--prune`, files that left out the serving
release removed it.

The question is how files say which release serves.

## Decision drivers

- Files kept in version control describe the state wanted, including
  which release takes the application's traffic, so that applying them
  needs no flags from a person.

- A release is staged, previewed, promoted and removed through files
  alone, and an export says which release serves, so that it applies
  elsewhere with the same outcome.

- ADR 0052 rejected a key in a release's table that holds it: where
  traffic goes belongs to routes and the application, not to a release,
  and a release's table is immutable once recorded.

- Files without the new key keep meaning what they meant.

- A rollout under way moves the serving release itself, so files must not
  silently cut it short.

## Considered options

- A release a route's split names, in the same files, starts held.

- The job that applies the files always passes `--hold`.

- A key in the application's table that names the release that serves.

## Decision outcome

Chosen option: a key in the application's table, `serving`, because it is
the only one that covers the whole of a release's life in files, and it
leaves files without the key as they were. It is on the application, not
on a release, so ADR 0052's reason for rejecting a hold key does not apply:
it says where the application's own traffic goes, which no route can say,
and changing it changes no release.

- **The key names a declared release or one the daemon has.** Naming
  neither is refused. Without the key, each release an apply adds serves
  in turn, so the last in id order serves in the end, and `--hold` holds
  them all.

- **The other releases an apply adds are started held.** They do not
  take the application's traffic, with or without a route split, and
  serve only what a split, a preview or a rollout gives them.

- **Naming a release the daemon has promotes it.** The plan gains a
  `promote` action, with the switch and the drain a promotion makes, kept
  warm as `--retain` and `--retain-workers` say. Promotions come after the
  hosts, routes and overlays, so a split in the same files protects the
  release it names from the drain. A deploy of the release the key names
  stays where deploys are; the order of the apply's other changes is
  issue #439's.

- **`--hold` with a key that moves traffic is refused.** The flag says
  traffic stays where it is and the key says it moves, so the apply asks
  for one or the other rather than choosing.

- **A rollout under way decides what serves.** While one runs, is paused
  or awaits approval, the key may name its baseline or its candidate, and
  nothing is promoted. Naming the baseline warns that, once the rollout
  promotes the candidate, applying the same files promotes the baseline
  back. Naming any other release is refused, with `muructl rollout
  promote` and `muructl rollout abort` as the way to end the rollout
  first. No flag overrides it: `--override-freeze` answers an overlay's
  freeze, which is another thing, and an apply that ended a rollout would
  skip the gates that judge it, which the rollout commands already do
  explicitly.

- **`--prune` needs the release that serves declared.** The release the
  key names, or without the key the one that serves after the apply, must
  be declared, or the plan refuses, since pruning would remove it.

- **Export writes the key.** It names the release that serves, so an
  export applies back without a change and, on another node, the same
  release serves and the others are held.

- **The plan says what happens to each release.** A deploy's detail
  starts `held:`, `serving:` or, without the key, `serving until APP@ID
  replaces it:`, and a promotion says which release it takes over from.

### Consequences

- Good: a release is staged by declaring it and promoted by changing one
  line, with no flags, which is what a job applying a repository needs.

- Good: `--prune` cannot remove the release that serves, and an export
  round-trips which release serves.

- Good: an application's releases can be held while another's are not,
  in one apply.

- Bad: files that name a release keep naming it. A rollout started by
  hand that promotes another, or a promotion by hand, is undone by the
  next apply of unchanged files. The warning during a rollout says so,
  and nothing warns after one.

- Bad: a rollout that rolls back leaves its candidate named, if the files
  were changed to name it. The next apply refuses to promote a release
  whose latest rollout rolled back, so a job applying the files fails
  until they name another release or a person promotes it by hand with
  `muructl rollout bluegreen`, after which the files agree (issue #448).

- Bad: files without the key still let the release an apply adds serve
  before a split in the same files is written.

- Good: an added release that neither serves nor is named by a route's
  traffic or an overlay in the same files is recorded and left stopped,
  so staging a release costs nothing until a rollout, a promotion or a
  split starts it (issue #449). `--hold` still starts every one held.

- Bad: a stopped release cannot be previewed until something starts it,
  since the `Muru-Preview` header reaches only a release that runs: a
  route split giving it 0% starts it held for previews.

## More information

Specification sections 9.2, 10.4 and 19.1. Issues #417 and #420; the
apply's order and atomicity are issue #439. ADR 0052 rejected a hold key
on a release's table. Issue #447 later held, without the key, every
release an apply adds but the last in id order, so the plan's `serving
until` detail went. Issue #448 refused promoting a rolled-back candidate,
where the next apply had promoted it without its gates. Issue #449 left
stopped an added release that nothing serves or names, where it had been
started held.
