# Database migrations

A deploy runs the new release beside the old one until the new one is
ready, and a canary, a split or a sticky grace keeps both serving for
longer. All that time both releases use the same database, so a schema
change has to work with the code of both. This guide covers changing a
schema in steps that keep it so, and running each step with
[`muructl exec`](commands.md).

## Expand, deploy, contract

Split a change that would break the old release into three steps:

1. **Expand.** Add what the new code needs without removing anything the
   old code uses: a new table, a new nullable column, a new index. Run the
   migration while the old release serves; it does not notice.

2. **Deploy.** Deploy the release that uses the new schema. While both
   releases serve, the new one must cope with rows the old one writes,
   and the old one with rows the new one writes.

3. **Contract.** Once no release that uses the old schema serves or is
   retained for a rollback, remove what only it used: drop the old column,
   make the new one required. This is a migration of its own, shipped in
   a later release.

Renaming a column is the usual example. Expand adds the new column and
has the new code write both and read the new one, falling back to the
old; a backfill copies the old values across; contract drops the old
column a release or two later. Changes that already work with both
releases, such as adding a table or an index, need only the first step.

## Running the migration

Run a migration with the release whose code it belongs to, so it uses
that release's migration files and settings. With the expand step in the
new release, hold the release, migrate with it, then move traffic:

```
muructl deploy shop . --hold
muructl exec shop@r8 -- python manage.py migrate --noinput
muructl rollout bluegreen shop@r8
```

The held release starts with no traffic, so its workers are up before
the schema changes and nothing reaches them until it is promoted. For a
[canary](rollouts.md), start the rollout after the migration in the same
way.

With Alembic, or any tool the environment installs, the same applies:

```
muructl exec shop@r8 -- alembic upgrade head
```

Run the contract step with the release that removes the old code's last
use, once it serves and the release before it is gone, its retention
window over or drained:

```
muructl exec shop -- python manage.py migrate --noinput
```

`muructl exec` exits with the migration's status, so a script or CI job
that deploys can stop at a failed migration before it moves any traffic.

## Rolling back

A rollback moves traffic to the previous release but leaves the schema
as it is. After an expand step that is safe by design, since the old
release works with the expanded schema. Undo a migration only after
rolling back, and only when the release that serves no longer needs it,
with that release's code:

```
muructl rollback shop
muructl exec shop -- python manage.py migrate orders 0006
```

## Caches, queues and files

The same holds for anything else both releases share. Cached values and
queued messages written by one release are read by the other, so add
fields rather than change their meaning, and give a cache entry whose
shape changes a new key, with a version in the key say. Files written to
shared storage follow the same rule.
