# Client classes and rate limits

```{admonition} Status
:class: note

Client classes, rate limits, method restrictions and route site modes,
under the daemon. Classes and limits are declared in the daemon's own
configuration file; routing by class and modes set by overlays are
still to come.
```

The edge gives every request a client class, which the access log,
metrics, rate limits and the application see. Classification is for
protecting capacity and routing, not a web application firewall: a
deployment that needs serious bot defence puts a CDN or WAF in front and
feeds its verdict in as a header.

## Classes

`[[client_class]]` rules go in the daemon's configuration file, where
they are tried in order; the first whose inputs all match names the
class, and a request that matches none is class `default`.

```toml
[[client_class]]
name = 'search_bot_verified'
user_agent = ['Googlebot', 'bingbot', 'DuckDuckBot', 'Applebot']
verify = 'dns'
verify_domains = ['googlebot.com', 'google.com', 'search.msn.com', 'duckduckgo.com', 'applebot.apple.com']

[[client_class]]
name = 'ai_crawler'
user_agent_list = 'lists/ai-crawlers.txt'

[[client_class]]
name = 'unverified_bot'
user_agent = ['bot', 'crawler', 'spider', 'Googlebot']

[[client_class]]
name = 'partner'
addresses = ['203.0.113.0/24']
```

| Input | Matches when |
| --- | --- |
| `user_agent` | the User-Agent contains one of the substrings, without regard to case |
| `user_agent_regex` | the User-Agent matches one of the regular expressions |
| `user_agent_list` | the User-Agent contains a substring from the file |
| `addresses` | the client's address is one of these addresses or networks; `loopback` and `private` work too |
| `address_list` | the client's address is in a network from the file |
| `headers` | each named header is present and matches its regular expression |
| `cookies` | each named cookie is present and matches its regular expression |

A rule's inputs must all match, and an input with several entries
matches when any of them does; `user_agent`, `user_agent_regex` and
`user_agent_list` together are one input. The client's address is the
one a [trusted proxy](behind-a-proxy.md) forwarded, so a CDN's verdict
header, say `headers = { 'X-Bot-Score' = '^[0-2]?[0-9]$' }`, classifies
what reaches you through it. Class names are lower case letters, digits,
`_` and `-`.

`verify = 'dns'` confirms a claimed crawler: the client's address must
reverse-resolve to a name that is, or ends in, one of `verify_domains`,
and that name must resolve back to the address. A lookup never holds a
request. The first request from an address starts one, and until it has
confirmed the address the rule does not match, so the request falls
through to the later rules, the `unverified_bot` above for instance.
Confirmed names are remembered for 24 hours and failures for an hour.

List files hold one entry a line; blank lines and lines starting with
`#` are ignored. A path is relative to the configuration file's
directory, and a list that changes is read again within a second, which
the `client_class.list_reloaded` event records. Community lists of AI
crawler user agents or a provider's address ranges can be dropped in as
they are.

The class appears in the access log as `class=` (for anything but
`default`) and as `client_class` in JSON lines, as the `client_class`
label of `muru_requests_total`, and to the application as
`muru.client_class` in the environ or `client_class` in the scope's
`extensions["muru"]`.

## Rate limits

A `[rate_limit.NAME]` table in the daemon's file is a token bucket for
each key, and hosts and routes name the limits their requests draw from:

```toml
[rate_limit.crawlers]
match = { client_class = ['ai_crawler', 'unverified_bot'] }
key = 'address'
rate = '2/s'
burst = 10
action = 'reject'
headers = true
```

```toml
[host.shop]
names = ['shop.example.com']
rate_limits = ['crawlers']

[[route]]
host = 'shop'
path = '/search'
app = 'shop'
rate_limits = ['search']
```

| Key | Default | Purpose |
| --- | --- | --- |
| `match.client_class` | every class | the classes whose requests draw from the limit |
| `key` | `address` | what a bucket is kept for: `address`, `client_class`, `header:NAME`, `cookie:NAME`, or a list combining them |
| `rate` | required | how fast a bucket fills: `2/s`, `100/m` or `1000/h` |
| `burst` | the rate's count | how many requests a full bucket allows at once |
| `action` | `reject` | `reject` answers 429; `delay` holds the request until a token comes, up to `max_delay`, and refuses beyond it; `log` only counts |
| `max_delay` | `1s` | the longest hold under `delay` |
| `ipv4_prefix`, `ipv6_prefix` | 32, 64 | how much of an address makes the key, so that an IPv6 client's /64 shares one bucket |
| `headers` | false | send the `RateLimit-Policy` and `RateLimit` fields with a 429 |

A request draws from the host's limits, then the route's. A refusal is
429 `rate_limited` with `Retry-After`, and a page configured for 429 or
`429:rate_limited` replaces the built-in one (see the [error pages
guide](error-pages.md)). Every refusal, hold and logged request counts in
`muru_rate_limited_total` by limit, class and action, so `action = 'log'`
shows what a limit would do before it is enforced, and the
`ratelimit.enforcing` event records a limit beginning to refuse or hold
requests after a quiet minute.

Buckets belong to the edge process: idle full ones are dropped, and
every bucket starts full again when the edge is replaced.

## Method restrictions and site modes

A route can refuse methods, answered with 503 `read_only` or, with
`answer = 'method_not_allowed'`, with 405 and the remaining methods in
`Allow`:

```toml
[[route]]
host = 'shop'
path = '/'
app = 'shop'
methods = { deny = ['POST', 'PUT', 'PATCH', 'DELETE'] }
mode = 'read-only'
```

A route's `mode` is any short word, passed to the application as
`muru.mode` in the environ or `mode` in the scope's `extensions["muru"]`,
so it can hide or disable what the mode rules out. Both are the usual
way to take a site read-only for maintenance of its database, and
`muructl apply` checks them, with the rate limits a host or route names,
against the daemon's file.

## Changing them

`muructl daemon reload`, or SIGHUP, reads the daemon's file again and
gives the edge changed classes and limits without a restart. An applied
file's `[[client_class]]` and `[rate_limit]` tables are reported as
belonging in the daemon's file and are not applied.
