# k8s-reaper

A Kubernetes operator that finds workloads stuck crash-looping and, after a
threshold, either reports them or scales them to zero, so a broken Deployment
stops restarting every five minutes forever and a human gets told.

- **`report` mode** (default): observes, emits Kubernetes Events and metrics,
  annotates the workload with what it _would_ do. Never changes replicas.
- **`full` mode**: records the original replica count, scales the workload to
  zero and keeps Flux from scaling it straight back up (disables kustomize
  reconciliation on the object, or suspends the owning HelmRelease).
- **Restore** is one annotation: the reaper puts back the replicas and undoes
  exactly the Flux changes it made.

## Status

All six user stories are built and pass on a dev cluster: report, reap with
Flux held off, one-step restore, runtime config, CronJobs, and notifications.
The Helm chart is in review in
[mbround18/helm-charts#251](https://github.com/mbround18/helm-charts/pull/251).
The design lives in
[`specs/001-crashloop-reaper/`](specs/001-crashloop-reaper/spec.md).

## Guides

- [Getting started](docs/guides/getting-started.md)
- [Configuration](docs/guides/configuration.md)
- [Restoring a workload](docs/guides/restoring-a-workload.md)
- [Notifications](docs/guides/notifications.md)

Building or changing the reaper: see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option (`MIT OR Apache-2.0`).
