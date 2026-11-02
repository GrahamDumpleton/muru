# A Kubernetes Deployment

```{admonition} Status
:class: note

A Deployment of `muructl serve`: probes, a rolling update that loses no
requests, a restricted security context, sizing from resource limits,
logs for a collector, autoscaling and migrations, and `muructl`
commands through `kubectl exec`. Reloading mounted configuration and
certificates is still to come, as is a check that the debugging commands
that attach to a worker work in a pod with every capability dropped.
```

An image built as [in a container](containers.md#an-image) runs in a pod
as it runs under Docker. Its settings come from the environment, so the
whole configuration is in the Deployment.

## The Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: shop
spec:
  replicas: 2
  selector:
    matchLabels: {app: shop}
  template:
    metadata:
      labels: {app: shop}
    spec:
      terminationGracePeriodSeconds: 30
      securityContext:
        runAsNonRoot: true
        seccompProfile: {type: RuntimeDefault}
      containers:
        - name: shop
          image: registry.example.com/shop:1.4.2
          env:
            - {name: MURU_SERVE_SHUTDOWN_DELAY, value: "5s"}
            - {name: MURU_SERVE_SHUTDOWN_BUDGET, value: "25s"}
            - {name: MURU_SERVE_TRUSTED_PROXY, value: "private"}
            - {name: MURU_SERVE_LOG_FORMAT, value: "json"}
            - name: POD_NAME
              valueFrom: {fieldRef: {fieldPath: metadata.name}}
            - name: POD_NAMESPACE
              valueFrom: {fieldRef: {fieldPath: metadata.namespace}}
            - name: NODE_NAME
              valueFrom: {fieldRef: {fieldPath: spec.nodeName}}
          ports:
            - {name: http, containerPort: 8080}
            - {name: admin, containerPort: 9090}
          startupProbe:
            httpGet: {path: /startupz, port: admin}
            periodSeconds: 2
            failureThreshold: 60
          livenessProbe:
            httpGet: {path: /livez, port: admin}
          readinessProbe:
            httpGet: {path: /readyz, port: admin}
            periodSeconds: 2
          resources:
            requests: {cpu: "1", memory: 512Mi}
            limits: {cpu: "2", memory: 1Gi}
          resizePolicy:
            - {resourceName: cpu, restartPolicy: RestartContainer}
            - {resourceName: memory, restartPolicy: RestartContainer}
          securityContext:
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
            capabilities: {drop: ["ALL"]}
          volumeMounts:
            - {name: run, mountPath: /run/muru}
            - {name: tmp, mountPath: /tmp}
      volumes:
        - name: run
          emptyDir: {sizeLimit: 2Gi}
        - name: tmp
          emptyDir: {}
---
apiVersion: v1
kind: Service
metadata:
  name: shop
spec:
  selector: {app: shop}
  ports:
    - {name: http, port: 80, targetPort: http}
```

The pieces, in turn:

- **Two ports.** The application is on `http` and the probes and metrics
  on `admin`. The Service exposes only `http`, so an Ingress in front of
  it never reaches the admin port. A NetworkPolicy can limit the admin
  port to the kubelet and Prometheus.
- **Three probes.** `/startupz` holds the other two off until the
  application has loaded, however long its import takes. `/livez`
  restarts a container whose edge or host has stopped answering, never
  one whose workers are only busy. `/readyz` takes a pod out of the
  Service without restarting it, while no workers are ready or once it is
  stopping. [Probes and metrics](containers.md#probes-and-metrics)
  describes each.
- **Stopping.** SIGTERM fails `/readyz` at once. For
  `MURU_SERVE_SHUTDOWN_DELAY` the pod keeps serving while the Service's
  endpoints and the ingress stop sending to it, closing each connection
  after its response. Then the requests in flight finish and the
  container exits, all within `MURU_SERVE_SHUTDOWN_BUDGET`, which is set
  under `terminationGracePeriodSeconds`. A rolling update loses no
  requests and needs no `preStop` hook; [stopping](containers.md#stopping)
  covers the sequence.
- **The proxy in front.** The ingress controller's connections come from
  pod addresses, so `MURU_SERVE_TRUSTED_PROXY=private` applies its
  `Forwarded` and `X-Forwarded-*` headers. Name its range instead to
  trust nothing else; [behind a proxy](behind-a-proxy.md) has the
  details.
- **The security context.** The pod passes the `restricted` Pod Security
  Standard: a user ID other than root, any ID the platform assigns, no
  privilege escalation, no capabilities, and a read-only root filesystem.
  Only `/run/muru` and `/tmp` are written, so they are the only volumes.
  Request bodies too large to hold in memory are spooled to `/run/muru`,
  up to `--spool-quota` (1 GB by default), so its `sizeLimit` is above
  that. A volume with `medium: Memory` would count those bodies against
  the container's memory limit. `runAsNonRoot` needs the image's `USER`
  to be a number, as in the [image](containers.md#an-image).
- **Resources.** The worker count is the CPU limit rounded up, 2 here,
  and each worker's hard memory limit is worked out from the memory
  limit, so a worker that grows is replaced before the kernel kills one.
  [Sizing from the container's
  limits](containers.md#sizing-from-the-containers-limits) shows the
  arithmetic and the lines that report it. The `resizePolicy` restarts
  the container when its limits change; [resizing](#resizing) says why.
- **Logs.** With `MURU_SERVE_LOG_FORMAT=json` every line on standard
  output is one JSON record, a traceback included, carrying the pod, its
  namespace and its node from the Downward API variables, which is what a
  collector such as Fluent Bit or Vector reads from the node.
  [Logs](containers.md#logs) shows the records.

## Resizing

`muructl serve` reads its limits when it starts. Kubernetes can change a
running container's CPU and memory in place, through `kubectl patch
--subresource resize` or a VerticalPodAutoscaler in its
`InPlaceOrRecreate` mode. A container that keeps running after such a
change goes on with the worker count and the hard limits of its old
limits:

- with more CPU, the extra goes mostly unused;
- with more memory, workers are still replaced at the old share, so
  their use never shows the autoscaler that more would be taken;
- with less memory, the hard limits are above what the container now
  has, and the kernel may kill a worker before Muru replaces it.

`restartPolicy: RestartContainer` in the `resizePolicy` has the kubelet
restart the container when the limit changes. The restart is an ordinary
stop: readiness fails, the requests in flight finish, and the new
container sizes itself from the new limits. The pod stays where it is,
on the same node with the same address.

A VerticalPodAutoscaler in its `Recreate` mode evicts the pod and
creates another with the new resources, which sizes itself as any new
pod does. A HorizontalPodAutoscaler on `muru_capacity_utilisation`, below,
and a VerticalPodAutoscaler on CPU and memory can run together, since
they act on different figures.

## Autoscaling

A HorizontalPodAutoscaler scales on `muru_capacity_utilisation`, the
share of the worker slots in use averaged over 30 seconds, rather than
on CPU, which stays low while an application waits on its database with
every slot busy. Prometheus scrapes the admin port:

```yaml
template:
  metadata:
    annotations:
      prometheus.io/scrape: "true"
      prometheus.io/port: "9090"
```

The Prometheus adapter makes the metric a pods metric:

```yaml
rules:
  - seriesQuery: 'muru_capacity_utilisation{namespace!="",pod!=""}'
    resources:
      overrides:
        namespace: {resource: namespace}
        pod: {resource: pod}
    metricsQuery: 'avg(<<.Series>>{<<.LabelMatchers>>}) by (<<.GroupBy>>)'
```

and the autoscaler keeps it near 70 percent:

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: shop
spec:
  scaleTargetRef: {apiVersion: apps/v1, kind: Deployment, name: shop}
  minReplicas: 2
  maxReplicas: 10
  metrics:
    - type: Pods
      pods:
        metric: {name: muru_capacity_utilisation}
        target: {type: AverageValue, averageValue: 700m}
```

KEDA can scale on the same figure, or on how long requests wait for a
slot, with a Prometheus trigger:

```yaml
apiVersion: keda.sh/v1alpha1
kind: ScaledObject
metadata:
  name: shop
spec:
  scaleTargetRef: {name: shop}
  minReplicaCount: 2
  maxReplicaCount: 10
  triggers:
    - type: prometheus
      metadata:
        serverAddress: http://prometheus.monitoring:9090
        query: >-
          histogram_quantile(0.95, sum by (le)
          (rate(muru_request_duration_seconds_bucket{app="shop",phase="queue"}[2m])))
        threshold: "0.1"
```

[Saturation for an autoscaler](metrics.md#saturation-for-an-autoscaler)
describes both figures.

## Migrations

A Job runs the application's own migration command from the same image,
before the Deployment is updated to it:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: shop-migrate-1-4-2
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      securityContext:
        runAsNonRoot: true
        seccompProfile: {type: RuntimeDefault}
      containers:
        - name: migrate
          image: registry.example.com/shop:1.4.2
          command: ["/app/.venv/bin/python", "manage.py", "migrate", "--noinput"]
          securityContext:
            allowPrivilegeEscalation: false
            capabilities: {drop: ["ALL"]}
```

The command runs as any other process in the image does; `muructl` takes
no part in it.

## Looking inside a pod

`muructl` in the container reaches `muructl serve` through its run
directory, so `kubectl exec` runs the commands that read or adjust a
running application:

```
kubectl exec deploy/shop -- muructl status
kubectl exec deploy/shop -- muructl logs --access --follow
kubectl exec deploy/shop -- muructl debug stacks
kubectl exec deploy/shop -- muructl restart --all
```

Whoever may exec into the pod may use them, which RBAC decides.
