# Monitoring

k8s-reaper reports what it sees and does in five places: Prometheus metrics
and health endpoints on port `8080`, alert rules the Helm chart can install,
Kubernetes Events on the workloads it tracks, the status of the `ReaperConfig`,
and its own logs. Metrics can also be pushed over OpenTelemetry. This guide is
the reference for all of them, with a runbook for every alert the chart ships.

## 1. At a glance

| Where                        | What you get                                                       | Section |
| ---------------------------- | ------------------------------------------------------------------ | ------- |
| `GET :8080/metrics`          | Per-workload state, skips, actions, cycles, config and build info  | [3](#3-metrics) |
| `GET :8080/healthz`, `/readyz` | Liveness and readiness for the pod probes                        | [2](#2-health-endpoints) |
| PrometheusRule (chart)       | Seven alerts on the metrics above                                  | [4](#4-scraping-and-alerts-with-the-helm-chart), [5](#5-alert-runbooks) |
| `kubectl get reaperconfig`   | Mode, validity, candidate and reaped counts for the whole cluster  | [7](#7-the-reaperconfig-status) |
| Kubernetes Events            | One Event per state change, on the workload itself                 | [8](#8-kubernetes-events) |
| Logs                         | JSON on stdout                                                     | [9](#9-logs) |
| OTLP                         | The same metrics, pushed to a collector                            | [10](#10-opentelemetry) |

## 2. Health endpoints

The reaper serves three paths on `0.0.0.0:8080` (`--http-addr` /
`REAPER_HTTP_ADDR`). Any other path returns `404`.

| Path       | `200` when | `503` when |
| ---------- | ---------- | ---------- |
| `/healthz` | A cycle has started within `max(10 × cycleInterval, 10m)` (10 minutes with the default `60s`). | No cycle has started for that long: the loop is stuck. |
| `/readyz`  | The watch caches have synced, a valid config has been applied at least once since the pod started (the built-in defaults count), and the last cycle finished less than 3 × `cycleInterval` ago (3 minutes with the default `60s`). | Before the first cycle, while the config has never been valid, or when cycles have stopped finishing. |
| `/metrics` | Always. Prometheus text format (OpenMetrics). | |

A `503` comes with a one-line reason, which `kubectl describe pod` shows in
the probe's failure event:

| Reason | Probe | Meaning |
| ------ | ----- | ------- |
| `no cycle started in 10m0s` | `/healthz` | The cycle loop is stuck. The kubelet restarts the container. |
| `caches not synced yet` | `/readyz` | Still starting: the watch caches have not finished their first list. |
| `no valid config applied yet` | `/readyz` | The `ReaperConfig` has been invalid since the pod started. |
| `no cycle finished yet` | `/readyz` | The first cycle is still running. |
| `last cycle finished 4m12s ago` | `/readyz` | Cycles have stopped finishing. |

The chart wires `/healthz` to the liveness probe and `/readyz` to the
readiness probe. With the defaults (`cycleInterval: 60s`, probe period `10s`,
3 failures), a hung loop is unready after 3 minutes and restarted after about
10.5 minutes. A loop that ends or panics exits the process, so it restarts at
once.

An invalid edit to a `ReaperConfig` that was valid before does not make
`/readyz` fail: the reaper keeps running on the last valid config, and the
`Valid` condition says what is wrong.

The same answer is on the `ReaperConfig` as its `Ready` condition:

| `Ready` reason | Status | When |
| -------------- | ------ | ---- |
| `CachesNotSynced` | `False` | From start until the first cycle finishes. |
| `Stalled` | `False` | The last cycle finished more than 3 × `cycleInterval` ago. |
| `CycleCompleted` | `True` | Otherwise. |

`Ready` is about the cycle loop only; config problems show on `Valid`. It is
updated every `cycleInterval` even while the loop is stuck, so `Stalled` shows
up without a restart:

```sh
kubectl get reaperconfig default \
  -o jsonpath='{range .status.conditions[*]}{.type}={.status} {.reason}{"\n"}{end}'
```

If `/readyz` stays at `503`, see
[troubleshooting.md](troubleshooting.md#readyz-returns-503).

## 3. Metrics

All metric names start with `reaper_`. Names and label sets are stable.

To look by hand (the Deployment is named after the Helm release, here
`k8s-reaper`):

```sh
kubectl -n k8s-reaper port-forward deploy/k8s-reaper 8080:8080
curl -s localhost:8080/metrics | grep '^reaper_'
```

### 3.1 Reference

| Metric | Type | Labels | Meaning |
| ------ | ---- | ------ | ------- |
| `reaper_workload_state` | gauge | `namespace`, `kind`, `name`, `state` | `1` for each tracked workload, with its current state. |
| `reaper_workload_skipped` | gauge | `namespace`, `kind`, `name`, `reason` | `1` while a candidate is skipped, with the skip reason. |
| `reaper_workload_crashloop_seconds` | gauge | `namespace`, `kind`, `name` | Seconds since the crash loop started (`crashloop-since`), or since a CronJob started failing (`failing-since`). |
| `reaper_actions_total` | counter | `action`, `result`, `mode` | Reap and restore steps, by outcome. |
| `reaper_cycles_total` | counter | `result` | Cycles, `ok` or `error`. |
| `reaper_cycle_duration_seconds` | histogram | none | Time to decide and act in one cycle. 12 buckets from 0.05 s, doubling up to 102.4 s. |
| `reaper_last_cycle_timestamp_seconds` | gauge | none | Unix time the last cycle finished. |
| `reaper_config_info` | gauge | `mode`, `generation` | `1` for the config in use: its mode and `ReaperConfig` generation (`0` for built-in defaults). |
| `reaper_config_valid` | gauge | none | `1` if the latest `ReaperConfig` generation is valid, `0` if not. |
| `reaper_notifications_total` | counter | `channel`, `provider`, `result` | Notifications by channel. See [notifications.md](notifications.md#4-check-that-it-works). |
| `reaper_notification_channels` | gauge | `channel`, `provider`, `valid` | `1` per channel file read in the last cycle. |
| `reaper_unowned_crashlooping_pods` | gauge | `namespace` | Crash-looping pods with no supported owner (bare pods, or owners such as an Argo Rollout). These are never tracked and get no Event. |
| `reaper_build_info` | gauge | `version`, `git_sha` | Always `1`. |

### 3.2 Label values

| Label | Values |
| ----- | ------ |
| `kind` | `Deployment`, `StatefulSet`, `DaemonSet`, `CronJob` |
| `state` | `observed`, `candidate`, `reaping`, `reaped` |
| `reason` | `ReportOnlyScope`, `CannotScaleToZero`, `ManagedByController`, `ManagedByAutoscaler`, `PartiallyAvailable`, `RateLimited`, `ReapCeilingReached`. What each means and what to do: [troubleshooting.md](troubleshooting.md#a-candidate-is-never-reaped). |
| `action` | `reap`, `restore` (one per whole reap or restore), and the steps `scale`, `kustomize_disable`, `kustomize_restore`, `helmrelease_hold`, `helmrelease_release`, `cronjob_suspend`, `cronjob_resume` |
| `result` (actions) | `success`, `error`, or `planned` (report mode: a reap that full mode would have run) |
| `mode` | `report` or `full`, the global mode at the time |
| `result` (notifications) | `sent`, `failed`, `dropped`, `filtered` |

### 3.3 How the series behave

- **Per-workload series exist only while the workload has a state.** When a
  workload recovers or is restored, its `reaper_workload_state`,
  `reaper_workload_skipped` and `reaper_workload_crashloop_seconds` series
  disappear. A state change replaces the series: there is never a `0` for
  the old state.
- **A skipped candidate is still a candidate.** It has both
  `reaper_workload_state{state="candidate"}` and `reaper_workload_skipped`.
- **In full mode, a reap goes from `candidate` to `reaped` within one
  cycle**, so a workload that is reaped normally is never seen as `candidate`
  on `/metrics`. A candidate that stays a candidate in full mode is being
  skipped.
- **`planned` is counted once per workload**, when it first becomes a
  candidate in report mode, not every cycle.
- **A failed reap counts twice:** once for the step that failed (for example
  `action="scale",result="error"`) and once for the reap
  (`action="reap",result="error"`).
- **`reaper_cycles_total{result="error"}` is wider than action errors.** A
  cycle is also an error when the reaper could not read the `ReaperConfig`, or
  could not write its own label or annotations on a workload. Those failures
  are logged but not counted in `reaper_actions_total`.
- **Counters start at zero when the pod restarts.** Use `increase()` or
  `rate()`, never the raw value.
- **Before the first cycle**, `reaper_config_valid` and
  `reaper_last_cycle_timestamp_seconds` read `0`.

Cardinality is bounded by the number of workloads in a state, plus one series
per namespace with unowned crash-looping pods.

## 4. Scraping and alerts with the Helm chart

The chart can install a `ServiceMonitor` and a `PrometheusRule`. Both need the
Prometheus Operator CRDs (kube-prometheus-stack installs them), and both are
**off by default**.

```yaml
serviceMonitor:
  enabled: true
  interval: 30s
  scrapeTimeout: ""
  honorLabels: true
  labels:
    release: kube-prometheus-stack
prometheusRule:
  enabled: true
  labels:
    release: kube-prometheus-stack
  runbookUrl: https://github.com/mbround18/k8s-reaper/blob/main/docs/guides/monitoring.md#5-alert-runbooks
```

- **`labels` must match your Prometheus.** The Prometheus Operator only picks
  up a ServiceMonitor that matches the Prometheus resource's
  `serviceMonitorSelector`, and a PrometheusRule that matches its
  `ruleSelector`. kube-prometheus-stack selects on `release: <its release
  name>` by default. Check yours:

  ```sh
  kubectl get prometheus -A -o jsonpath='{range .items[*]}{.metadata.name}: {.spec.serviceMonitorSelector} {.spec.ruleSelector}{"\n"}{end}'
  ```

  The namespace selectors (`serviceMonitorNamespaceSelector`,
  `ruleNamespaceSelector`) must also include the reaper's namespace.

- **Keep `honorLabels: true`.** The reaper's own `namespace` label names the
  workload's namespace. With `honorLabels: false`, Prometheus renames it to
  `exported_namespace` and puts the reaper's namespace in `namespace`, so
  every query and alert label in this guide changes.

- **Enable the ServiceMonitor whenever you enable the PrometheusRule.**
  `ReaperDown` looks for `up{job="<release fullname>"}`, the job name the
  ServiceMonitor produces. Without the ServiceMonitor, or with your own scrape
  config under a different job name, `ReaperDown` fires permanently.

- **`runbookUrl`** is copied into the `runbook_url` annotation of every alert.
  It defaults to [restoring-a-workload.md](restoring-a-workload.md). Point it
  at section 5 of this page, as above, to land on the runbooks.

The chart has no switch for single alerts. To quiet one, use an Alertmanager
silence or route.

The ServiceMonitor scrapes the `http` port (`8080`) of the chart's Service,
in the release namespace only.

## 5. Alert runbooks

The chart's rules, from `templates/prometheusrule.yaml`. Every alert carries
the labels of its expression (`namespace`, `kind`, `name` for the per-workload
alerts) plus `severity`.

| Alert | Severity | For |
| ----- | -------- | --- |
| [`ReaperCrashLoopPlanned`](#reapercrashloopplanned) | warning | 5m |
| [`ReaperWorkloadReaped`](#reaperworkloadreaped) | warning | 0m |
| [`ReaperWorkloadSkipped`](#reaperworkloadskipped) | info | 30m |
| [`ReaperActionFailed`](#reaperactionfailed) | critical | 0m |
| [`ReaperStalled`](#reaperstalled) | critical | 5m |
| [`ReaperConfigInvalid`](#reaperconfiginvalid) | warning | 5m |
| [`ReaperDown`](#reaperdown) | critical | 10m |

### ReaperCrashLoopPlanned

```promql
reaper_workload_state{state="candidate"} == 1
```

For `5m`, severity `warning`.

**What it means.** A workload has met both thresholds (`crashLoopDuration`
and `minRestarts`, or `failingDuration` and `minFailedRuns` for a CronJob)
and is a reap candidate.

- **In report mode it fires for every candidate. This is by design:** it is
  how report mode tells you what full mode would stop.
- **In full mode** a workload that is reaped passes through `candidate`
  within one cycle, so this alert only fires for candidates that are not
  being reaped: skipped ones. `ReaperWorkloadSkipped` follows after 30
  minutes with the reason.

**What to do.**

1. Look at the workload and the plan:

   ```sh
   kubectl events -n <namespace> --for <kind>/<name>
   kubectl get <kind> -n <namespace> <name> \
     -o jsonpath='{.metadata.annotations.reaper\.boop\.ninja/plan}{"\n"}'
   ```

2. Fix the crash, or decide the workload should not be reaped: label it
   `reaper.boop.ninja/mode=report`, or raise its thresholds
   ([configuration.md](configuration.md#4-per-namespace-and-per-workload-settings)).
3. The alert resolves when the workload recovers (healthy for
   `recoveryWindow`), is reaped, or is opted out.

### ReaperWorkloadReaped

```promql
reaper_workload_state{state="reaped"} == 1
```

For `0m`, severity `warning`.

**What it means.** Full mode stopped this workload: scaled to 0, or a CronJob
suspended, with Flux held off. It stays firing until the workload is
restored.

**What to do.**

1. Read why: the `Reaped` Event's note names the container, restart count,
   last exit and the Flux holds placed.

   ```sh
   kubectl events -n <namespace> --for <kind>/<name>
   ```

2. Fix the cause in Git or in the image.
3. Restore it with `reaper.boop.ninja/restore=true`. A fix pushed through
   Flux does not reach a reaped workload until you restore it. See
   [restoring-a-workload.md](restoring-a-workload.md).

If the workload shares a HelmRelease with others, the whole release is frozen
while it is reaped
([restoring-a-workload.md](restoring-a-workload.md#6-shared-helmreleases)).

### ReaperWorkloadSkipped

```promql
reaper_workload_skipped == 1
```

For `30m`, severity `info`.

**What it means.** A candidate has been skipped for 30 minutes. The `reason`
label says why. It is informational: the reaper is working as configured.

**What to do.** Look up the reason in
[troubleshooting.md](troubleshooting.md#a-candidate-is-never-reaped). In
short:

| `reason` | Usual action |
| -------- | ------------ |
| `ReportOnlyScope` | Expected if you set `mode=report` on the namespace or workload. Otherwise check `flux.kustomize` and `flux.helmRelease`. |
| `CannotScaleToZero` | A DaemonSet. Fix it by hand; the reaper never acts on DaemonSets. |
| `ManagedByController` | An operator owns it. Fix it through the operator. |
| `ManagedByAutoscaler` | A KEDA ScaledObject targets it. Fix it, or pause the ScaledObject. |
| `PartiallyAvailable` | Some replicas still serve. Usually a bad rollout: roll back. |
| `RateLimited` | Waiting for `maxReapsPerCycle`. Clears on the next cycles. |
| `ReapCeilingReached` | `maxReaped` workloads are already reaped. Restore some, or raise the limit. |

The alert resolves when the skip reason goes away, which can mean the
workload is about to be reaped.

### ReaperActionFailed

```promql
increase(reaper_actions_total{result="error"}[15m]) > 0
```

For `0m`, severity `critical`.

**What it means.** A reap or restore step failed against the Kubernetes API
in the last 15 minutes. The alert has the `action` and `mode` labels but not
the workload. It resolves 15 minutes after the last error.

The reaper retries on the next cycle from what is visible on the objects. A
half-done reap shows the state `reaping`, and finishes or rolls forward;
nothing is done twice.

**What to do.**

1. Find the workload from the Events:

   ```sh
   kubectl get events -A --field-selector reason=ReapFailed
   kubectl get events -A --field-selector reason=RestoreFailed
   ```

   The note reads `reap step failed: <error>; retried next cycle`.
2. Check the reaper's logs for `write failed; retrying next cycle`.
3. Typical causes:
   - **`Forbidden`**: the reaper's RBAC is missing a verb. See
     [troubleshooting.md](troubleshooting.md#forbidden-in-the-logs).
   - **Conflict**: something else changed the object at the same moment.
     The next cycle normally succeeds; if it repeats, find the other writer.
   - **Not found**: the workload or its HelmRelease was deleted mid-reap.
4. If the same action keeps failing, the alert stays firing because
   `increase` keeps seeing new errors.

### ReaperStalled

```promql
time() - reaper_last_cycle_timestamp_seconds > 600
```

For `5m`, severity `critical`.

**What it means.** No cycle has finished for more than 10 minutes. The
reaper is not detecting, reaping or restoring anything. Restore annotations
are not acted on.

The gauge is `0` until the first cycle finishes, so this also fires when a
freshly started reaper never gets its watch caches synced.

**What to do.**

1. Check the pod and its readiness:

   ```sh
   kubectl -n k8s-reaper get pods
   kubectl -n k8s-reaper logs deploy/k8s-reaper --tail=100
   ```

2. If the last log line is `waiting for caches to sync`, look for repeated
   `watch error; retrying with backoff` lines. They are almost always RBAC
   (`Forbidden`) or API server trouble.
3. If cycles started and stopped, the loop is stuck on an API call. Restart
   the pod; the reaper resumes from the state written on the objects.

   ```sh
   kubectl -n k8s-reaper rollout restart deploy/k8s-reaper
   ```

If no cycle has started for `max(10 × cycleInterval, 10m)` either, the
liveness probe restarts the pod by itself. A cycle that started but never
finishes keeps the reaper unready (`Ready` reason `Stalled`) until then.

### ReaperConfigInvalid

```promql
reaper_config_valid == 0
```

For `5m`, severity `warning`.

**What it means.** The latest `ReaperConfig` generation failed validation
(for example a duration out of range). The reaper keeps using the last valid
config, or the built-in defaults if it has never seen a valid one. **Your
change has not taken effect.**

**What to do.**

```sh
kubectl get rc default \
  -o jsonpath='{.status.conditions[?(@.type=="Valid")].message}{"\n"}'
kubectl get events -n default --field-selector reason=ConfigInvalid
```

Fix the fields named in the message and apply again. See
[configuration.md](configuration.md#5-when-the-config-is-invalid) and
[troubleshooting.md](troubleshooting.md#the-reaperconfig-is-invalid).

### ReaperDown

```promql
absent(up{job="<release fullname>"} == 1)
```

For `10m`, severity `critical`. The chart fills in the job name, which is
the release's full name (`k8s-reaper` for a release named `k8s-reaper`).

**What it means.** Prometheus has had no successful scrape of the reaper for
10 minutes. Either the reaper is down, or Prometheus cannot see it.

**What to do.**

1. Is the pod running?

   ```sh
   kubectl -n k8s-reaper get pods
   kubectl -n k8s-reaper describe pod -l app.kubernetes.io/name=k8s-reaper
   ```

2. If it runs, is the target in Prometheus (Status, Targets)? If not:
   - `serviceMonitor.enabled` is `false`, or its `labels` do not match the
     Prometheus `serviceMonitorSelector` (see [section 4](#4-scraping-and-alerts-with-the-helm-chart));
   - you scrape the reaper with your own config, under another `job` name.
3. If the reaper has been down, workloads it reaped stay reaped and restore
   annotations wait. Nothing is lost: it picks up from the objects when it
   comes back.

## 6. Useful queries

```promql
# Workloads by state, per namespace
sum by (namespace, state) (reaper_workload_state)

# Everything reaped right now
reaper_workload_state{state="reaped"} == 1

# Skipped candidates, by reason
sum by (reason) (reaper_workload_skipped)

# The ten longest crash loops, in hours
topk(10, reaper_workload_crashloop_seconds) / 3600

# Reaps in the last day (full mode)
sum(increase(reaper_actions_total{action="reap", result="success"}[24h]))

# New candidates in the last day (report mode)
sum(increase(reaper_actions_total{action="reap", result="planned"}[24h]))

# Restores in the last day
sum(increase(reaper_actions_total{action="restore", result="success"}[24h]))

# Failed steps, by action
sum by (action) (increase(reaper_actions_total{result="error"}[1h]))

# Share of cycles that ended in an error
sum(rate(reaper_cycles_total{result="error"}[15m])) / sum(rate(reaper_cycles_total[15m]))

# p95 cycle duration
histogram_quantile(0.95, sum by (le) (rate(reaper_cycle_duration_seconds_bucket[15m])))

# Seconds since the last cycle
time() - reaper_last_cycle_timestamp_seconds

# The mode and config generation actually in use
reaper_config_info

# Crash-looping pods the reaper cannot track
reaper_unowned_crashlooping_pods > 0

# Notification results, per channel
sum by (channel, result) (increase(reaper_notifications_total[1h]))

# Running version
reaper_build_info
```

## 7. The ReaperConfig status

```sh
kubectl get reaperconfig
```

```
NAME      MODE     VALID   CANDIDATES   REAPED   AGE
default   report   True    3            0        5d
```

| Column | From | Meaning |
| ------ | ---- | ------- |
| `MODE` | `spec.mode` | The mode you asked for. If the latest generation is invalid, the reaper may still be running an older mode. `reaper_config_info{mode}` shows the one in use. |
| `VALID` | condition `Valid` | `False` while the latest generation is invalid. |
| `CANDIDATES` | `status.counts.candidate` | Candidates, skipped ones included. |
| `REAPED` | `status.counts.reaped` | Workloads reaped right now. |

The full status also has `counts.observed`, `counts.reaping`,
`counts.skipped`, `observedGeneration`, `activeGeneration`, `lastCycleTime`
and a `Ready` condition (see
[getting-started.md](getting-started.md#44-the-reaperconfig-status)).

The status is written at the end of each cycle. If the reaper stops, nothing
updates it: `Ready` keeps saying `True` and the counts go stale. Check
`lastCycleTime` to know how fresh it is:

```sh
kubectl get rc default -o jsonpath='{.status.lastCycleTime}{"\n"}'
```

If no `ReaperConfig` object exists, the reaper runs on built-in defaults and
has nowhere to write a status. The metrics still work.

## 8. Kubernetes Events

The reaper records an Event (`events.k8s.io/v1`, reporting controller
`reaper.boop.ninja/k8s-reaper`) when something changes: a workload is first
seen crash-looping, becomes a candidate, is skipped for a new reason, is
reaped or restored, or recovers. It does not repeat them every cycle.

The reasons and what each means are listed in
[getting-started.md](getting-started.md#42-events) (report mode) and
[getting-started.md](getting-started.md#what-full-mode-does) (full mode).
The full contract, with each Event's action and note, is in
[contracts/events.md](../../specs/001-crashloop-reaper/contracts/events.md).

```sh
# Everything about one workload
kubectl events -n shop --for deploy/api

# One reason across the cluster
kubectl get events -A --field-selector reason=Reaped

# Events about the ReaperConfig (cluster-scoped, so they land in `default`)
kubectl get events -n default --field-selector reason=ConfigInvalid
```

A few details worth knowing:

- Events about a workload are on the workload (Deployment, StatefulSet,
  DaemonSet, CronJob), not on its pods. `HelmReleaseHeld` and
  `HelmReleaseReleased` are on the HelmRelease.
- The `Restored` note starts with how the restore was triggered:
  `requested` (the annotation), `manual-scale-up`, `manual-resume`, or
  `opted-out` (a `reaper.boop.ninja/mode=off` label on a reaped workload).
- An invalid `reaper.boop.ninja/mode` value or threshold annotation produces
  a `ConfigInvalid` Event on that namespace or workload, once until it is
  fixed.
- Kubernetes keeps Events for a limited time (one hour by default). For a
  durable record, rely on the metrics, or run an event exporter.
- Alertmanager does not read Events. Alerts come from the metrics.

## 9. Logs

The reaper logs to stdout as JSON, one object per line. Set the format and
level with chart values or environment variables:

| Chart value | Variable | Default | Values |
| ----------- | -------- | ------- | ------ |
| `logFormat` | `REAPER_LOG_FORMAT` (or `--log-format`) | `json` | `json`, `text` |
| `logLevel`  | `RUST_LOG` | `info` | A `tracing` filter: `debug`, `warn`, or per target such as `info,kube_client=warn` |

```sh
kubectl -n k8s-reaper logs deploy/k8s-reaper --tail=200
```

Messages worth searching for:

| Message | Level | Meaning |
| ------- | ----- | ------- |
| `optional CRDs discovered` | info | At startup: whether KEDA and Flux HelmRelease CRDs were found. |
| `waiting for caches to sync` / `caches synced` | info | Startup. Not ready until the second one. |
| `config applied` | info | A new `ReaperConfig` generation is in use. |
| `invalid ReaperConfig; keeping the last valid config` | warn | See `ReaperConfigInvalid`. |
| `reading ReaperConfig failed; keeping the active config` | warn | The GET failed; the cycle continues on the current config. |
| `ReaperConfig removed; using built-in defaults` | info | The object was deleted. |
| `invalid reaper setting ignored` | warn | A bad mode label or threshold annotation, with the object, key and value. |
| `crash-looping pod has no supported owner; not tracked` | info | A pod counted in `reaper_unowned_crashlooping_pods`. Logged every cycle while it lasts. |
| `cycle triggered by a watched change` | info | A restore request or manual scale-up started a cycle early. |
| `write failed; retrying next cycle` | warn | A write to a workload or HelmRelease failed. |
| `emitting Event failed` | warn | The Event could not be created. The action itself still happened. |
| `writing ReaperConfig status failed` | warn | The status subresource could not be patched. |
| `watch error; retrying with backoff` | warn | A watch failed. Repeated, it usually means RBAC. |
| `notification channel ignored` | warn | See [notifications.md](notifications.md#4-check-that-it-works). |

The text format is easier to read while debugging:

```yaml
# HelmRelease values
logFormat: text
logLevel: debug
```

## 10. OpenTelemetry

Set `otel.endpoint` in the chart to push metrics over OTLP as well. It sets
`OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_SERVICE_NAME` (from
`otel.serviceName`, default `k8s-reaper`). Empty, the default, keeps OTLP off.

```yaml
otel:
  endpoint: http://otel-collector.observability:4318
  serviceName: k8s-reaper
```

- The exporter uses **OTLP over HTTP**, so point it at the collector's HTTP
  receiver (port `4318` by convention), not gRPC.
- The standard OTel variables also work through `extraEnv`, for example
  `OTEL_RESOURCE_ATTRIBUTES` or a signal-specific
  `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`.
- **Metrics** are the same instruments as on `/metrics`, with `.` in place of
  `_` and no `_total`: `reaper.workload.state`, `reaper.actions`,
  `reaper.cycles`, `reaper.cycle.duration.seconds`, and so on. Labels are the
  same.
- **Traces**: the reaper installs an OTLP trace exporter, but this release
  records no spans of its own, so expect little or nothing on the traces
  side.
- **Logs** are not sent over OTLP. Collect them from stdout.

`/metrics` keeps working when OTLP is on. You can use either or both.
