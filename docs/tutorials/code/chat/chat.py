async def application(scope, receive, send):
    if scope["type"] == "lifespan":
        await lifespan(scope, receive, send)
    elif scope["type"] == "http":
        await page(scope, receive, send)
    elif scope["type"] == "websocket":
        await chat(scope, receive, send)


async def lifespan(scope, receive, send):
    while True:
        message = await receive()

        if message["type"] == "lifespan.startup":
            scope["state"]["members"] = set()
            await send({"type": "lifespan.startup.complete"})
        elif message["type"] == "lifespan.shutdown":
            await send({"type": "lifespan.shutdown.complete"})
            return


async def page(scope, receive, send):
    members = scope["state"]["members"]
    body = f"{len(members)} in the chat\n".encode()
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"text/plain")],
        }
    )
    await send({"type": "http.response.body", "body": body})


async def chat(scope, receive, send):
    members = scope["state"]["members"]
    await receive()  # websocket.connect
    await send({"type": "websocket.accept"})
    members.add(send)

    try:
        while True:
            message = await receive()

            if message["type"] == "websocket.disconnect":
                return

            for member in list(members):
                if member is not send:
                    await member({"type": "websocket.send", "text": message["text"]})
    finally:
        members.discard(send)
