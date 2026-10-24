# Explanation

Background and reasoning: why Muru works the way it does. Pages here
describe the design Muru is being built to, and each says at the top how
much of it exists today. The [design specification](../project/design/index.md)
covers the same ground in more detail, for contributors.

Still to be written:

- Releases and traffic policies.
- How scaling decides.
- Security model.
- Comparison with mod_wsgi, gunicorn, uvicorn and Granian.

```{toctree}
:maxdepth: 1

architecture
worker-protocol
process-models
preload
```
