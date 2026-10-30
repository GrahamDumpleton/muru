# Secrets

```{admonition} Status
:class: note

Secret stores of the file, env and exec kinds, references from a
release's env table, delivery as variables or files, and `muructl secret`
for file stores. A delivered file belongs to the application's
[user](users.md).
```

Applications need credentials, and the configuration refers to them by
name so that exports, events, logs and dry runs never contain their
values. A secret lives in a store the daemon's configuration file
declares, and a release's environment refers to it by store and key.

## Stores

```toml
[secrets.store.local]
kind = 'file'                       # a directory of files, one per key

[secrets.store.ci]
kind = 'env'                        # variables of the daemon's own environment

[secrets.store.vault]
kind = 'exec'                       # a command that prints the value for a key
command = ['/usr/local/bin/vault-read', '{key}']
timeout = '5s'
```

A file store keeps each secret in a file named for its key, under the
daemon's state directory as `secrets/NAME` unless `path` names another
directory, readable only by the daemon's user. `muructl secret` manages
it, reading the value from standard input so that it never appears on a
command line or in a shell history:

```
printf '%s' "$PASSWORD" | muructl secret set local shop-db-password
muructl secret list local
muructl secret remove local shop-db-password
```

An env store takes the value from the daemon's environment, for
container and CI deployments where the platform injects it. An exec
store runs the command with `{key}` replaced by the key and takes its
output, which is how Vault, a cloud secret manager or the 1Password
command line are adapted; the command's exit status and the timeout
decide whether the value counts.

The stores are read from the daemon's configuration file when the daemon
starts and again on `muructl daemon reload` (or SIGHUP), so a store added
or changed is there for the next release to start; a release already
running keeps the secrets it was given. The [daemon
guide](daemon.md#reloading-the-configuration) lists what else a reload
applies.

## References

A release's `[env]` table, in a declared release or a project's
`[tool.muru]` table, holds plain values and secret references side by
side:

```toml
[app.shop.release.r42.env]
DJANGO_SETTINGS_MODULE = 'shop.settings'
DB_PASSWORD = { secret = { store = 'local', key = 'shop-db-password' } }
API_KEY = { secret = { store = 'vault', key = 'shop/api-key' }, as = 'file' }
```

`muructl deploy` takes the same with `--env NAME=VALUE` and `--secret
NAME=STORE:KEY`, or `--secret NAME=STORE:KEY:file`, over the table.

By default the value is delivered as the variable. With `as = 'file'` it
is written to a file under the release's run directory that only its
owner can read, and `NAME_FILE` holds the path, following the
`NAME_FILE` convention that Docker and many applications use. Values
are resolved when the release starts, from the stores as they are then;
rotating a secret therefore means deploying a new release, which gets
the usual drain and rollback. A missing key, an unknown store or a
failing command fails the release with a message naming the store and
key, and never the value.

## Handling

Values are held only as long as needed and never written to the state
store, exports, events, logs or dry-run output, nor passed on a command
line. `muructl config export` and `config diff` show the store and key;
`muructl secret list` shows names only. A development run (`muructl
run`) takes plain `[tool.muru.env]` values and refuses secret references,
since only the daemon has the stores.
