---
status: accepted
date: 2026-11-02
---

# ADR 0044: muructl serve runs one application in one process, configured in layers, with probes on an admin port

## Context and problem statement

`muructl run` is development mode and `muructl daemon run` is a node
daemon with a state directory, an admin socket and many applications.
Neither is what a container wants: one application, configured by the
image and the environment, nothing persistent, and the probes and
metrics a container platform expects (issues #102, #103, #106 and #110,
tracked by #245). Running the daemon as a container's main process works
as a stop gap but carries state, files and administration a container
has no use for. Section 13.9 of the specification describes the command
this record settles.

## Decision drivers

- Switching an existing Dockerfile to Muru is a one-line change, and the
  same image works under Docker, Compose, Podman and Kubernetes.

- A deployment overrides one setting without rebuilding the image or
  copying a whole configuration file, as Kubernetes users expect.

- The container runs under an arbitrary UID with a read-only root, as
  OpenShift and the restricted Pod Security profile require.

- Start-up never depends on the network, and two containers from one
  image never differ.

- Probes and metrics are reachable by the platform without being
  reachable through the application's Ingress, and without anything that
  changes the process being exposed on the network.

## Considered options

- The daemon as the container's main process, as today.

- A separate edge process beside the controller, as the daemon runs it.

- One process, as `muructl run` has, with production defaults and
  container behaviour.

For the admin port: closed by default and opened per deployment; open by
default with everything on it; or open by default with only the probes
and metrics.

## Decision outcome

Chosen option: one process with production defaults, configured in
layers, with the probes and metrics on an admin port open by default.

- **One process.** The controller and edge are threads of one muructl
  process and the host and workers its children, as in `muructl run`.
  Inside one container a separate edge process brings no isolation and
  no independent upgrade. The container is the release: no traffic
  management inside it, since the orchestrator replaces containers.

- **Configuration layers**, lowest first: defaults, `[tool.muru]` with a
  serve table of its own, the file named by `MURU_SERVE_CONFIG`, one
  `MURU_SERVE_*` variable per setting, flags. Flags over the environment
  over files is the order servers and configuration libraries commonly
  use (gunicorn, Click, Viper, Spring Boot). The prefix keeps them apart
  from the read-only MURU_* runtime context. A list in a higher layer
  replaces the lower one.

- **The entrypoint** comes from the argument or the configuration
  layers, the interpreter from the image contract's order without its
  label, since a process cannot read its own image's labels.

- **One writable run directory**, `/run/muru` by default, holding the
  admin socket, the release's sockets and the upload spool. If it cannot
  be written, the start fails naming the volume it needs. No passwd entry
  assumed and files created group-writable.

- **No installs at run time**, always under `serve`, with no container
  detection: nothing is built, installed or fetched, and a missing piece
  fails the start naming the build step that should have provided it.

- **The admin port** is open by default on 0.0.0.0:9090 with `/livez`,
  `/readyz`, `/startupz` and `/metrics`. `/status` needs
  `--admin-status`, since it describes the configuration. The dashboard is
  not on it. Operations that change the process are only on the admin
  socket, reached by `docker exec` or `kubectl exec`, so the platform's
  exec permission is their access control. `muructl probe` asks the
  probes' questions over a socket in the run directory for a Docker
  `HEALTHCHECK`.

- **Logs** go to standard output through one relay, as ADR 0043 sets
  out, in text by default or JSON with `--log-format json`.

### Consequences

- Good: a container runs with no volume but its run directory, and the
  probes and metrics a platform expects work with no configuration.

- Good: one setting is overridden from a Deployment's `env` without
  touching the image.

- Good: an image built to the image contract serves both shapes, alone
  under `serve` and as a daemon's container release.

- Bad: anyone on the cluster network can read the probes and metrics
  unless a NetworkPolicy limits it; the metrics carry mount, group and
  release names and traffic volumes.

- Bad: settings now come from five layers, so `--print-config` names the
  layer of each value to make the result explainable.

- Bad: the dashboard is not available in a container until it gets a
  sign-in path of its own there.

## More information

- Specification sections 13.9, 8.5, 8.6, 11.8 and 13.8, and appendix D.

- Issues #245, #102, #103, #106 and #110; ADR 0035 (muructl as PID 1),
  ADR 0030 (the dashboard's session), ADR 0043 (times on log lines).
