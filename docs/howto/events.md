# Events and webhooks

```{admonition} Status
:class: note

The daemon records every change of state as an event, `muructl events`
shows or follows them, and webhook sinks receive them as signed
CloudEvents requests. OTLP, journald and file sinks, and the audit log,
are still to come.
```

## The event stream

Every change the daemon makes or notices is an event: a release created,
ready, draining or stopped, traffic switched, a host restarted, the edge
replaced, logs reopened, a certificate that could not be read. Each has
a node-wide sequence number, a time, a type such as `release.ready`, a
subject such as `shop@r1`, a level (`info`, `notice`, `warning` or
`error`) and a sentence.

```
$ muructl events
    41 12:20:01 info     release.created      shop@r3                  created from /srv/shop
    42 12:20:03 info     release.ready        shop@r3                  ready: 2 workers listening
    43 12:20:03 info     traffic.switched     shop                     traffic moved to shop@r3
$ muructl events --follow --after 43
```

`--follow` keeps printing as events happen and `--after N` starts after a
sequence number, so a watcher that reconnects misses nothing. Events are
kept in the state store, the last ten thousand of them.

## Webhooks

A `[[events.sink]]` entry in the daemon's configuration file names a URL
to deliver events to:

```toml
[[events.sink]]
name = "ops"
url = "https://hooks.example.com/muru"
secret_file = "/etc/muru/hooks/ops.secret"
types = ["release.*", "traffic.*", "edge.*"]
levels = ["notice", "warning", "error"]
```

| Key | Effect |
| --- | --- |
| `name` | names the sink; events about the sink itself carry it as their subject |
| `url` | where deliveries go, `http://` or `https://` |
| `secret`, `secret_file` | the key deliveries are signed with, inline or read from a file; one of them is required |
| `types` | patterns the event type must match (`*` matches anything); every type when omitted |
| `levels` | levels delivered; every level when omitted |

Each delivery is an HTTP POST with one event as CloudEvents 1.0 JSON:

```json
{"specversion": "1.0", "id": "42", "source": "muru://node1",
 "type": "dev.muru.release.ready", "subject": "shop@r3",
 "time": "2026-10-24T12:20:03.418Z", "datacontenttype": "application/json",
 "muruseq": 42, "level": "info",
 "data": {"message": "ready: 2 workers listening"}}
```

The headers carry the same identification and the signature:

| Header | Value |
| --- | --- |
| `Content-Type` | `application/cloudevents+json` |
| `X-Muru-Event` | the type |
| `X-Muru-Delivery` | the id, which is the sequence number |
| `X-Muru-Signature` | `sha256=` and the hex HMAC-SHA256 of the body with the secret |

A receiver checks the signature over the exact bytes of the body before
trusting a delivery:

```python
import hashlib
import hmac

def verify(secret: bytes, body: bytes, header: str) -> bool:
    expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header)
```

Any 2xx response accepts the delivery. Anything else, or no answer within
ten seconds, is retried with a wait that doubles from one second to a
minute, for as long as it takes: deliveries to a sink happen one at a
time and in order, from a cursor the daemon keeps in its state store, so
a receiver that is down gets everything it missed once it is back, and a
daemon restart carries on where the last one stopped. A receiver can
therefore see an event twice, when the answer was lost, and the id tells
it so. A sink named for the first time starts from the events after it
was added. After three failed attempts in a row the daemon records an
`events.sink.failing` warning, and an `events.sink.recovered` notice when
deliveries get through again; both are delivered to the sink too, when
its filters allow.
