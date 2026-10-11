# k8s-reaper

k8s-reaper finds Kubernetes workloads that have been crash-looping for a long
time and stops them. It scales a broken Deployment or StatefulSet to zero, or
suspends a CronJob that keeps failing. It keeps Flux from bringing the workload
straight back, and tells you about it. When you have fixed the cause, one
annotation puts everything back the way it was.

A workload in `CrashLoopBackOff` costs restarts, logs, alerts and image pulls
forever, and the noise hides the problems that matter. k8s-reaper ends that
quietly: a workload that has been broken for half an hour is reaped one at a
time, and a person decides when it comes back.

```console
$ kubectl get deploy -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state
NAMESPACE   NAME     READY   UP-TO-DATE   AVAILABLE   AGE   STATE
shop        worker   0/0     0            0           24d   reaped
shop        api      0/1     1            0           24d   candidate
billing     sync     0/1     1            0           3d    observed
```

## What it does

- **Report mode, the default.** It finds crash-looping workloads, labels them,
  writes the exact plan it would carry out and emits Events and metrics. It
  never changes replicas, CronJobs or Flux objects. Start here.
- **Full mode.** It carries out the plan:
  - records the original replica count, then scales to zero or suspends the
    CronJob;
  - holds Flux back by disabling Kustomization reconciliation on the object,
    or by suspending the owning HelmRelease.
- **One-step restore.** Annotate the workload with
  `reaper.boop.ninja/restore=true`. The reaper puts the replicas back, resumes
  the CronJob and undoes exactly the Flux changes it made, nothing else.
- **Safe by default:**
  - at most one reap a minute, and at most five workloads held at once;
  - longest-broken first;
  - never its own namespace or the system namespaces;
  - never a workload with a ready replica;
  - never a DaemonSet;
  - never anything you label `reaper.boop.ninja/mode=off`.
- **Configured at runtime.** One `ReaperConfig` object holds the mode,
  thresholds, scope and limits. Changes apply on the next cycle, with no
  restart.
- **Observable.** It has Prometheus metrics, ready-made alerts, Kubernetes
  Events, JSON logs and OpenTelemetry. It can also notify Discord, Slack,
  generic webhooks or email.

What it does **not** reap: pods stuck in `ImagePullBackOff` or `Pending`,
single failed Job pods, and workloads that are only partly broken. Those are
problems for a person, not for scaling to zero.

## Install

The chart is published in the Helm repository
`https://mbround18.github.io/helm-charts/`, from
[mbround18/helm-charts](https://github.com/mbround18/helm-charts)
(`charts/k8s-reaper`). With Helm:

```sh
helm repo add mbround18 https://mbround18.github.io/helm-charts/
helm install k8s-reaper mbround18/k8s-reaper -n k8s-reaper --create-namespace
```

With Flux, from the Helm repository:

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

Or with Flux from Git:

```yaml
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
      sourceRef:
        kind: GitRepository
        name: mbround18-helm-charts # points at github.com/mbround18/helm-charts
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

Then read what it finds:

```sh
kubectl get reaperconfig default
kubectl get events -A --field-selector reason=ReapPlanned
```

[Getting started](docs/guides/getting-started.md) walks through all of it,
from install to full mode.

## Documentation

| Guide | Read it to |
| --- | --- |
| [Getting started](docs/guides/getting-started.md) | Install, read the reports, switch to full mode |
| [How it works](docs/guides/how-it-works.md) | Understand detection, states, plans and safety limits |
| [Rolling out to production](docs/guides/rolling-out.md) | Go from report to full mode safely, with a real timeline |
| [Configuration](docs/guides/configuration.md) | Every `ReaperConfig` field and per-workload override |
| [Helm chart](docs/guides/helm-chart.md) | Every chart value, upgrades and uninstalling |
| [Flux](docs/guides/flux.md) | Learn how Flux is held back and given back |
| [CronJobs](docs/guides/cronjobs.md) | Suspend CronJobs that fail every run |
| [Restoring a workload](docs/guides/restoring-a-workload.md) | Bring a reaped workload back |
| [Notifications](docs/guides/notifications.md) | Send reaps to Discord, Slack, webhooks or email |
| [Monitoring and alerts](docs/guides/monitoring.md) | Use the metrics and alerts, with a runbook per alert |
| [Troubleshooting](docs/guides/troubleshooting.md) | Find out why something did or did not happen |

The full index is in [docs/guides](docs/guides/README.md).

## Requirements

- Kubernetes: built against the 1.36 API and tested on kind and k3s.
- Flux v2 (`kustomize.toolkit.fluxcd.io/v1`, `helm.toolkit.fluxcd.io/v2`),
  only if you use Flux. Without it the reaper still reaps; there is just
  nothing to hold back.
- The image is `mbround18/k8s-reaper` on Docker Hub, for linux/amd64. Tagged
  releases are also published as `ghcr.io/mbround18/k8s-reaper`.

## Contributing

The reaper is a Rust workspace with a pure decision core and thin Kubernetes,
Flux, notification and telemetry adapters. Setup, architecture, tests and
releases are in [CONTRIBUTING.md](CONTRIBUTING.md). The design and its
contracts are in [`specs/001-crashloop-reaper/`](specs/001-crashloop-reaper/spec.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option (`MIT OR Apache-2.0`).
