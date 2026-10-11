# Troubleshooting

This guide starts from what you see and works back to the cause. Each entry
gives a symptom, the likely causes, and the fix. Most answers come from four
places: the workload's Events, its `reaper.boop.ninja/*` annotations, the
reaper's metrics, and its logs. For what each of those contains, see
[monitoring.md](monitoring.md).

## First checks

Run these before anything else:

```sh
# Is the reaper running and ready?
kubectl -n k8s-reaper get pods

# Which mode, is the config valid, how many candidates and reaped?
kubectl get reaperconfig

# What does the reaper say about this workload?
kubectl events -n shop --for deploy/api
kubectl get deploy -n shop api -o yaml | grep 'reaper\.boop\.ninja'

# Recent warnings from the reaper itself
kubectl -n k8s-reaper logs deploy/k8s-reaper --tail=200 | grep -i warn
```

Replace `shop`, `deploy/api` and the `k8s-reaper` namespace with your own.

## Detection

### A workload I expected is not labelled

The workload is crash-looping, but it has no `reaper.boop.ninja/state` label.

**Causes and fixes:**

- **Its namespace is out of scope.** The reaper never looks at its own
  namespace, at namespaces in `scope.excludeNamespaces` (by default
  `kube-system`, `kube-public`, `kube-node-lease`, `flux-system`), or, when
  `scope.includeNamespaces` is set, at namespaces that do not match it.
  Exclude wins over include. Check the lists:

  ```sh
  kubectl get rc default -o jsonpath='{.spec.scope}{"\n"}'
  ```

- **It is opted out.** A `reaper.boop.ninja/mode=off` label or annotation on
  the workload or its namespace makes the reaper ignore it entirely:

  ```sh
  kubectl get ns shop --show-labels
  kubectl get deploy -n shop api -o jsonpath='{.metadata.labels}{"\n"}{.metadata.annotations}{"\n"}'
  ```

- **It is not a crash loop.** A container counts only when it is not ready
  and is either waiting in `CrashLoopBackOff` or has just exited with a
  non-zero code. These never count:
  - `ImagePullBackOff` and `ErrImagePull`;
  - `Pending` pods (unschedulable, waiting for a volume);
  - containers that exit with code `0`;
  - containers that are running but failing their readiness probe.

- **It has no supported owner.** The reaper tracks Deployments, StatefulSets,
  DaemonSets and CronJobs. A bare pod, a ReplicaSet without a Deployment, or a
  pod owned by another kind (an Argo Rollout, an operator's custom resource) is
  not tracked. It shows up in `reaper_unowned_crashlooping_pods` and in the log
  line `crash-looping pod has no supported owner; not tracked`. Pods of
  standalone Jobs are ignored.

- **It is already at 0 replicas.** A workload with `replicas: 0` is never
  tracked.

- **CronJobs:** `cronJobs.enabled` is `false`; the CronJob is suspended; or
  no Job has finished yet. Running Jobs are not judged.

- **The reaper is not running a cycle.** See
  [`/readyz` returns 503](#readyz-returns-503).

### It is `observed` but never becomes a candidate

A candidate needs **both** thresholds: crash-looping for at least
`detection.crashLoopDuration` (default `30m`), **and** the worst container at
`detection.minRestarts` restarts or more (default `10`).

**Causes and fixes:**

- **The clock starts when the reaper first sees the crash loop.** It does not
  read how long the pod has been failing before that. After a fresh install,
  or after a workload was restored, even a pod that has been broken for days
  waits the full `crashLoopDuration`. Check when the reaper started counting:

  ```sh
  kubectl get deploy -n shop api \
    -o jsonpath='{.metadata.annotations.reaper\.boop\.ninja/crashloop-since}{"\n"}'
  ```

- **Not enough restarts yet.** The kubelet backs off restarts up to 5
  minutes apart, so reaching 10 restarts takes a while.

- **A per-workload annotation raised the thresholds.** Look for
  `reaper.boop.ninja/crashloop-duration` and `reaper.boop.ninja/min-restarts`
  on the workload. They can only raise the global values.

- **It recovered in between.** A workload that stays healthy for
  `recoveryWindow` (default `10m`) is forgotten, and the next crash loop
  starts a new clock. A shorter healthy spell keeps the original start.

- **CronJobs** need every run since the last success to have failed for
  `cronJobs.failingDuration` (default `6h`) **and** at least
  `cronJobs.minFailedRuns` (default `3`) failed Jobs. One successful Job
  clears the streak.

## Reaping

### A candidate is never reaped

The workload is labelled `candidate`, but nothing changes.

**First, check the mode actually in use.** In report mode nothing is ever
reaped; that is the point of it.

```sh
kubectl get rc default
```

The `MODE` column shows what the spec asks for. If `VALID` is `False`, the
reaper is still running an older generation, possibly in report mode. The
metric `reaper_config_info{mode}` shows the mode in use. See
[The ReaperConfig is invalid](#the-reaperconfig-is-invalid).

**Then, read the skip reason.** It is in the `ReapSkipped` Event
(`reason=<reason>; would: …`), the `skip` field of the
`reaper.boop.ninja/plan` annotation, and the `reason` label of
`reaper_workload_skipped`:

```sh
kubectl get deploy -n shop api \
  -o jsonpath='{.metadata.annotations.reaper\.boop\.ninja/plan}{"\n"}'
# {"skip":"PartiallyAvailable","would":[…]}
```

Only one reason is shown at a time, checked in the order of this table.
Clearing one can reveal the next.

| Skip reason | Cause | Fix |
| ----------- | ----- | --- |
| `CannotScaleToZero` | It is a DaemonSet. DaemonSets are always report-only. | Fix it by hand. |
| `ManagedByController` | The workload has a controller owner reference: an operator created it and would undo a scale-down. | Fix it through the operator. |
| `ManagedByAutoscaler` | A KEDA ScaledObject targets it and would scale it back up. A plain HPA does not cause this; HPAs pause at 0 replicas. | Fix the workload, or pause the ScaledObject. |
| `ReportOnlyScope` | A `reaper.boop.ninja/mode=report` label or annotation on the workload or its namespace. Or the workload is applied by a Flux Kustomization while `flux.kustomize` is `false`, or rendered by a HelmRelease while `flux.helmRelease` is `false`: without the Flux hold, Flux would scale it straight back. | Remove the label, or turn the Flux setting back on. |
| `PartiallyAvailable` | At least one replica is still available. Typical of a rollout where the new pods crash but old ones still serve. | Roll back or fix the rollout. The reaper only acts once nothing is available. |
| `RateLimited` | More candidates than `limits.maxReapsPerCycle` (default `1`) this cycle. The longest crash-looping goes first. | None needed; the rest are taken on later cycles. Raise the limit to go faster. `0` means never. |
| `ReapCeilingReached` | `limits.maxReaped` (default `5`) workloads are already reaped or being reaped. | Restore some, or raise the limit. `0` means never. |

Two notes:

- **`RateLimited` also shows in report mode.** The plan is the same in both
  modes, so with more candidates than `maxReapsPerCycle`, the extra ones are
  reported as `RateLimited`, exactly as full mode would treat them. In report
  mode nothing is ever reaped, so they stay that way.
- A `mode=off` workload never gets a skip reason: it is ignored outright.
  A pod with an unsupported owner has no workload to skip; it is counted in
  `reaper_unowned_crashlooping_pods` instead (see
  [above](#a-workload-i-expected-is-not-labelled)).

### It is stuck in `reaping`

The state label reads `reaping` and does not move to `reaped`.

**Cause:** a reap step failed. Each failure records a `ReapFailed` Event
(`reap step failed: <error>; retried next cycle`) and increments
`reaper_actions_total{result="error"}`. The reaper retries every cycle from
where it stopped.

**Fix:** read the error in the Event and fix it. The usual one is RBAC (see
[`Forbidden` in the logs](#forbidden-in-the-logs)). To back out instead, set
the restore annotation; it works on `reaping` too.

### A reaped workload came back

The workload was reaped, and later it is running again without anyone
setting the restore annotation.

**Find out who brought it back.** If the reaper noticed, there is a
`Restored` Event, and its note says how:

```sh
kubectl events -n shop --for deploy/api
```

| `Restored` note starts with | Meaning |
| --------------------------- | ------- |
| `requested` | Someone set `reaper.boop.ninja/restore`. |
| `manual-scale-up` | Someone or something set `replicas` above 0. |
| `manual-resume` | Someone or something set `spec.suspend: false` on the CronJob, or resumed the HelmRelease the reaper suspended. |
| `opted-out` | Someone labelled it `reaper.boop.ninja/mode=off`, or its namespace left scope. |

To see which client changed the replicas, look at the managed fields:

```sh
kubectl get deploy -n shop api -o yaml --show-managed-fields | grep -B3 -A10 'f:replicas'
```

**Common causes:**

- **A manual `kubectl apply` or `kubectl scale`**, or a `helm upgrade` run by
  hand, set `replicas` back. The reaper treats any scale-up of a reaped
  workload as a restore.
- **Flux, because the hold was not placed.** The reaper only holds Flux off a
  workload that carries both `kustomize.toolkit.fluxcd.io/name` and
  `/namespace` labels (Kustomization), or both `helm.toolkit.fluxcd.io/name`
  and `/namespace` labels (HelmRelease). Without them, Flux reapplies
  `replicas` on its next reconcile. The workload then crash-loops again and,
  after another `crashLoopDuration`, is reaped again. Check the labels, and
  check the `Reaped` Event's note for the holds placed.
- **Another GitOps tool** (for example Argo CD with self-heal). The reaper
  only knows how to hold Flux off. Label such workloads
  `reaper.boop.ninja/mode=report`.

## Restoring

### A restore did nothing

You set `reaper.boop.ninja/restore`, and nothing changed.

**Causes and fixes:**

- **A `RestoreIgnored` Event.** The workload is not reaped (its state is not
  `reaped` or `reaping`), so there is nothing to undo. The reaper removes the
  annotation and changes nothing else. If the workload is at 0 replicas, the
  reaper did not put it there.
- **A `RestoreFailed` Event.** A step failed; the note gives the error. The
  reaper retries every cycle.
- **The annotation is on the wrong object.** It must be on the Deployment,
  StatefulSet or CronJob's own metadata, not the pod template, a ReplicaSet,
  a pod, or the HelmRelease.
- **The reaper is not running cycles.** The annotation stays in place until
  it does. See [`/readyz` returns 503](#readyz-returns-503).
- **The HelmRelease is still suspended.** It stays suspended until every
  reaped workload it renders is restored
  ([restoring-a-workload.md](restoring-a-workload.md#6-shared-helmreleases)).

## Configuration

### The ReaperConfig is invalid

`kubectl get rc` shows `VALID False`, the `ReaperConfigInvalid` alert fires,
or there is a `ConfigInvalid` Event on the ReaperConfig.

**Cause:** a value the API server accepted but the reaper rejected: a
duration out of its range (for example `crashLoopDuration: 1m`, or `200h`
where the maximum is `168h`), an empty namespace entry, or an unknown
notification reason. The reaper keeps the last valid config.

**Fix:** read the field errors, fix them, apply again:

```sh
kubectl get rc default \
  -o jsonpath='{.status.conditions[?(@.type=="Valid")].message}{"\n"}'
# spec.detection.crashLoopDuration: 1m is out of range 5m..168h
```

Ranges for every field are in
[configuration.md](configuration.md#3-field-reference).

### `kubectl apply` or the HelmRelease rejects the ReaperConfig

**Cause:** the CRD rejected a value before the reaper saw it: a `mode` other
than `report` or `full`, an integer out of range, or a duration not written
with `h`, `m` and `s`. `7d` and `1.5h` are the usual ones. Write `168h` and
`1h30m`.

**Fix:** correct the value. If the chart renders the ReaperConfig, the
HelmRelease upgrade fails until you fix `reaperConfig.spec` in its values.

### A `ConfigInvalid` Event on a workload or namespace

**Cause:** a `reaper.boop.ninja/mode` value other than `off` or `report`, or
a threshold annotation the reaper cannot use. The note names it, for
example:

```
reaper.boop.ninja/crashloop-duration="7d" is not valid; ignored
```

The reaper ignores the value and uses the global setting. It reports each bad
value once, until it is fixed.

**Fix:** use `h`/`m`/`s` durations within the allowed range, and quote numbers
in YAML (`"200"`). See
[configuration.md](configuration.md#4-per-namespace-and-per-workload-settings).

## The reaper itself

### `/readyz` returns 503

The pod is running but not `Ready`, or `ReaperStalled` fires.

**Causes and fixes:**

The `503` body says which case it is; `kubectl -n k8s-reaper describe pod`
shows it in the `Readiness probe failed` event. The `ReaperConfig`'s `Ready`
condition gives the same answer as a reason.

**Causes and fixes:**

- **It is still starting** (`caches not synced yet`, `no cycle finished yet`;
  `Ready` reason `CachesNotSynced`). The reaper is ready only after its watch
  caches have synced and the first cycle has finished. On a large cluster this
  takes a little while. The log shows `waiting for caches to sync`, then
  `caches synced`.
- **The caches never sync.** Repeated `watch error; retrying with backoff`
  lines. Usually RBAC; see [the next entry](#forbidden-in-the-logs).
- **The config has never been valid** (`no valid config applied yet`). The
  pod started with an invalid `ReaperConfig`. Fix it (see
  [The ReaperConfig is invalid](#the-reaperconfig-is-invalid)); the reaper is
  ready within a cycle. An invalid edit to a config that was valid before
  does **not** make it unready.
- **Cycles stopped finishing** (`last cycle finished 4m12s ago`; `Ready`
  reason `Stalled`). Ready needs the last cycle to have finished within
  3 × `cycleInterval`. A cycle stuck on a slow API server makes it unready.
  If no new cycle starts within `max(10 × cycleInterval, 10m)`, `/healthz`
  fails too and the kubelet restarts the pod, which resumes from what is
  written on the objects. You can restart it sooner with
  `kubectl -n k8s-reaper rollout restart deploy/k8s-reaper`.

### `Forbidden` in the logs

Warnings such as `watch error; retrying with backoff`, `write failed;
retrying next cycle`, `emitting Event failed` or `writing ReaperConfig status
failed`, with a `403` or `forbidden` error.

**Cause:** the reaper's ServiceAccount lacks a permission. The chart's
ClusterRole covers everything the reaper needs. This usually happens with
`rbac.create: false` and a hand-written role, or when a cluster policy blocks
a verb.

**Fix:** compare with the chart's `templates/rbac.yaml`, and test the exact
permission (here, for a release named `k8s-reaper` in namespace
`k8s-reaper`):

```sh
SA=system:serviceaccount:k8s-reaper:k8s-reaper
kubectl auth can-i patch deployments --as=$SA -A
kubectl auth can-i patch deployments/scale --as=$SA -A
kubectl auth can-i patch helmreleases.helm.toolkit.fluxcd.io --as=$SA -A
kubectl auth can-i create events.events.k8s.io --as=$SA -A
kubectl auth can-i patch reaperconfigs/status --as=$SA
```

### A `404 page not found` warning at startup

One WARN line from `kube_client` mentioning `404 page not found`, right after
an install or upgrade.

**Cause:** the chart installs the `ReaperConfig` CRD and the reaper in the
same release. If the reaper starts before the API server serves the new CRD,
its first read of the ReaperConfig gets a plain `404`. You may also see
`reading ReaperConfig failed; keeping the active config` for that cycle.

**Fix:** none, if it does not repeat. The next cycle reads the object
normally. If it repeats every cycle, check that the CRD is installed:

```sh
kubectl get crd reaperconfigs.reaper.boop.ninja
```

### `ReaperDown` fires but the pod is running

**Cause:** Prometheus is not scraping it. The ServiceMonitor is disabled,
its labels do not match the Prometheus `serviceMonitorSelector`, or you
scrape with your own job name. See
[monitoring.md](monitoring.md#4-scraping-and-alerts-with-the-helm-chart).

## Notifications

### Notifications are not arriving

Check, in order:

1. `notifications.enabled` is `true`, and the reason is in
   `notifications.reasons` (and in the channel's own filter, if it has one).
   `ReapPlanned` and `ReapSkipped` are not in the defaults.
2. The Secret exists, in the reaper's namespace, with the name the chart
   mounts (`notifications.secretName`). A missing Secret silently turns
   notifications off.
3. `reaper_notification_channels` shows the channel with `valid="true"`. If
   `false`, the log line `notification channel ignored` says why.
4. `reaper_notifications_total` by `result`: `failed` is a delivery error,
   `filtered` a channel filter, `dropped` a full queue or a shutdown.
5. A repeat of the same reason for the same workload within `minInterval`
   (default `1h`) is dropped on purpose.

Details for each provider and field are in
[notifications.md](notifications.md).

## Uninstalling

### Workloads stayed at 0 after uninstalling

**Cause:** uninstalling the reaper does not undo its reaps. Reaped workloads
stay at 0 replicas (or suspended), keep `kustomize.toolkit.fluxcd.io/reconcile:
disabled`, and their HelmReleases stay suspended. Nothing is left to restore
them.

**Fix:** restore everything **before** you uninstall, while the reaper is
still running:

```sh
kubectl get deploy,sts,cronjob -A -l 'reaper.boop.ninja/state in (reaped,reaping)' \
  -o custom-columns=KIND:.kind,NS:.metadata.namespace,NAME:.metadata.name --no-headers |
while read -r kind ns name; do
  kubectl annotate "$kind/$name" -n "$ns" reaper.boop.ninja/restore=now --overwrite
done

# Wait until this prints nothing, then uninstall
kubectl get deploy,sts,cronjob -A -l reaper.boop.ninja/state
```

The second command lists every workload that still carries the state label,
including `observed` and `candidate` ones. Those are not stopped, and their
leftover annotations are harmless, but wait for no `reaped` or `reaping` to
remain.

Switching to report mode first stops new reaps while you restore.

If you already uninstalled, the simplest fix is to install the reaper again,
restore, and then uninstall. Each reaped workload records what was changed in
its `reaper.boop.ninja/*` annotations
([getting-started.md](getting-started.md#43-annotations-on-the-workload)), if
you prefer to undo it by hand.
