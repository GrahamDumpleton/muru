# Fixture applications

One small application per supported style, each with its own test client
suite that the integration tests run over HTTP through Muru:

- plain WSGI (`plain_wsgi.py`, `wsgi_conformance.py` for the conformance
  suite, and `preload_wsgi.py` and `unsafe_import_wsgi.py` with the
  `hooks_*.py` modules for the lifecycle tests)
- plain ASGI
- Flask (`flask_app.py`)
- Django (`django_site/`)
- Starlette
- FastAPI
- a Channels-style WebSocket application
- a deliberately leaking application (memory policy tests)
- a slow-import monolith simulation (warm-up gating tests)

Each framework fixture also carries an upload adapter for offloaded uploads
(design specification section 4.11), to keep that contract honest.
