# Appendix B. Admin API reference

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

gRPC service muru.admin.v1 on the admin socket, optionally over TCP with mTLS, plus a JSON gateway with the same operations. The worker protocol (muru.worker.v1) is internal and not part of this API.

**Conventions:** resource names such as apps/shop/releases/r42; resource_version for optimistic concurrency; a Watch for every List; idempotency keys, audit records and events for every mutation; gRPC status codes, and RFC 9457 problem+json on the gateway; roles read, deploy and admin; long-running work (release start, rollout, drain, backup) returns an operation that can be polled or watched.

**Resources** (Get, List, Watch, Create, Update and Delete unless noted)

| Resource | Gateway path | Notes |
| --- | --- | --- |
| Application | /v1/apps/{app} | defaults, reload, memory and timeout settings |
| Release | /v1/apps/{app}/releases/{release} | create starts it, immutable except scale and limits |
| WorkerGroup | /v1/apps/{app}/releases/{release}/groups/{group} | Get, List, Watch, Update of sizes and scaling |
| Worker | /v1/apps/{app}/releases/{release}/groups/{group}/workers/{worker} | read only |
| Schedule | /v1/apps/{app}/schedules/{schedule} | scheduled capacity |
| Rollout | /v1/apps/{app}/rollouts/{rollout} | create starts a rollout |
| Host | /v1/hosts/{host} | virtual hosts |
| Route | /v1/routes/{route} | matching, target, limits, error pages, uploads, redirects |
| TrafficPolicy | /v1/routes/{route}/traffic | Get and Update only |
| Upstream | /v1/upstreams/{upstream} | HTTP upstreams |
| EdgePool | /v1/edge/pools/{pool} | listeners and processes |
| Certificate | /v1/certificates/{cert} | stored certificates, ACME state later |
| EventSink | /v1/events/sinks/{sink} | webhook, OTLP, journald and file sinks |
| Overlay | /v1/overlays/{overlay} | with state: inactive, active, advisory or latched |
| ClientClass | /v1/client-classes/{class} | ordered rules |
| RateLimit | /v1/rate-limits/{limit} | buckets held in edge shared memory |

**Operations**

| RPC | Gateway | Purpose |
| --- | --- | --- |
| ApplyConfig | POST /v1/config:apply | apply files, with dry_run and prune |
| ExportConfig | GET /v1/config:export | export desired state |
| DiffConfig | POST /v1/config:diff | compare submitted configuration |
| GetConfigSchema | GET /v1/config:schema | JSON Schema |
| GetEffectiveConfig | GET /v1/apps/{app}/releases/{release}:effective | merged settings with sources |
| Deploy | POST /v1/apps/{app}:deploy | create a release and apply the default strategy |
| Rollback | POST /v1/apps/{app}:rollback | restore the previous traffic policy |
| ReloadApp | POST /v1/apps/{app}:reload | new release from the same source |
| CheckRelease | POST /v1/apps/{app}:check | validate in a throwaway host |
| PauseRollout, ResumeRollout, PromoteRollout, AbortRollout | POST /v1/apps/{app}/rollouts/{rollout}:pause, :resume, :promote, :abort | rollout control |
| DrainRelease | POST /v1/apps/{app}/releases/{release}:drain | drain with timeouts |
| SetScale | POST /v1/apps/{app}/releases/{release}:scale | sizes, bounds, temporary floors |
| GetScaleStatus | GET /v1/apps/{app}/releases/{release}/scale | signals, decisions, churn |
| SetMaintenance | POST /v1/apps/{app}:maintenance | maintenance mode |
| Exec | POST /v1/apps/{app}/releases/{release}:exec | one-off command, streamed output |
| TailLogs | GET /v1/apps/{app}/releases/{release}/logs | stream logs |
| ReopenLogs | POST /v1/logs:reopen | reopen log files, optionally in workers |
| SetLogLevel | POST /v1/apps/{app}/releases/{release}:loglevel | time-limited logger levels |
| DumpStacks | POST /v1/.../workers/{worker}:stacks | Python stacks |
| WatchEvents | GET /v1/events (Server-Sent Events) | event stream from a sequence number |
| GetStatus | GET /v1/status | daemon, edges, snapshot version |
| ReloadDaemonConfig | POST /v1/daemon:reload | re-read the daemon configuration |
| UpgradeDaemon | POST /v1/daemon:upgrade | re-exec and replace edges |
| Backup, Restore | POST /v1/daemon:backup, :restore | state store |
| ActivateOverlay, EndOverlay, PromoteOverlay | POST /v1/overlays/{overlay}:activate, :end, :promote | overlay control |
| PreviewOverlay | GET /v1/overlays/{overlay}:preview | effective changes if activated |
| TriggerOverlay | POST /v1/overlays/{overlay}:trigger (signed webhook) | external trigger from monitoring systems |
| ClassifyRequest | POST /v1/client-classes:test | classify a sample request |

Message shapes are defined in proto/muru/admin/v1 during phase 2, and an OpenAPI description of the JSON gateway is generated from them and published with the documentation. This appendix fixes the surface so that every capability in the spec is reachable.
