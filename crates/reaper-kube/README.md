# reaper-kube

The kube-rs adapter: Kubernetes objects in as the core's `Snapshot`, core writes out as API calls.

- `snapshot`: watch-backed `Caches` and the pure object-to-view mapping.
- `writer`: `KubeWriter`, the `ClusterWriter` port (merge patches with a
  `resourceVersion` precondition, the `scale` subresource).
- `events`: `KubeEventSink`, the `EventSink` port over `events.k8s.io/v1`.
- `crd`: the `ReaperConfig` CRD type and `crd_yaml()`.
- `config_watch`: reads the ReaperConfig each cycle and writes its status.

The CRD YAML is checked against `tests/golden/crd.yaml`. Regenerate with
`UPDATE_GOLDEN=1 cargo test -p reaper-kube --test crd_golden`.

See [CONTRIBUTING.md, Architecture](../../CONTRIBUTING.md#architecture).
