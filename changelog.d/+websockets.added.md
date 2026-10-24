`muructl run` serves WebSockets to ASGI applications: the edge completes
the handshake once the application accepts (with its subprotocol and
headers), relays whole text and binary messages in both directions, carries
close codes each way, closes a socket with 1009 for a message over 1 MiB,
pings idle clients and closes a silent socket after five minutes. Closing
before accepting answers 403, and the `websocket.http.response` extension
lets the application decline with a full response. HTTP/2 extended CONNECT
is accepted as well.
