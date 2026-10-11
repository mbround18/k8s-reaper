# Getting started

k8s-reaper watches your cluster for workloads that have been crash-looping for
a long time. It has two modes:

- **`report`** (the default) tells you what is broken and what it would do.
  It never changes replicas, Flux objects or CronJobs.
- **`full`** stops the broken workload: it scales it to zero, or pauses a
  CronJob, and keeps Flux from undoing that.

This guide installs the reaper with Flux, leaves it in report mode while you
read what it finds, and then switches it to full mode.

## 1. Install with Flux

The Helm chart lives in
[mbround18/helm-charts](https://github.com/mbround18/helm-charts) under
`charts/k8s-reaper`. Point Flux at the chart repository with a `GitRepository`:

```yaml
apiVersion: source.toolkit.fluxcd.io/v1
kind: GitRepository
metadata:
  name: mbround18-helm-charts
  namespace: flux-system
spec:
  interval: 1h
  url: https://github.com/mbround18/helm-charts
  ref:
    branch: main
```

Then install the chart into its own namespace with a `HelmRelease`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: k8s-reaper
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: k8s-reaper
  namespace: k8s-reaper
spec:
  interval: 30m
  chart:
    spec:
      chart: ./charts/k8s-reaper
      reconcileStrategy: Revision
      sourceRef:
        kind: GitRepository
        name: mbround18-helm-charts
        namespace: flux-system
  install:
    crds: CreateReplace
  upgrade:
    crds: CreateReplace
  values:
    # ReaperConfig/default. See configuration.md for the fields.
    reaperConfig:
      spec:
        mode: report
    # Both are off by default. The labels are whatever your Prometheus
    # selects on, here kube-prometheus-stack's.
    serviceMonitor:
      enabled: true
      labels:
        release: kube-prometheus-stack
    prometheusRule:
      enabled: true
      labels:
        release: kube-prometheus-stack
```

Commit both files to your Flux repository and let Flux apply them.

The chart installs:

- the `ReaperConfig` CRD;
- the reaper Deployment (one replica) and its RBAC;
- a metrics Service, plus the ServiceMonitor and the PrometheusRule with the
  alerts when they are enabled;
- `ReaperConfig/default` with `mode: report`.

The reaper never acts on its own namespace. By default it also skips
`kube-system`, `kube-public`, `kube-node-lease` and `flux-system`.

## 2. Check that it runs

```sh
kubectl -n k8s-reaper get pods
kubectl get reaperconfig default
```

`reaperconfig` has the short name `rc`. The output looks like this:

```
NAME      MODE     VALID   CANDIDATES   REAPED   AGE
default   report   True    0            0        2m
```

`VALID` must be `True`. If it is `False`, see [section 4.4](#44-the-reaperconfig-status).

## 3. Stay in report mode

Leave the reaper in report mode for a while, ideally days. In report mode it
only writes its own labels and annotations, and Events. Nothing else on the
cluster changes.

A workload goes through these states:

| State       | Meaning |
| ----------- | ------- |
| `observed`  | A container is crash-looping. The reaper notes when it started. |
| `candidate` | It has crash-looped for at least 30 minutes **and** the container has restarted at least 10 times. Full mode would act now. |
| `reaping`   | Full mode only: a reap is in progress. |
| `reaped`    | Full mode only: the workload is stopped. |

A CronJob becomes a candidate when every run has failed for at least 6 hours
**and** at least 3 runs have failed. One successful run clears it.

A workload that stays healthy for 10 minutes loses its state. Image pull
failures (`ImagePullBackOff`, `ErrImagePull`) are not crash loops and are
ignored. So are pods of standalone Jobs.

These thresholds are the defaults. To change them, see
[configuration.md](configuration.md).

## 4. Read the reports

### 4.1 The state label

Every workload the reaper is tracking carries the label
`reaper.boop.ninja/state`. List them all:

```sh
kubectl get deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state
```

Only the candidates:

```sh
kubectl get deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state=candidate
```

### 4.2 Events

The reaper records a Kubernetes Event when something changes, not on every
cycle. List one reason across the cluster:

```sh
kubectl get events -A --field-selector reason=ReapPlanned
kubectl get events -A --field-selector reason=ReapSkipped
```

Or see everything about one workload:

```sh
kubectl events -n shop --for deploy/api
```

The reasons you will see in report mode:

| Reason                 | Type    | Meaning |
| ---------------------- | ------- | ------- |
| `CrashLoopObserved`    | Normal  | A workload started crash-looping. |
| `ReapPlanned`          | Warning | A workload is a candidate. The note lists what full mode would do. |
| `ReapSkipped`          | Warning | A candidate the reaper would not act on, even in full mode. The note reads `reason=<reason>; would: …`. |
| `CrashLoopRecovered`   | Normal  | A workload stayed healthy for the recovery window. |
| `JobFailuresObserved`  | Normal  | A CronJob's latest Job failed. |
| `JobFailuresRecovered` | Normal  | A CronJob's Job succeeded again. |
| `ConfigApplied`        | Normal  | A new ReaperConfig took effect. |
| `ConfigInvalid`        | Warning | A ReaperConfig change, or a mode label value, is invalid. The last valid config is kept. |

Events about the ReaperConfig itself are in the `default` namespace, because
the ReaperConfig is cluster-scoped:

```sh
kubectl get events -n default --field-selector reason=ConfigInvalid
```

The skip reasons in a `ReapSkipped` note:

| Skip reason           | Why the reaper will not act |
| --------------------- | --------------------------- |
| `OptedOut`            | The workload or its namespace opted out. |
| `ReportOnlyScope`     | A `reaper.boop.ninja/mode=report` label or annotation applies. |
| `CannotScaleToZero`   | A DaemonSet. DaemonSets are always report-only. |
| `UnsupportedOwner`    | The pod's owner is a kind the reaper does not handle. |
| `ManagedByController` | Another controller owns the workload, for example a database operator. |
| `ManagedByAutoscaler` | An autoscaler, such as a KEDA ScaledObject, would fight the scale-down. |
| `PartiallyAvailable`  | At least one replica is still available, for example during a bad rollout. |
| `RateLimited`         | Full mode only: the per-cycle limit was reached. Retried next cycle. |
| `ReapCeilingReached`  | Full mode only: the limit of workloads reaped at once was reached. |

### 4.3 Annotations on the workload

The reaper writes its findings onto the workload. Show them all:

```sh
kubectl get deploy -n shop api -o yaml | grep 'reaper\.boop\.ninja'
```

| Annotation                          | Value |
| ----------------------------------- | ----- |
| `reaper.boop.ninja/crashloop-since` | When the crash loop was first seen (RFC 3339, UTC). |
| `reaper.boop.ninja/healthy-since`   | When the workload became healthy again, while it waits out the recovery window. |
| `reaper.boop.ninja/plan`            | The plan, as JSON. Set from `candidate` onward. |

The plan is the same in both modes. What report mode shows is exactly what
full mode will do. Read it on its own:

```sh
kubectl get deploy -n shop api -o jsonpath='{.metadata.annotations.reaper\.boop\.ninja/plan}'
```

```json
{"actions":[{"scaleTo":{"replicas":0}},{"disableKustomizeReconcile":"Deployment/shop/api"}]}
```

A skipped candidate shows the skip reason and what it would have done:
`{"skip":"PartiallyAvailable","would":[…]}`.

On a CronJob, `crashloop-since` is replaced by three keys:

| Annotation                          | Value |
| ----------------------------------- | ----- |
| `reaper.boop.ninja/failing-since`   | When the failing streak began. |
| `reaper.boop.ninja/failed-runs`     | How many failed Jobs the reaper has counted. |
| `reaper.boop.ninja/last-failed-job` | The name of the last failed Job counted. |

### 4.4 The ReaperConfig status

The status sums up the whole cluster:

```sh
kubectl get reaperconfig default -o jsonpath='{.status}' ; echo
```

| Field                | Meaning |
| -------------------- | ------- |
| `counts`             | How many workloads are `observed`, `candidate`, `reaping`, `reaped` and `skipped`. |
| `conditions` `Valid` | `False` when the spec is invalid. The message lists the field errors. |
| `conditions` `Ready` | `True` when a cycle completed recently. |
| `activeGeneration`   | The config generation in use. It lags `observedGeneration` while the spec is invalid. |
| `lastCycleTime`      | When the last cycle finished. |

### 4.5 Metrics and alerts

The reaper serves Prometheus metrics on port `8080` at `/metrics`. The chart's
ServiceMonitor scrapes them (`serviceMonitor.enabled: true`). To look by hand
(the Deployment is named after the release, here `k8s-reaper`):

```sh
kubectl -n k8s-reaper port-forward deploy/k8s-reaper 8080:8080
curl -s localhost:8080/metrics | grep '^reaper_'
```

| Metric                                | What it tells you |
| ------------------------------------- | ----------------- |
| `reaper_workload_state`               | 1 per workload, labelled with `namespace`, `kind`, `name` and `state`. |
| `reaper_workload_skipped`             | 1 while a candidate is skipped, with the skip `reason`. |
| `reaper_workload_crashloop_seconds`   | How long each workload has been crash-looping, or failing for a CronJob. |
| `reaper_actions_total`                | Actions by `action`, `result` and `mode`. In report mode `result` is `planned`. |
| `reaper_unowned_crashlooping_pods`    | Crash-looping pods per namespace with no supported owner, such as bare pods. These get no Event. |
| `reaper_config_valid`                 | 0 when the latest ReaperConfig is invalid. |
| `reaper_last_cycle_timestamp_seconds` | When the last cycle finished. |

The chart's alerts fire on these metrics:

| Alert                    | Fires when |
| ------------------------ | ---------- |
| `ReaperCrashLoopPlanned` | A workload has been a candidate for 5 minutes. |
| `ReaperWorkloadReaped`   | A workload is reaped. |
| `ReaperWorkloadSkipped`  | A candidate has been skipped for 30 minutes. |
| `ReaperActionFailed`     | An action failed in the last 15 minutes. |
| `ReaperStalled`          | No cycle finished for 10 minutes. |
| `ReaperConfigInvalid`    | The ReaperConfig has been invalid for 5 minutes. |
| `ReaperDown`             | The reaper has not been scraped for 10 minutes. |

To get these in chat or mail as well, see [notifications.md](notifications.md).

## 5. Keep workloads out of full mode

Before switching, decide what full mode must never touch. Set the label
`reaper.boop.ninja/mode` on a namespace or a workload:

```sh
# Ignore a workload completely: not observed, annotated or reported.
kubectl label deploy -n shop legacy-worker reaper.boop.ninja/mode=off

# Report on a whole namespace, but never act on it, even in full mode.
kubectl label namespace payments reaper.boop.ninja/mode=report
```

| Value    | Effect |
| -------- | ------ |
| `off`    | Ignored entirely. Any state the reaper had is cleared, and a reaped workload is restored first. |
| `report` | Reported, never acted on. Its candidates get `ReapSkipped` with `ReportOnlyScope`. |

There is no `full` value. A label can only lower the mode, never raise it
above the ReaperConfig's. Any other value is ignored and reported with a
Warning Event on the object.

If the workload is managed by Flux, put the label in Git, so Flux does not
remove it.

## 6. Switch to full mode

When the reports match what you expect, set `mode: full`:

```sh
kubectl patch reaperconfig default --type merge -p '{"spec":{"mode":"full"}}'
```

If the chart manages `ReaperConfig/default`, set
`reaperConfig.spec.mode: full` in the HelmRelease values in Git instead, or
Flux will put `report` back.

The change takes effect on the next cycle, with no restart. Check it:

```sh
kubectl get reaperconfig default
kubectl get events -n default --field-selector reason=ConfigApplied
```

### What full mode does

For each candidate that is not skipped, the reaper first records what it is
about to change on the workload. Then:

- **Deployments and StatefulSets** are scaled to 0. The original replica count
  is kept in `reaper.boop.ninja/original-replicas`.
- **CronJobs** get `spec.suspend: true`. Jobs that are already running keep
  running, and the reaper never deletes Jobs.
- **Workloads applied by a Flux Kustomization** (both
  `kustomize.toolkit.fluxcd.io/name` and `/namespace` labels) also get
  `kustomize.toolkit.fluxcd.io/reconcile: disabled`, so Flux does not scale
  them straight back up. Any earlier value is kept and put back on restore.
- **Workloads rendered by a Flux HelmRelease** (both
  `helm.toolkit.fluxcd.io/name` and `/namespace` labels) get that HelmRelease
  suspended. **This freezes the whole release**, not just the broken workload,
  until the last of its reaped workloads is restored. The reaper records a
  `HelmReleaseHeld` Event on the HelmRelease.
- **DaemonSets** are never acted on.

The reaper also writes `reaper.boop.ninja/reason` (which container, how many
restarts, since when) and `reaper.boop.ninja/reaped-at`, sets the state label
to `reaped`, and records a `Reaped` Event.

Full mode is careful by default:

- It reaps at most 1 workload per cycle, longest-crashing first.
- It keeps at most 5 workloads reaped at once.
- It never acts on a workload that still has an available replica.

The rest are reported with `ReapSkipped`. To change these limits, see
[configuration.md](configuration.md).

The full-mode Events:

| Reason                | Type    | Meaning |
| --------------------- | ------- | ------- |
| `Reaped`              | Warning | The workload was stopped. The note gives the original replicas (or `suspended` for a CronJob), the reason and the Flux holds placed. |
| `ReapFailed`          | Warning | A reap step failed. It is retried next cycle. |
| `HelmReleaseHeld`     | Warning | A HelmRelease was suspended for a reaped workload. |
| `HelmReleaseReleased` | Normal  | The HelmRelease no longer holds any reaped workload. |
| `Restored`            | Normal  | A workload was brought back. |
| `RestoreFailed`       | Warning | A restore step failed. It is retried next cycle. |
| `RestoreIgnored`      | Normal  | Someone asked to restore a workload the reaper had not reaped. |

See what is reaped right now:

```sh
kubectl get deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state=reaped
```

## 7. Bring a workload back

One annotation undoes everything the reaper did to a workload:

```sh
kubectl annotate deploy -n shop api reaper.boop.ninja/restore=true
```

This works in either mode. For manual scale-up, CronJobs, and fixing a
Flux-managed workload, see [restoring-a-workload.md](restoring-a-workload.md).

To go back to report mode, set `mode` to `report` again. Workloads that are
already reaped stay reaped until you restore them.

## Next steps

- [configuration.md](configuration.md): every ReaperConfig field, thresholds,
  scope and per-workload overrides.
- [restoring-a-workload.md](restoring-a-workload.md): every way back.
- [notifications.md](notifications.md): send reaps to Discord, Slack, a
  webhook or email.
