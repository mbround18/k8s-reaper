# reaper-flux

The Flux adapter, used by `reaper-kube`.

- `helmrelease`: a minimal `HelmRelease` (`helm.toolkit.fluxcd.io/v2`),
  metadata and `spec.suspend` only.
- `holds`: `merge_body()`, the JSON merge patch that places or releases a
  HelmRelease hold, with its `resourceVersion` precondition.

What goes into a hold is decided by `reaper_core::hold`; this crate only puts
it on the wire. Tests: `cargo test -p reaper-flux`.

See [CONTRIBUTING.md, Architecture](../../CONTRIBUTING.md#architecture).
