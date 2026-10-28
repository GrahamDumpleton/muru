Added worker inspection to the daemon's dashboard: each running release
links to a page of its workers, with their status refreshed every two
seconds and their stacks merged across workers, and each worker to a page
of its own with its status, stacks, asyncio tasks and, on request, its
memory, as `muructl debug` shows them; the same answers are served as JSON
at `/v1/debug/OP?release=APP@RELEASE`.
