# Traffic between releases

A route sends every request to its application's serving release unless
it has a `traffic` table. With one, the route shares its requests between
the releases the table names and the serving release, keeps a client on
the release it first got when told to, and lets a preview reach a release
that takes no share. This is the mechanism canaries, A/B splits and dark
launches are built on.

## Splitting requests

```toml
[[route]]
host = "shop"
app = "shop"

[route.traffic]
split = { r2 = 10 }
```

`split` gives each release a percentage of the route's requests, by
release id. The application's serving release takes whatever is left, so
this route sends one request in ten to `r2` and the rest to the serving
release. The percentages are whole numbers and may add up to at most 100.
Applying a policy that names a release which does not exist is refused.

Clients are not picked at random request by request. Each client is
hashed to one of 10,000 buckets, and each release in `split` owns a
contiguous range of them, the newest release's at the top. A client that
lands in `r2` at 10% is still in `r2` when the share grows to 25%, and
the same client gets the same release in every edge process and across
edge upgrades.

A release named in `split` is kept running. If it was retained after a
deploy it comes back to ready; if it was stopped it is started. When a
policy stops naming a release that is not the serving one, the release
drains and is retained as it would be after a deploy. A deploy or rollback
that would drain a release some policy still names leaves it running.

## The key

What a client is hashed by is the `hash_key`:

| `hash_key` | The key |
| --- | --- |
| `cookie` (the default) | An anonymous id the edge issues as the `muru_id` cookie on a client's first request, kept for a year |
| `header:NAME` | The value of that request header, for example a user id set by a trusted proxy or a gateway |
| `cookie:NAME` | The value of that cookie, for example a session cookie the application sets |
| `client` | The client's address |

A header or cookie key that a request does not carry falls back to the
client's address.

A `salt` is mixed into the hash, so two routes with the same key spread
the same clients differently. Without one the route's host and path are
the salt, so two routes of one application make independent splits
unless they are given the same salt.

## Sticky clients

```toml
[route.traffic]
split = { r2 = 25 }
sticky = true
sticky_grace = "10m"
```

With `sticky = true` the edge sets a `muru_release` cookie naming the
release a client was given, and honours it on later requests as long as
that release serves the route. The cookie is ignored once the release
stops, and a cookie naming a release the route does not know is ignored
too.

When a sticky release leaves the route, because the policy no longer
names it or a deploy supersedes it, it stays reachable through the cookie
for `sticky_grace` (10 minutes by default) and then drains. New clients
stop landing on it at once. Its state shows as draining during the grace,
with a message saying so.

## Previews

A release with a share of `0` takes no ordinary traffic but can still be
reached by name, which is how a release is checked before it gets a
share:

```toml
[route.traffic]
split = { r2 = 0 }
```

A request carrying the `Muru-Preview: r2` header or a `muru_preview=r2`
cookie goes to `r2` whatever the policy says, as long as `r2` is one of
the route's releases. A preview naming an unknown release is ignored and
the request is routed as usual. The serving release can be previewed the
same way, which is useful while a split sends most clients elsewhere.

The preview header is not an `X-Muru-` header, so it is not among the
headers the edge removes from requests.

## Seeing the policy

`muructl route list` shows each route's shares, stickiness and key:

```text
HOST             PATH                     TARGET
shop             /                        shop (r2 10%, sticky, by header:x-user)
```

Applying a changed policy records a `traffic_policy.changed` event naming
the route and the new policy. A release that a policy names but that
cannot start records `traffic_policy.release_unavailable`.

The response to every request names the release that served it in the
access log, and an application can read its own release id from
`muru.context().release`.
