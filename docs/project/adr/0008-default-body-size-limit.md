---
status: accepted
date: 2026-10-23
---

# ADR 0008: Request bodies are limited to 10 MB by default

## Context and problem statement

An edge that buffers or streams request bodies needs a default limit. gunicorn
and uvicorn impose none; Apache and mod_wsgi-express default to 10 MB
(specification section 4.6).

## Decision drivers

- Protect spool space, memory and Python threads from unbounded uploads by
  default.

- Make the limit discoverable when it bites.

## Considered options

- No limit by default, matching gunicorn and uvicorn.

- 10 MB by default, matching mod_wsgi-express, settable per host, route,
  mount and group.

## Decision outcome

Chosen option: 10 MB. The 413 page names the limit in effect and the setting
that changes it, a declared Content-Length over the limit is refused before
reading, and the migration guides for gunicorn and uvicorn call the change
out. Development mode uses 100 MB.

### Consequences

- Good: safe by default; upload routes state their limits explicitly.

- Bad: a fresh deployment of an application that accepts large uploads sees
  413 responses until the routes are configured.

## More information

Specification sections 4.6, 4.11 and 13.1.
