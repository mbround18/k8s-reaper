# Helm chart

The k8s-reaper Helm chart lives in
[mbround18/helm-charts](https://github.com/mbround18/helm-charts) under
`charts/k8s-reaper` (app `0.2.0`). It installs the
`ReaperConfig` CRD, a single reaper pod with least-privilege RBAC, a metrics
Service, and `ReaperConfig/default` in report mode. A ServiceMonitor and a
PrometheusRule are available but off by default. This guide covers installing
it with Flux or plain Helm, every value, what each template creates, and how to
upgrade and uninstall it safely.

The chart is published in the Helm repository
`https://mbround18.github.io/helm-charts/`, which is the simplest way to
install it ([below](#install-from-the-helm-repository)). You can also install
it from Git: with a Flux `GitRepository` ([section 1](#1-install-with-flux)),
or with `helm install` from a clone ([section 2](#2-install-with-helm-from-a-clone)).

## Install from the Helm repository

With Helm:

```sh
helm repo add mbround18 https://mbround18.github.io/helm-charts/
helm install k8s-reaper mbround18/k8s-reaper -n k8s-reaper --create-namespace
```

Add `-f my-values.yaml` for your values ([section 3](#3-values)). Helm
installs the CRD on the first install only ([section 7](#7-crd-upgrades)).

With Flux, point a `HelmRepository` at it and install with a `HelmRelease`.
The reaper never acts on the namespace it runs in, so give it its own:

```yaml
apiVersion: source.toolkit.fluxcd.io/v1
kind: HelmRepository
metadata:
  name: mbround18
  namespace: flux-system
spec:
  interval: 1h
  url: https://mbround18.github.io/helm-charts/
---
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
      chart: k8s-reaper
      version: 0.1.x
      sourceRef:
        kind: HelmRepository
        name: mbround18
        namespace: flux-system
  install:
    crds: CreateReplace
  upgrade:
    crds: CreateReplace
  values:
    reaperConfig:
      spec:
        mode: report
```

`version: 0.1.x` takes new patch releases of the chart and nothing newer. As
with the Git source, keep `install.crds` and `upgrade.crds` at
`CreateReplace` so a new chart version updates the CRD.

## 1. Install with Flux

Point Flux at the chart repository:

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

Install the chart into its own namespace. The reaper never acts on the
namespace it runs in, so do not share that namespace with workloads you want
watched.

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
    reaperConfig:
      spec:
        mode: report
```

`install.crds` and `upgrade.crds` matter. Flux's defaults are `Create` on
install and `Skip` on upgrade, so without `upgrade.crds: CreateReplace` a new
chart version never updates the CRD (see [section 7](#7-crd-upgrades)).

`reconcileStrategy` decides when Flux upgrades the release:

| Strategy                  | Flux upgrades when                                                      |
| ------------------------- | ----------------------------------------------------------------------- |
| `Revision`                | The `GitRepository` gets any new commit, even one to another chart.     |
| `ChartVersion` (default)  | `version` in the chart's `Chart.yaml` changes.                          |

To pin the chart exactly, set `ref.commit` (or a tag, if the repository has
one) in the `GitRepository` instead of `ref.branch`.

Check the install:

```sh
kubectl -n k8s-reaper get pods
kubectl get reaperconfig default
```

```
NAME      MODE     VALID   CANDIDATES   REAPED   AGE
default   report   True    0            0        2m
```

## 2. Install with Helm from a clone

```sh
git clone https://github.com/mbround18/helm-charts.git
helm install k8s-reaper ./helm-charts/charts/k8s-reaper \
  --namespace k8s-reaper --create-namespace \
  -f my-values.yaml
```

Helm installs the CRD from the chart's `crds/` directory on the first install.
It never upgrades or deletes it afterwards ([section 7](#7-crd-upgrades),
[section 9](#9-uninstall)).

To see exactly what the chart renders with your values:

```sh
helm template k8s-reaper ./helm-charts/charts/k8s-reaper -n k8s-reaper -f my-values.yaml
```

## 3. Values

Every key in `values.yaml`:

| Key                          | Default                          | Description |
| ---------------------------- | -------------------------------- | ----------- |
| `image.repository`           | `mbround18/k8s-reaper`           | Image repository. The same image is published to `ghcr.io/mbround18/k8s-reaper`. |
| `image.tag`                  | `""`                             | Image tag. Empty uses the chart's `appVersion` (`0.2.0`). |
| `image.pullPolicy`           | `IfNotPresent`                   | Container `imagePullPolicy`. |
| `imagePullSecrets`           | `[]`                             | Pod `imagePullSecrets`, for a private mirror. |
| `nameOverride`               | `""`                             | Replaces the chart name in resource names and the `app.kubernetes.io/name` label. |
| `fullnameOverride`           | `""`                             | Replaces the full resource name. Otherwise it is `<release>-k8s-reaper`, or just the release name when that already contains `k8s-reaper`. |
| `serviceAccount.create`      | `true`                           | Create the ServiceAccount. |
| `serviceAccount.name`        | `""`                             | ServiceAccount name. Empty uses the full name, or `default` when `create` is `false`. |
| `serviceAccount.annotations` | `{}`                             | Annotations on the ServiceAccount. |
| `rbac.create`                | `true`                           | Create the ClusterRole and ClusterRoleBinding ([section 5](#5-rbac)). If `false`, you must grant the same permissions yourself. |
| `reaperConfig.create`        | `true`                           | Render the cluster-scoped `ReaperConfig` from `reaperConfig.spec`. Set `false` to manage the object yourself ([section 6](#6-the-reaperconfig)). |
| `reaperConfig.name`          | `default`                        | Name of the `ReaperConfig` to render and to read. Always passed to the reaper as `REAPER_CONFIG_NAME`, also when `create` is `false`. |
| `reaperConfig.spec`          | `{mode: report}`                 | The `ReaperConfig` spec, free-form. Any field from [configuration.md](configuration.md). Unset fields take the reaper's defaults. An empty spec still renders `mode: report`. |
| `notifications.secretName`   | `k8s-reaper-notifications`       | Secret of notification channel URLs to mount, `optional: true`. `""` drops the mount ([section 4](#the-notifications-secret)). |
| `notifications.mountPath`    | `/etc/k8s-reaper/notifications`  | Where the Secret is mounted. Passed as `REAPER_NOTIFICATIONS_DIR`. |
| `logFormat`                  | `json`                           | `json` or `text`. Passed as `REAPER_LOG_FORMAT`. |
| `logLevel`                   | `info`                           | Log filter, passed as `RUST_LOG`, for example `info,reaper_core=debug`. |
| `otel.endpoint`              | `""`                             | OTLP endpoint for traces and metrics, passed as `OTEL_EXPORTER_OTLP_ENDPOINT`. Empty keeps OTLP off. |
| `otel.serviceName`           | `k8s-reaper`                     | `OTEL_SERVICE_NAME`. Only set when `otel.endpoint` is set. |
| `extraEnv`                   | `[]`                             | Extra container environment variables, for example `OTEL_RESOURCE_ATTRIBUTES`. No environment variable changes what the reaper reaps. |
| `podAnnotations`             | `{}`                             | Annotations on the pod. |
| `podLabels`                  | `{}`                             | Extra labels on the pod. |
| `service.type`               | `ClusterIP`                      | Service type. |
| `service.port`               | `8080`                           | Service port. It always targets the container's `http` port (8080). |
| `service.annotations`        | `{}`                             | Annotations on the Service. |
| `serviceMonitor.enabled`     | `false`                          | Create a ServiceMonitor. Needs the Prometheus Operator CRDs. |
| `serviceMonitor.interval`    | `30s`                            | Scrape interval. |
| `serviceMonitor.scrapeTimeout` | `""`                           | Scrape timeout. Empty leaves it to Prometheus. |
| `serviceMonitor.honorLabels` | `true`                           | Keep the reaper's own `namespace` label, which names the workload's namespace, instead of letting Prometheus rename it to `exported_namespace`. |
| `serviceMonitor.labels`      | `{}`                             | Extra labels, so your Prometheus selects the ServiceMonitor. |
| `prometheusRule.enabled`     | `false`                          | Create a PrometheusRule with the reaper's alerts. Needs the Prometheus Operator CRDs. |
| `prometheusRule.labels`      | `{}`                             | Extra labels, so your Prometheus selects the rules. |
| `prometheusRule.runbookUrl`  | the [restoring-a-workload](restoring-a-workload.md) guide on GitHub | `runbook_url` annotation on every alert. |
| `resources`                  | requests `cpu: 10m`, `memory: 32Mi`; limits `memory: 128Mi` | Container resources. |
| `podSecurityContext`         | non-root, uid/gid `65532`, `RuntimeDefault` seccomp | Pod security context ([section 5](#5-rbac)). |
| `securityContext`            | non-root, no privilege escalation, read-only root filesystem, all capabilities dropped, `RuntimeDefault` seccomp | Container security context. |
| `priorityClassName`          | `""`                             | Pod `priorityClassName`. |
| `nodeSelector`               | `{}`                             | Pod `nodeSelector`. |
| `tolerations`                | `[]`                             | Pod `tolerations`. |
| `affinity`                   | `{}`                             | Pod `affinity`. |

Replicas and the update strategy are not values. The chart always renders one
replica with the `Recreate` strategy (see [section 5](#5-rbac)).

The reaper's memory grows with the number of pods, Jobs and workloads it
caches. On a small production cluster it used about 58Mi, above the 32Mi
request. Raise `resources.requests.memory` to match what you see, and raise
the 128Mi limit on a large cluster.

## 4. What the chart creates

| Resource                        | Name                        | When                                | `argocd.argoproj.io/sync-wave` |
| ------------------------------- | --------------------------- | ----------------------------------- | ------------------------------ |
| CustomResourceDefinition        | `reaperconfigs.reaper.boop.ninja` | First install (from `crds/`)  | none |
| ServiceAccount                  | full name                   | `serviceAccount.create`             | `0` |
| ClusterRole, ClusterRoleBinding | full name                   | `rbac.create`                       | `0` |
| ReaperConfig (cluster-scoped)   | `reaperConfig.name`         | `reaperConfig.create`               | `0` |
| Deployment                      | full name                   | always                              | `30` |
| Service                         | full name                   | always                              | `40` |
| ServiceMonitor                  | full name                   | `serviceMonitor.enabled`            | `40` |
| PrometheusRule                  | full name                   | `prometheusRule.enabled`            | `40` |

The full name is `k8s-reaper` for a release called `k8s-reaper`.

The sync-wave annotations are a convention of the chart repository, which
annotates its charts for Argo CD (and whose tests check them). They do nothing
under Flux or plain Helm: Helm installs the CRD first and orders the rest by
kind. The ReaperConfig also
carries `argocd.argoproj.io/sync-options: SkipDryRunOnMissingResource=true`,
because its CRD may not exist yet when Argo CD runs a dry run.

### The Deployment

- One replica, `Recreate` strategy, container args `run`.
- Port `8080` named `http`, serving `/healthz` (liveness), `/readyz`
  (readiness) and `/metrics`. `/readyz` fails until the caches have synced and
  a cycle has completed, and again if cycles stop finishing.
- Environment set by the chart:

  | Variable                      | Value |
  | ----------------------------- | ----- |
  | `REAPER_NAMESPACE`            | The pod's namespace, from the downward API. This namespace is always excluded. |
  | `REAPER_CONFIG_NAME`          | `reaperConfig.name` |
  | `REAPER_HTTP_ADDR`            | `0.0.0.0:8080` |
  | `REAPER_LOG_FORMAT`           | `logFormat` |
  | `RUST_LOG`                    | `logLevel` |
  | `REAPER_NOTIFICATIONS_DIR`    | `notifications.mountPath`, only when `notifications.secretName` is set |
  | `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME` | Only when `otel.endpoint` is set |

  `extraEnv` is appended after these.

### The notifications Secret

The chart mounts the Secret named by `notifications.secretName` read-only at
`notifications.mountPath`, with `optional: true`:

- **The chart never creates the Secret.** Create it yourself in the release
  namespace, one key per channel and one URL per key. Channel URLs never go
  into values, so they stay out of Helm release history.
- **A missing Secret is fine.** The pod starts, and notifications stay off
  until the Secret exists.
- **`secretName: ""`** drops the volume, the mount and
  `REAPER_NOTIFICATIONS_DIR`. Notifications are then off.
- Changes to the Secret take effect without a restart, once the kubelet syncs
  the mount.

```sh
kubectl -n k8s-reaper create secret generic k8s-reaper-notifications \
  --from-literal=ops-discord=https://discord.com/api/webhooks/<id>/<token>
```

Which events are sent is set in `reaperConfig.spec.notifications`. See
[notifications.md](notifications.md) for channel URLs and filters.

### ServiceMonitor and PrometheusRule

Both are off by default, and both need the Prometheus Operator CRDs. Add the
labels your Prometheus selects on:

```yaml
serviceMonitor:
  enabled: true
  labels:
    release: kube-prometheus-stack
prometheusRule:
  enabled: true
  labels:
    release: kube-prometheus-stack
```

The ServiceMonitor scrapes the `http` port at `/metrics` in the release
namespace only.

The PrometheusRule defines `ReaperCrashLoopPlanned`, `ReaperWorkloadReaped`,
`ReaperWorkloadSkipped`, `ReaperActionFailed`, `ReaperStalled`,
`ReaperConfigInvalid` and `ReaperDown`. `ReaperDown` matches
`up{job="<full name>"}`, the Service name, which is the ServiceMonitor's
default job label. What each alert means and what to do is in
[monitoring.md](monitoring.md).

`ReaperDown` fires when the ServiceMonitor is enabled but your Prometheus
does not select it (its `labels` do not match the Prometheus
`serviceMonitorSelector`). That is correct: nothing is scraping the reaper.
Fix the labels rather than silencing the alert.

## 5. RBAC

`rbac.create` renders one ClusterRole and binds it to the ServiceAccount. In
short:

| Resources                                                  | Verbs                         | Why |
| ---------------------------------------------------------- | ----------------------------- | --- |
| pods, namespaces, replicasets, jobs                        | get, list, watch              | Detect crash loops and failing CronJob runs; read namespace mode labels. |
| deployments, statefulsets, daemonsets, cronjobs            | get, list, watch, patch       | Write its own labels and annotations; suspend CronJobs; set the Flux reconcile annotation. |
| deployments/scale, statefulsets/scale                      | get, patch, update            | Scale to 0 and back. |
| horizontalpodautoscalers, KEDA scaledobjects               | get, list, watch              | Detect autoscalers that would fight a scale-down. |
| Flux helmreleases                                          | get, list, watch, patch       | Suspend and resume a HelmRelease that rendered a reaped workload. |
| events (`events.k8s.io`)                                   | create, patch                 | Record its Events. |
| reaperconfigs                                              | get, list, watch              | Read its configuration. |
| reaperconfigs/status                                       | get, patch, update            | Write the status and counts. |

It has no `delete` verb anywhere, no access to Secrets, no wildcards, and no
`escalate`, `bind` or `impersonate`. Notification URLs are read from the
mounted Secret, not through the API. In report mode the reaper only uses its
write permissions for its own labels, annotations, Events and status.

### Security context

- The pod runs as uid and gid `65532`, non-root, with the `RuntimeDefault`
  seccomp profile.
- The container has a read-only root filesystem, no privilege escalation and
  all capabilities dropped. The reaper writes nothing to disk.

### Why one replica and `Recreate`

The reaper has no leader election. Two running at once would both act on the
same workloads and each apply the reap limits on its own. So the chart always
runs one replica, and `Recreate` stops the old pod before the new one starts on
every upgrade. The short gap is harmless: everything the reaper knows is
written on the objects themselves, and the new pod carries on from there.

## 6. The ReaperConfig

With `reaperConfig.create: true` (the default), the chart renders the
cluster-scoped `ReaperConfig/<reaperConfig.name>` from `reaperConfig.spec`.
Values merge with the chart's defaults, so setting only `limits` keeps
`mode: report`:

```yaml
reaperConfig:
  spec:
    limits:
      maxReaped: 10
```

renders:

```yaml
spec:
  limits:
    maxReaped: 10
  mode: report
```

**If the chart manages the ReaperConfig, change it in the values, not with
`kubectl`.** A `kubectl edit` or `kubectl patch` works until the next Helm
upgrade of the release (a chart or values change, or drift correction if the
HelmRelease has drift detection on). That upgrade puts the values back,
including `mode`. To switch modes, change `reaperConfig.spec.mode` in Git.

To manage the object yourself, for example to keep it in a separate
Kustomization or let operators edit it with `kubectl`, set:

```yaml
reaperConfig:
  create: false
  name: default   # still tells the reaper which object to read
```

Then create `ReaperConfig/default` yourself
([configuration.md](configuration.md)). Without it, the reaper runs on the
built-in defaults, which are report mode.

Switching `create` from `true` to `false` on an existing release makes Helm
delete the object it rendered. Apply your own copy straight after, or the
reaper falls back to the defaults (report mode) on its next cycle. Workloads
already reaped stay reaped either way.

Only one `ReaperConfig` name is read per reaper, and the object is
cluster-scoped. Do not install the chart twice in one cluster.

## 7. CRD upgrades

The CRD is in the chart's `crds/` directory. It is generated by
`k8s-reaper crd` and must match the image's version.

Helm installs files in `crds/` on the first install only. It never upgrades
them on `helm upgrade` and never deletes them on `helm uninstall`.

- **Flux:** set `install.crds: CreateReplace` and `upgrade.crds:
  CreateReplace` in the HelmRelease ([section 1](#1-install-with-flux)). Flux
  then replaces the CRD on every upgrade.
- **Plain Helm:** apply the CRD yourself before each upgrade:

  ```sh
  kubectl apply -f ./helm-charts/charts/k8s-reaper/crds/
  ```

  Or print it from the image you are about to run:

  ```sh
  docker run --rm mbround18/k8s-reaper:0.2.0 crd | kubectl apply -f -
  ```

## 8. Upgrade

### The chart

With Flux, the release upgrades according to `reconcileStrategy`
([section 1](#1-install-with-flux)). To pick up a change now:

```sh
flux reconcile source git mbround18-helm-charts -n flux-system
flux reconcile helmrelease k8s-reaper -n k8s-reaper
```

With Helm:

```sh
git -C helm-charts pull
kubectl apply -f ./helm-charts/charts/k8s-reaper/crds/
helm upgrade k8s-reaper ./helm-charts/charts/k8s-reaper -n k8s-reaper -f my-values.yaml
```

Always pass your values file. `helm upgrade` without it resets every value to
the chart's default, including `reaperConfig.spec`, which puts the reaper back
in report mode.

### The image

The image tag follows the chart's `appVersion` unless `image.tag` is set. To
run a different release of the reaper with the same chart:

```yaml
image:
  tag: 0.1.0
```

Image tags have no `v` prefix. Upgrade the CRD to the same version first, as
in [section 7](#7-crd-upgrades).

An upgrade restarts the single pod (`Recreate`). Reaped workloads stay reaped,
Flux holds stay in place, and the new pod continues where the old one stopped.
A reap that was interrupted halfway is finished on the next cycle.

## 9. Uninstall

**Restore every reaped workload before you uninstall.** Nothing undoes a reap
once the reaper is gone. A reaped workload stays at 0 replicas or suspended,
keeps `kustomize.toolkit.fluxcd.io/reconcile: disabled`, and its HelmRelease
stays suspended, so Flux will not fix or restart it either.

### 1. Restore everything

Restore works in every mode. Request it on every workload that is reaped or
being reaped:

```sh
for kind in deployment statefulset cronjob; do
  kubectl get "$kind" -A -l 'reaper.boop.ninja/state in (reaping,reaped)' \
    -o jsonpath='{range .items[*]}{.metadata.namespace}{" "}{.metadata.name}{"\n"}{end}' |
  while read -r ns name; do
    kubectl annotate "$kind/$name" -n "$ns" reaper.boop.ninja/restore=now --overwrite
  done
done
```

Each restore normally takes a few seconds and at most one cycle. See
[restoring-a-workload.md](restoring-a-workload.md) for what a restore does.

To stop new reaps while you do this, switch to report mode first
(`reaperConfig.spec.mode: report`). Report mode alone does not restore
anything.

### 2. Check that nothing is held

Both commands must print nothing:

```sh
kubectl get deploy,sts,cronjob -A -l 'reaper.boop.ninja/state in (reaping,reaped)'
kubectl get helmrelease -A \
  -o jsonpath='{range .items[*]}{.metadata.namespace}/{.metadata.name}{" "}{.metadata.annotations.reaper\.boop\.ninja/held-for}{"\n"}{end}' |
  awk 'NF > 1'
```

`kubectl get reaperconfig default` should also show `REAPED` as `0`.

### 3. Remove the release

With Flux, delete the HelmRelease from Git and let the parent Kustomization
prune it (it needs `prune: true`), or delete it directly:

```sh
kubectl -n k8s-reaper delete helmrelease k8s-reaper
```

With Helm:

```sh
helm uninstall k8s-reaper -n k8s-reaper
```

This removes the Deployment, Service, RBAC, ServiceAccount, the monitoring
objects and the chart-rendered `ReaperConfig`. A `ReaperConfig` you created
yourself (`reaperConfig.create: false`) stays.

### 4. Remove the CRD (optional)

Neither Helm nor Flux deletes a CRD from `crds/`. The
`reaperconfigs.reaper.boop.ninja` CRD stays after uninstall. Delete it if you
are not reinstalling. Deleting a CRD deletes every `ReaperConfig` with it:

```sh
kubectl delete crd reaperconfigs.reaper.boop.ninja
```

### 5. Leftover labels and annotations (optional)

Workloads that were only `observed` or `candidate` keep the
`reaper.boop.ninja/state` label and the reaper's annotations, such as
`reaper.boop.ninja/plan`. Nothing reads them once the reaper is gone. To
remove them, remove the annotations first, while the label still selects the
workloads:

```sh
kubectl annotate deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state \
  reaper.boop.ninja/crashloop-since- reaper.boop.ninja/healthy-since- \
  reaper.boop.ninja/plan- reaper.boop.ninja/failing-since- \
  reaper.boop.ninja/failed-runs- reaper.boop.ninja/last-failed-job-
kubectl label deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state reaper.boop.ninja/state-
```

`reaper.boop.ninja/mode` and the threshold annotations were set by people.
Remove them yourself if you no longer want them.

## Next steps

- [getting-started.md](getting-started.md): a first install, step by step.
- [rolling-out.md](rolling-out.md): going from report to full mode in
  production.
- [configuration.md](configuration.md): every `reaperConfig.spec` field.
- [monitoring.md](monitoring.md): metrics, alerts and runbooks.
- [flux.md](flux.md): how Flux holds work.
