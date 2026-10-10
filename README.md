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

**Specification only.** There is no code yet. The MVP is specified in
[`specs/001-crashloop-reaper/`](specs/001-crashloop-reaper/spec.md) using
[spec-kit](https://github.com/github/spec-kit).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option (`MIT OR Apache-2.0`).
