---
status: accepted
date: 2026-10-31
---

# ADR 0035: muructl is a container's PID 1, with a built-in init

## Context and problem statement

ADR 0010 and specification section 6.10 made the host PID 1 in a
container release, reaping orphans itself so that no tini was needed.
Running Muru as a container's main process has since taken another
shape: `muructl daemon run` or `muructl run` is the command, the host a
child of it. As PID 1 the daemon reaps only the processes it started, by
pid, so every orphan reparented to it stays a zombie, and SIGTERM makes
it detach, which as PID 1 means the kernel kills the edge and every
worker mid-request when it exits. Images needed tini or `docker run
--init`, and `docker stop` drained nothing.

## Decision drivers

- A container's main process must reap orphans and must treat SIGTERM
  as shutdown, or the image is incorrect as shipped.

- The daemon must keep reaping by pid: a blanket `waitpid(-1)` would
  collect exit statuses tokio waits for on its own child handles.

- Under systemd SIGTERM must go on detaching, so a daemon restart never
  drops traffic; the two cases must not be told apart by a heuristic
  about the parent process.

- The host still has to clean up a dead worker's descendants, in or out
  of a container.

## Considered options

- Reap inside the daemon, collecting only pids it does not know with
  `waitid(P_ALL, WEXITED | WNOHANG | WNOWAIT)` before tokio sees them.

- A built-in init: when muructl finds itself PID 1 it forks before tokio
  exists, the child runs the command and the parent forwards signals,
  reaps everything and exits with the child's status, as tini does.

- Leave it to the image: document tini or `docker run --init`.

## Decision outcome

Chosen option: the built-in init, with `MURU_NO_INIT=1` to opt out for
images that bring their own, and the host a subreaper under it. What
SIGTERM means is a setting, `on_sigterm` in the daemon file's
`[shutdown]` table, whose default is shutdown when the daemon is PID 1 or
the init's child (which `MURU_UNDER_INIT` in its environment says) and
detach otherwise. The shutdown is one sequence inside a budget, the same
one `muructl daemon stop --shutdown` runs, so it is testable outside a
container. Reaping inside the daemon was rejected as fragile beside
tokio's SIGCHLD handling; leaving it to the image was rejected because an
image that is wrong without an extra flag is wrong.

### Consequences

- Good: `docker stop` drains and the container exits 0 inside Docker's
  grace with no flag or extra binary; PID 1 is trivially correct and the
  daemon is unchanged; an upgrade re-executes the child, so PID 1 never
  changes.

- Good: the host collects and stops what its workers leave behind
  wherever it runs, which was specified and not built.

- Bad: a second process in the container, and a fork before the
  command's own start, however small; a daemon that is PID 1 without the
  init has to be told so through the table or the environment.

## More information

Specification sections 6.10 and 13.3, appendix C and appendix D.
Supersedes the PID 1 part of [ADR 0010](0010-container-release-layout.md);
the layout of a container release, with the daemon and the edge outside,
stands. Issues #99, #100 and #101.
