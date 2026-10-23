# Fixture applications

One small application per supported style, each with its own test client
suite that the integration tests run over HTTP through Muru:

- plain WSGI
- plain ASGI
- Flask
- Django
- Starlette
- FastAPI
- a Channels-style WebSocket application
- a deliberately leaking application (memory policy tests)
- a slow-import monolith simulation (warm-up gating tests)

Each framework fixture also carries an upload adapter for offloaded uploads
(design specification section 4.11), to keep that contract honest.
