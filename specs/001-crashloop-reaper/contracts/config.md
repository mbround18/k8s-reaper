# Contract: configuration

## ReaperConfig CRD

`reaperconfigs.reaper.boop.ninja`, version `v1alpha1`, **cluster-scoped**, with
a status subresource. The reaper reads exactly one object, named by
`--config-name` (default `default`). Its absence means defaults (report mode).
Short name `rc`; printer columns `Mode`, `Valid`, `Candidates`, `Reaped`, `Age`.

```yaml
apiVersion: reaper.boop.ninja/v1alpha1
kind: ReaperConfig
metadata:
  name: default
spec:
  mode: report                 # report | full
  cycleInterval: 60s
  detection:
    crashLoopDuration: 30m     # continuous crash-looping before a workload is a candidate
    minRestarts: 10            # restart count of the crash-looping container
    recoveryWindow: 10m        # continuous health before state is cleared
  scope:
    excludeNamespaces: [kube-system, kube-public, kube-node-lease, flux-system]
    includeNamespaces: []      # globs; empty = all (own namespace is always excluded)
  limits:
    maxReapsPerCycle: 1
    maxReaped: 5
  flux:
    kustomize: true            # disable kustomize reconcile on reaped objects
    helmRelease: true          # suspend owning HelmReleases
  cronJobs:
    enabled: true              # observe CronJobs and pause ones that keep failing
    failingDuration: 6h        # every run failed for this long (10m–168h)
    minFailedRuns: 3           # and at least this many failed runs (1–1000)
  notifications:               # US6; channel URLs come from a mounted Secret, never from here
    enabled: true
    reasons: [Reaped, ReapFailed, Restored, RestoreFailed, HelmReleaseHeld, ConfigInvalid]
    minInterval: 1h
status:
  observedGeneration: 3
  activeGeneration: 3          # generation in use (lags observedGeneration if invalid)
  lastCycleTime: "2026-10-10T16:00:00Z"
  counts: { observed: 2, candidate: 7, reaping: 0, reaped: 0, skipped: 1 }
  conditions:
    - type: Valid              # False with message listing field errors
      status: "True"
    - type: Ready
      status: "True"           # False when not ready
      reason: CycleCompleted   # or Stalled | CachesNotSynced
```

| `Ready` reason    | status  | When                                                     |
| ----------------- | ------- | -------------------------------------------------------- |
| `CachesNotSynced` | `False` | from start until the first cycle finishes                |
| `Stalled`         | `False` | the last cycle finished more than `3 × cycleInterval` ago |
| `CycleCompleted`  | `True`  | otherwise                                                |

`Ready` follows the readiness probe except for config: an invalid config shows
on `Valid`, not on `Ready`. `NotReady` is not written. A status task re-writes
only the conditions every `cycleInterval`, so `Stalled` appears while the loop
is stuck.

Durations use the Go/Flux form (`90s`, `30m`, `1h30m`). Ranges and defaults are
in [data-model.md](../data-model.md#validation-rules-config). The YAML above is
printed by `k8s-reaper crd` (CRD) and is what the chart's `values.yaml`
renders.

The reaper reads the object (`metadata.name` from `--config-name`) once at the
start of each cycle, so an edit takes effect on the next cycle without a
restart. A new valid generation emits `ConfigApplied`; an invalid one emits
`ConfigInvalid` once per generation, sets `Valid=False` and keeps the last valid
config. When the object is deleted the built-in defaults apply. Status is
merge-patched on `reaperconfigs/status` after every cycle.

## Process flags / environment

| Flag                    | Env                           | Default              | Purpose                                        |
| ----------------------- | ----------------------------- | -------------------- | ---------------------------------------------- |
| `--config-name`         | `REAPER_CONFIG_NAME`          | `default`            | Which ReaperConfig to read                     |
| `--namespace`           | `REAPER_NAMESPACE`            | (downward API, required) | Own namespace; always excluded            |
| `--http-addr`           | `REAPER_HTTP_ADDR`            | `0.0.0.0:8080`       | `/healthz`, `/readyz`, `/metrics`              |
| `--log-format`          | `REAPER_LOG_FORMAT`           | `json`               | `json` or `text`                               |
| `--notifications-dir`   | `REAPER_NOTIFICATIONS_DIR`    | `/etc/k8s-reaper/notifications` | Mounted Secret of channel URLs (US6, [notifications.md](./notifications.md)) |
| —                       | `RUST_LOG`                    | `info`               | Log filter                                     |
| —                       | `OTEL_EXPORTER_OTLP_ENDPOINT` | unset (OTLP off)     | Standard OTel SDK env; see [OpenTelemetry](#opentelemetry) |

None of these change reaping behaviour (constitution V).

Subcommands: `k8s-reaper run` (default) and `k8s-reaper crd` (prints the CRD
YAML to stdout, for the chart).

## OpenTelemetry

OTLP export is off unless an endpoint is set. Configuration is the standard
OTel SDK environment; the reaper adds no flags of its own.

| Variable                              | Effect                                              |
| ------------------------------------- | --------------------------------------------------- |
| `OTEL_EXPORTER_OTLP_ENDPOINT`         | enables both traces and metrics export              |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`  | enables traces only (overrides the general one for traces) |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | enables metrics only (overrides the general one for metrics) |
| `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES` | resource; `service.name` defaults to `k8s-reaper` when neither sets it |
| other `OTEL_*` the SDK reads (headers, timeout, batch and export intervals) | passed through to the SDK |

- An empty or blank endpoint counts as unset.
- Transport: OTLP over HTTP (protobuf). gRPC is not built in.
- Signals: metrics and traces. The metrics are the same instruments as
  `/metrics`, named with `.` in place of `_` and without the `_total`
  suffix on counters (`reaper_actions_total` is `reaper.actions`).
- The reaper creates **no spans of its own** (no `#[instrument]` or `span!`
  in its code). Traces carry only the spans that libraries open through
  `tracing`, bridged by the subscriber. `RUST_LOG` filters them as it
  filters logs.

## HTTP

| Path       | 200 when                                                                                         |
| ---------- | ------------------------------------------------------------------------------------------------ |
| `/healthz` | a cycle has started within `max(10 × cycleInterval, 10m)`. A loop that ends or panics exits the process |
| `/readyz`  | caches synced, **a valid config applied at least once since start** (built-in defaults count), and the last cycle finished within `3 × cycleInterval` |
| `/metrics` | always (Prometheus text)                                                                         |

Both probe endpoints return 503 with a one-line plain-text reason
(`no cycle started in 10m0s`, `caches not synced yet`,
`no valid config applied yet`, `no cycle finished yet`,
`last cycle finished 4m12s ago`) so `kubectl describe pod` shows why.

At defaults (`cycleInterval: 60s`, kubelet `periodSeconds: 10`,
`failureThreshold: 3`), a hung loop is unready after 3 min and restarted after
about 10.5 min (SC-001: ≤ 12 min).

## RBAC required (ClusterRole)

| API group             | Resources                                 | Verbs                     |
| --------------------- | ----------------------------------------- | ------------------------- |
| `""`                  | pods, namespaces                          | get, list, watch          |
| `apps`                | replicasets                               | get, list, watch          |
| `apps`                | deployments, statefulsets, daemonsets     | get, list, watch, patch   |
| `apps`                | deployments/scale, statefulsets/scale     | get, patch, update        |
| `batch`               | cronjobs                                  | get, list, watch, patch   |
| `batch`               | jobs                                      | get, list, watch          |
| `autoscaling`         | horizontalpodautoscalers                  | get, list, watch          |
| `keda.sh`             | scaledobjects                             | get, list, watch          |
| `helm.toolkit.fluxcd.io` | helmreleases                           | get, list, watch, patch   |
| `events.k8s.io`       | events                                    | create, patch             |
| `reaper.boop.ninja`    | reaperconfigs                             | get, list, watch          |
| `reaper.boop.ninja`    | reaperconfigs/status                      | get, patch, update        |

DaemonSets get `patch` only for the reaper's own annotations and labels.
CronJobs get `patch` for those and for `spec.suspend`. Jobs are never patched or
deleted. No
`delete`, no access to Secrets, and no `escalate`/`bind`/`impersonate`.
Notification channel URLs reach the pod as a mounted Secret volume
([notifications.md](./notifications.md)), which needs no RBAC.
