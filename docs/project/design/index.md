# Design specification

Muru is a routing edge, control plane and Python runtime for serving WSGI and ASGI applications, with zero-downtime releases, traffic management and Python-aware process management.

## How the specification is maintained

- This copy is the source of truth for intended design. It changes through pull requests like code, and its history is the git history.

- Each page carries a banner saying that it describes intended design for contributors. The user documentation (tutorials, how-to guides and reference) is authoritative for released behaviour.

- When implementation shows that the specification needs to change, the change is made in the same pull request as a new [architecture decision record](../adr/index.md) explaining why. Nobody deviates from the specification silently.

- Section numbers are kept stable so that issues, pull requests and decision records can refer to them. New material is added as new subsections rather than by renumbering.

- Cross references in the text of the form "section 4.9" refer to the numbered sections and appendices listed below.

## Sections

```{toctree}
:maxdepth: 1

01-overview.md
02-architecture.md
03-data-model.md
04-edge.md
05-worker-protocol.md
06-python-runtime.md
07-process-management.md
08-interpreters-environments-and-containers.md
09-releases-and-traffic-management.md
10-control-plane.md
11-observability.md
12-security-model.md
13-operations.md
14-implementation-plan.md
15-testing-strategy.md
16-continuous-integration.md
17-packaging-and-release.md
18-documentation.md
19-decisions-open-questions-and-risks.md
20-future-ideas.md
appendices/index
```
