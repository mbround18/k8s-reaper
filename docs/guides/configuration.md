# Configuration

k8s-reaper is configured in three places:

- **The `ReaperConfig` object.** Cluster-wide settings: the mode, how long a
  workload must crash-loop before it counts, which namespaces are in scope,
  how many workloads may be reaped, Flux handling, CronJobs and notifications.
- **Labels and annotations on namespaces and workloads.** Teams use them to
  opt out, to stay report-only, or to make the reaper more patient with one
  workload.
- **Process flags and environment variables.** Ports, log format and the
  like. They never change what the reaper reaps, and are not covered here.

With no configuration at all, the reaper runs in report mode with the defaults
listed below: it records what it would do and changes nothing.

## 1. The ReaperConfig object

`ReaperConfig` (group `reaper.boop.ninja`, version `v1alpha1`, short name `rc`)
is cluster-scoped. The reaper reads exactly one object, named `default`. To
read a different one, start the reaper with `--config-name` or
`REAPER_CONFIG_NAME`. Other `ReaperConfig` objects are not used.

The reaper reads the object at the start of every cycle. An edit takes effect
on the next cycle, with no restart.

Every field is optional, so the smallest valid object is:

```yaml
apiVersion: reaper.boop.ninja/v1alpha1
kind: ReaperConfig
metadata:
  name: default
spec: {}
```

```sh
kubectl get rc
```

```
NAME      MODE     VALID   CANDIDATES   REAPED   AGE
default   report   True    7            0        3d
```

## 2. Durations

Durations are written as whole numbers with the units `h`, `m` and `s`,
combined if you like: `90s`, `30m`, `6h`, `1h30m`.

- There is no day unit. `7d` is rejected; write `168h`. For one day write
  `24h`.
- There are no fractions and no `ms`. `1.5h` is rejected; write `1h30m`.

The CRD checks this form when you apply the object, so a badly written
duration never reaches the reaper. Each field's allowed range is checked by
the reaper itself (see [section 5](#5-when-the-config-is-invalid)).

## 3. Field reference

### Top level

| Field           | Type     | Default  | Allowed        | What it does |
| --------------- | -------- | -------- | -------------- | ------------ |
| `mode`          | string   | `report` | `report`, `full` | `report` records what would happen and changes nothing. `full` reaps. |
| `cycleInterval` | duration | `60s`    | `10s` to `10m` | Time between cycles. A watched change, such as a restore request, can start a cycle sooner. |

There is no global `off` mode. To stop the reaper entirely, scale its
Deployment to zero. `off` exists only as a per-namespace or per-workload
setting ([section 4](#4-per-namespace-and-per-workload-settings)).

### `detection`

When a Deployment, StatefulSet or DaemonSet counts as broken. A workload
becomes a candidate only when **both** thresholds are met.

A DaemonSet cannot be scaled to zero, so a broken one is reported (skip
reason `CannotScaleToZero`) but never reaped.

| Field               | Type     | Default | Allowed          | What it does |
| ------------------- | -------- | ------- | ---------------- | ------------ |
| `crashLoopDuration` | duration | `30m`   | `5m` to `168h`   | How long the workload must have been crash-looping without a break. |
| `minRestarts`       | integer  | `10`    | `1` to `10000`   | Restart count of the crash-looping container. |
| `recoveryWindow`    | duration | `10m`   | `1m` to `24h`    | How long a crash-looping workload must stay healthy before the reaper forgets it. If it crashes again inside the window, the original crash-loop start is kept. |

### `scope`

Which namespaces the reaper looks at. Entries are namespace names or globs:
`*` matches any run of characters and `?` matches exactly one, so `shop-*`
matches `shop-prod` but not `shop`.

| Field               | Type            | Default | What it does |
| ------------------- | --------------- | ------- | ------------ |
| `excludeNamespaces` | list of globs   | `kube-system`, `kube-public`, `kube-node-lease`, `flux-system` | Namespaces never looked at. |
| `includeNamespaces` | list of globs   | empty   | If set, only matching namespaces are looked at. Empty means all namespaces. |

- **Setting a list replaces its default.** If you set `excludeNamespaces`,
  repeat the four system namespaces unless you mean to include them.
- **Exclude wins.** A namespace that matches both lists is excluded.
- **The reaper's own namespace is always excluded**, whatever the lists say.
- Entries must not be empty strings.
- **Excluding a namespace restores what the reaper reaped there.** On its
  next cycle the reaper restores each workload it has reaped (or started to
  reap) in a namespace that left scope, with a `Restored` Event noted
  `opted-out`, and then stops looking at the namespace. Removing a namespace
  from `includeNamespaces` does the same. This happens in report mode too, but
  not while the global mode is `off`.

### `limits`

Brakes on how much the reaper does in full mode. Candidates are taken in
order, the one that has been crash-looping longest first. Paused CronJobs count
toward the same limits as Deployments and StatefulSets.

| Field              | Type    | Default | Allowed      | What it does |
| ------------------ | ------- | ------- | ------------ | ------------ |
| `maxReapsPerCycle` | integer | `1`     | `0` to `50`  | New reaps started in one cycle. The rest are skipped with `RateLimited` and considered again next cycle. |
| `maxReaped`        | integer | `5`     | `0` to `500` | No new reap starts while this many workloads are reaped (or being reaped). The rest are skipped with `ReapCeilingReached`. |

Setting either limit to `0` means nothing new is reaped, even in full mode:
every candidate is reported as skipped. A reap that is already under way is
always finished.

### `flux`

How the reaper stops Flux from undoing a reap.

| Field         | Type | Default | What it does |
| ------------- | ---- | ------- | ------------ |
| `kustomize`   | bool | `true`  | On a reaped workload applied by a Flux Kustomization, set `kustomize.toolkit.fluxcd.io/reconcile: disabled`. |
| `helmRelease` | bool | `true`  | Suspend the HelmRelease that rendered a reaped workload. |

If a step is turned off and a workload needs it, that workload
is reported but never reaped (skip reason `ReportOnlyScope`), because Flux
would scale it straight back up. Suspending a HelmRelease freezes every object
in that release, not only the reaped workload.

### `cronJobs`

When a CronJob counts as failing. A failing CronJob is paused
(`spec.suspend: true`) instead of scaled to zero. Running Jobs are never
stopped or deleted. A CronJob becomes a candidate only when **both**
thresholds are met: every run has failed for at least `failingDuration`, and
at least `minFailedRuns` failed runs have been counted.

| Field             | Type     | Default | Allowed          | What it does |
| ----------------- | -------- | ------- | ---------------- | ------------ |
| `enabled`         | bool     | `true`  |                  | Watch CronJobs at all. Turning it off stops watching, but restoring a CronJob that is already paused still works. |
| `failingDuration` | duration | `6h`    | `10m` to `168h`  | How long every run must have failed. The streak starts when the first Job after the last success fails. |
| `minFailedRuns`   | integer  | `3`     | `1` to `1000`    | Failed runs counted in the streak. It keeps an hourly or daily job from being paused after one or two failures. |

A CronJob that is already suspended is never a candidate.

### `notifications`

Which state changes are sent to chat or email, the cluster name shown in
messages, and how often a repeat is allowed. The fields are `enabled`,
`clusterName`, `reasons`, `minInterval` and `channels`. Channel URLs live in a
Secret, never in the ReaperConfig.

See [Notifications](./notifications.md) for every field, its default and the
Secret setup.

## 4. Per-namespace and per-workload settings

These keys are set by people and the reaper never changes or deletes them.

### Mode: `reaper.boop.ninja/mode`

Set it as a label or an annotation on a Namespace, or on a Deployment,
StatefulSet, DaemonSet or CronJob. Put it on the workload's own metadata, not
on the pod template.

| Value         | Effect |
| ------------- | ------ |
| `off`         | Ignored entirely: not watched, annotated or reported. Any state the reaper holds is cleared. A workload the reaper has reaped is restored first. |
| `report`      | Watched and reported, never acted on, even when the global mode is `full` (skip reason `ReportOnlyScope`). |
| anything else | Ignored. The reaper records a `ConfigInvalid` Warning Event on that object. |

The effective mode is the most restrictive of the global mode, the namespace
and the workload, in the order `off` < `report` < `full`. There is no `full`
value: a label or annotation can only lower the mode. If one object has both
a label and an annotation, the label is used.

```sh
# A team opts its whole namespace out
kubectl label namespace shop-staging reaper.boop.ninja/mode=off

# One workload is reported but never reaped
kubectl label deployment -n shop-prod catalog reaper.boop.ninja/mode=report

# Back to the global mode
kubectl label namespace shop-staging reaper.boop.ninja/mode-
```

### Thresholds: per-workload annotations

A team can make the reaper more patient with one workload. These are
annotations on the workload (not labels, and not on the namespace):

| Annotation                             | On                                  | Raises                        | Allowed         |
| -------------------------------------- | ----------------------------------- | ----------------------------- | --------------- |
| `reaper.boop.ninja/crashloop-duration` | Deployment, StatefulSet, DaemonSet  | `detection.crashLoopDuration` | `5m` to `168h`  |
| `reaper.boop.ninja/min-restarts`       | Deployment, StatefulSet, DaemonSet  | `detection.minRestarts`       | `1` to `10000`  |
| `reaper.boop.ninja/failing-duration`   | CronJob                             | `cronJobs.failingDuration`    | `10m` to `168h` |
| `reaper.boop.ninja/min-failed-runs`    | CronJob                             | `cronJobs.minFailedRuns`      | `1` to `1000`   |

**An override can only raise a threshold.** The reaper uses the larger of the
annotation and the global value. An annotation below the global value has no
effect; it never makes the reaper act sooner.

A value that cannot be parsed, or is outside the allowed range, is ignored:
the global value applies and the reaper records a `ConfigInvalid` Warning
Event on the workload.

There is no override for `recoveryWindow`.

## 5. When the config is invalid

Some mistakes are caught by the Kubernetes API server, so `kubectl apply`
fails and nothing changes:

- `mode` other than `report` or `full`;
- an integer outside its range (`minRestarts`, `maxReapsPerCycle`,
  `maxReaped`, `minFailedRuns`);
- a duration not in the `h`/`m`/`s` form, such as `7d`;
- `notifications.clusterName` longer than 63 characters.

Others are only caught by the reaper: a duration outside its range (for
example `crashLoopDuration: 1m`), an empty namespace entry, or an unknown
notification reason. The object is stored, but the reaper does not use it:

- **The last valid configuration stays active.** If the reaper has not seen a
  valid generation since it started, the defaults stay active.
- A `ConfigInvalid` Warning Event is recorded on the ReaperConfig, once per
  generation. Its note lists every field that failed and why.
- The `Valid` condition becomes `False`, with the same field errors in its
  message.
- `status.observedGeneration` moves to the new generation, but
  `status.activeGeneration` stays at the generation in use.
- The metric `reaper_config_valid` drops to `0`.

Fix the object and apply it again. The next cycle uses it, records a
`ConfigApplied` Event and sets `Valid` back to `True`.

```sh
kubectl get rc default \
  -o jsonpath='{.status.observedGeneration} {.status.activeGeneration}{"\n"}'
kubectl get rc default \
  -o jsonpath='{.status.conditions[?(@.type=="Valid")].message}{"\n"}'
kubectl get events -A --field-selector reason=ConfigInvalid
```

A field error reads, for example:

```
spec.detection.crashLoopDuration: 1m is out of range 5m..168h
```

A misspelt field name is not a ReaperConfig field, so the reaper never sees
it. Check what was stored with `kubectl get rc default -o yaml`.

### When the object is deleted

If the ReaperConfig is deleted, the next cycle runs on the built-in defaults,
including `mode: report`. Workloads already reaped stay reaped until they are
restored. Re-create the object to apply your settings again.

## 6. Examples

### Minimal: switch on full mode

Everything else keeps its default:

```yaml
apiVersion: reaper.boop.ninja/v1alpha1
kind: ReaperConfig
metadata:
  name: default
spec:
  mode: full
```

### Tenant namespaces (`shop-*`)

The `shop-*` namespaces hold tenant workloads, and they are in scope like any
other namespace. Pick the approach that fits.

**Start with the tenants only.** Run full mode, but only on `shop-*`, while
everything else stays out of scope. `excludeNamespaces` is left out, so its
default still applies:

```yaml
apiVersion: reaper.boop.ninja/v1alpha1
kind: ReaperConfig
metadata:
  name: default
spec:
  mode: full
  scope:
    includeNamespaces: ["shop-*"]
```

**Keep the tenants out entirely.** Add `shop-*` to the exclude list, and
repeat the defaults, because setting the list replaces them:

```yaml
apiVersion: reaper.boop.ninja/v1alpha1
kind: ReaperConfig
metadata:
  name: default
spec:
  mode: full
  scope:
    excludeNamespaces:
      - kube-system
      - kube-public
      - kube-node-lease
      - flux-system
      - "shop-*"
```

Anything already reaped in `shop-*` is restored on the next cycle, with a
`Restored` Event noted `opted-out`; after that the namespaces are not looked
at.

**Let one tenant stay report-only.** No change to the ReaperConfig; the
tenant labels its own namespace:

```sh
kubectl label namespace shop-prod reaper.boop.ninja/mode=report
```

**Give a workload that restarts a lot more room.** If a workload in a tenant
namespace restarts often by design, its owners can raise its thresholds. With
the global defaults (`30m`, `10`), this Deployment becomes a candidate only
after 6 hours of crash-looping and 200 restarts:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ingest-api
  namespace: shop-prod
  annotations:
    reaper.boop.ninja/crashloop-duration: 6h
    reaper.boop.ninja/min-restarts: "200"
spec:
  # ...
```

```sh
kubectl annotate deployment -n shop-prod ingest-api \
  reaper.boop.ninja/crashloop-duration=6h \
  reaper.boop.ninja/min-restarts=200
```

Annotation values are strings, so quote numbers in YAML.

### Full: every field

Every field with its default, except `mode`, which is set to `full`.
Applying this is the same as the defaults plus full mode:

```yaml
apiVersion: reaper.boop.ninja/v1alpha1
kind: ReaperConfig
metadata:
  name: default
spec:
  mode: full                   # report | full
  cycleInterval: 60s           # 10s to 10m
  detection:
    crashLoopDuration: 30m     # 5m to 168h
    minRestarts: 10            # 1 to 10000
    recoveryWindow: 10m        # 1m to 24h
  scope:
    excludeNamespaces: [kube-system, kube-public, kube-node-lease, flux-system]
    includeNamespaces: []      # empty = all namespaces
  limits:
    maxReapsPerCycle: 1        # 0 to 50
    maxReaped: 5               # 0 to 500
  flux:
    kustomize: true
    helmRelease: true
  cronJobs:
    enabled: true
    failingDuration: 6h        # 10m to 168h
    minFailedRuns: 3           # 1 to 1000
  notifications:               # see notifications.md
    enabled: true
    clusterName: ""            # up to 63 characters
    reasons: [Reaped, ReapFailed, Restored, RestoreFailed, HelmReleaseHeld, ConfigInvalid]
    minInterval: 1h            # 0s to 24h
    channels: {}               # per-channel filters, keyed by Secret key
```
