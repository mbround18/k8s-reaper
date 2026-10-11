# Contract delta: config, HTTP and status

Applies to [001 contracts/config.md](../../001-crashloop-reaper/contracts/config.md).
Implementation edits that file in place; this delta records what changes.

## HTTP (replaces the table)

| Path       | 200 when                                                                                         |
| ---------- | ------------------------------------------------------------------------------------------------ |
| `/healthz` | a cycle has started within `max(10 × cycleInterval, 10m)`. A loop that ends or panics exits the process |
| `/readyz`  | caches synced, **a valid config applied at least once since start** (built-in defaults count), and the last cycle finished within `3 × cycleInterval` |
| `/metrics` | always (Prometheus text)                                                                         |

Both probe endpoints return 503 with a one-line plain-text reason
(`no cycle started in 10m0s`, `no valid config applied yet`,
`last cycle finished 4m12s ago`) so `kubectl describe pod` shows why.

At defaults (`cycleInterval: 60s`, kubelet `periodSeconds: 10`,
`failureThreshold: 3`), a hung loop is unready after 3 min and restarted after
about 10.5 min (SC-001: ≤ 12 min).

## `status.conditions` (replaces the `Ready` line)

```yaml
    - type: Ready
      status: "True"           # False when not ready
      reason: CycleCompleted   # or Stalled | CachesNotSynced
```

| reason            | status  | When                                                         |
| ----------------- | ------- | ------------------------------------------------------------ |
| `CachesNotSynced` | `False` | from start until the first cycle finishes                    |
| `Stalled`         | `False` | the last cycle finished more than `3 × cycleInterval` ago     |
| `CycleCompleted`  | `True`  | otherwise                                                    |

`Ready` follows the readiness probe except for config: an invalid config shows
on `Valid`, not on `Ready`. `NotReady` is no longer written.

## OpenTelemetry (new section; FR-014)

| Variable                              | Effect                                              |
| ------------------------------------- | --------------------------------------------------- |
| `OTEL_EXPORTER_OTLP_ENDPOINT`         | enables both traces and metrics export              |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`  | enables traces only (overrides the general one for traces) |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | enables metrics only (overrides the general one for metrics) |
| `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES` | resource; `service.name` defaults to `k8s-reaper` when neither sets it |
| other `OTEL_*` the SDK reads (headers, timeout) | passed through to the SDK                |

- Transport: OTLP over HTTP (protobuf). gRPC is not built in.
- Signals: metrics (the same instruments as `/metrics`, with `.` in place of
  `_`) and traces.
- The reaper creates **no spans of its own**. Traces carry only what the
  `tracing` subscriber bridges (log spans from libraries). An empty or blank
  endpoint counts as unset.

The implementing task checks this table against `reaper-observability`
`tracing.rs` before it lands.
