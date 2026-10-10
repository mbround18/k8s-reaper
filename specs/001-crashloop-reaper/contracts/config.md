# Contract: configuration

## ReaperConfig CRD

`reaperconfigs.reaper.bruno.fyi`, version `v1alpha1`, **cluster-scoped**, with
a status subresource. The reaper reads exactly one object, named by
`--config-name` (default `default`). Its absence means defaults (report mode).
Short name `rc`; printer columns `Mode`, `Valid`, `Candidates`, `Reaped`, `Age`.

```yaml
apiVersion: reaper.bruno.fyi/v1alpha1
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
status:
  observedGeneration: 3
  activeGeneration: 3          # generation in use (lags observedGeneration if invalid)
  lastCycleTime: "2026-10-10T16:00:00Z"
  counts: { observed: 2, candidate: 7, reaping: 0, reaped: 0, skipped: 1 }
  conditions:
    - type: Valid              # False with message listing field errors
      status: "True"
    - type: Ready              # caches synced and a cycle completed in the last 3 intervals
      status: "True"
```

Durations use the Go/Flux form (`90s`, `30m`, `1h30m`). Ranges and defaults are
in [data-model.md](../data-model.md#validation-rules-config). The YAML above is
printed by `k8s-reaper crd` (CRD) and is what the chart's `values.yaml`
renders.

## Process flags / environment

| Flag                    | Env                           | Default              | Purpose                                        |
| ----------------------- | ----------------------------- | -------------------- | ---------------------------------------------- |
| `--config-name`         | `REAPER_CONFIG_NAME`          | `default`            | Which ReaperConfig to read                     |
| `--namespace`           | `REAPER_NAMESPACE`            | (downward API, required) | Own namespace; always excluded            |
| `--http-addr`           | `REAPER_HTTP_ADDR`            | `0.0.0.0:8080`       | `/healthz`, `/readyz`, `/metrics`              |
| `--log-format`          | `REAPER_LOG_FORMAT`           | `json`               | `json` or `text`                               |
| —                       | `RUST_LOG`                    | `info`               | Log filter                                     |
| —                       | `OTEL_EXPORTER_OTLP_ENDPOINT` | unset (OTLP off)     | Standard OTel SDK env; also `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES` |

None of these change reaping behaviour (constitution V).

Subcommands: `k8s-reaper run` (default) and `k8s-reaper crd` (prints the CRD
YAML to stdout, for the chart).

## HTTP

| Path       | 200 when                                                            |
| ---------- | ------------------------------------------------------------------- |
| `/healthz` | the process is up and the cycle loop has not panicked               |
| `/readyz`  | caches synced, a valid config is active, last cycle < 3× interval   |
| `/metrics` | always (Prometheus text)                                            |

## RBAC required (ClusterRole)

| API group             | Resources                                 | Verbs                     |
| --------------------- | ----------------------------------------- | ------------------------- |
| `""`                  | pods, namespaces                          | get, list, watch          |
| `apps`                | replicasets                               | get, list, watch          |
| `apps`                | deployments, statefulsets, daemonsets     | get, list, watch, patch   |
| `apps`                | deployments/scale, statefulsets/scale     | get, patch, update        |
| `autoscaling`         | horizontalpodautoscalers                  | get, list, watch          |
| `keda.sh`             | scaledobjects                             | get, list, watch          |
| `helm.toolkit.fluxcd.io` | helmreleases                           | get, list, watch, patch   |
| `events.k8s.io`       | events                                    | create, patch             |
| `reaper.bruno.fyi`    | reaperconfigs                             | get, list, watch          |
| `reaper.bruno.fyi`    | reaperconfigs/status                      | get, patch, update        |

DaemonSets get `patch` only for the reaper's own annotations and labels. No
`delete`, no access to Secrets, and no `escalate`/`bind`/`impersonate`.
