# Quickstart: validating post-rollout hardening

Each scenario proves one success criterion on the shared kind dev cluster.
**Always pass `KUBE_CONTEXT=kind-fathom-dev`.** The `default` context is
roanoke, and the targets refuse anything that is not `kind-*` or `k3d-*` on
loopback. What each scenario expects is defined in [contracts/](./contracts/)
and [data-model.md](./data-model.md).

## 0. Prerequisites

```sh
export KUBE_CONTEXT=kind-fathom-dev
make test                                   # unit, contract and contracts-drift tests
make image dev-load dev-run KUBE_CONTEXT=$KUBE_CONTEXT
kubectl --context $KUBE_CONTEXT -n k8s-reaper rollout status deploy/k8s-reaper
```

The dev ReaperConfig uses `cycleInterval: 15s`, so the windows below are
short. The liveness window is still at least 10m.

## 1. Health (US1): SC-001, SC-002

```sh
make test-integration STORY=us7_health KUBE_CONTEXT=$KUBE_CONTEXT
```

The story does three things:

1. **Invalid config at start.** It applies an invalid `ReaperConfig`, then
   restarts the reaper.
   - Expected: `/readyz` is 503 with `no valid config applied yet`, and
     `Valid` is `False`. `Ready` reports only the loop (contracts/config.md):
     `CachesNotSynced` until the first cycle finishes, then `CycleCompleted`.
     It is never `NotReady`.
   - It then fixes the config. The reaper becomes ready within one cycle,
     and `Ready` is `True` with reason `CycleCompleted`.
2. **Invalid edit while running.** It edits the config to an invalid value.
   - Expected: `Valid=False`, but `/readyz` stays 200 (owner decision Q1).
3. **Hung loop.** It sets the test-only `REAPER_DEV_HANG_AFTER=1` on a dev
   build and waits.
   - Expected: `Ready` is `False` with reason `Stalled` after 3 intervals.
   - The pod is restarted once the liveness window and the probe failures
     have passed: `restartCount` goes up within 12 min (SC-001).
   - The hook exists only in debug builds (`#[cfg(debug_assertions)]`),
     following the repo's flag rule.

## 2. No false alerts at start-up (US2): SC-003

```sh
for i in $(seq 20); do
  kubectl --context $KUBE_CONTEXT -n k8s-reaper rollout restart deploy/k8s-reaper
  kubectl --context $KUBE_CONTEXT -n k8s-reaper rollout status deploy/k8s-reaper
  curl -s localhost:8080/metrics | grep -E '^reaper_(last_cycle_timestamp_seconds|config_valid) '
done
```

Run this with a `kubectl port-forward` to port 8080.

- Expected right after each start: the timestamp is the process start time
  (never `0`), and `reaper_config_valid` is either absent or `1`.
- If Prometheus runs on the cluster, check
  `ALERTS{alertname=~"Reaper.*"}`: it stays empty throughout.
- To cover cache sync delayed past 5 minutes, scale the API server's
  watch-cache down. If that is not possible on the shared cluster, run the
  unit test `telemetry::startup_series_do_not_alert`, which evaluates both
  alert expressions against the start-up values.

## 3. Failed record writes are alerted (US2): SC-004

Remove `patch` on deployments from the reaper's ClusterRole in the dev
deploy only (`deploy/dev/rbac-deny-patch.yaml`). The reaper writes a record
only when it changes, so give it one to write: delete and re-apply
`deploy/dev/fixtures/plain.yaml`, then wait until it has crash-looped a few
times (see `deploy/dev/README.md`).

- Expected: `reaper_actions_total{action="record",result="error"}`
  increases.
- Expected: a `RecordFailed` Warning Event on `plain/crashloop`.

Then restore the ClusterRole.

## 4. ReaperDown under another release name (US2): SC-005

This check runs in `mbround18/helm-charts`:

- `helm template other-name charts/k8s-reaper --set serviceMonitor.enabled=true`
  renders `up{job="other-name-k8s-reaper"}`, which matches the Service name.
- With `serviceMonitor.enabled=false`, it renders no `ReaperDown`.

These are the chart's pytest cases. A live firing check needs Prometheus,
and the shared kind cluster has none.

## 5. Letting go (US3): SC-006, FR-010

```sh
make test-integration STORY=us8_letting_go KUBE_CONTEXT=$KUBE_CONTEXT
```

The story uses the `helm/crashloop` fixture in full mode:

1. Wait for `reaped`, with `HelmReleaseHeld` on the release.
2. Run `flux resume helmrelease crashloop -n helm`.
   - Expected within 1 cycle: a `Restored` Event whose note starts
     `manual-resume`; the replicas are back; `held-for` has no entry;
     `HelmReleaseReleased`.
3. Let `plain/crashloop` be reaped, then add `plain` to
   `scope.excludeNamespaces`.
   - Expected within 1 cycle: a `Restored` Event whose note starts
     `opted-out`; the replicas are back; the Kustomization annotation has
     been removed.
   - After that, no further Events or labels on that workload.

## 6. Contracts stay in step (US4): SC-007

```sh
cargo test -p k8s-reaper --test contracts
```

Add a fake variant to `SkipReason` locally and the build fails (exhaustive
`ALL`). Remove a row from the events contract and the test fails.

## 7. Chart (US5): SC-008, FR-019

```sh
cargo run -q -p k8s-reaper -- crd | diff - ../helm-charts-k8s-reaper/charts/k8s-reaper/crds/reaperconfigs.reaper.boop.ninja.yaml
```

- Expected: the diff is empty, header comment included.

Then follow the "Install from the Helm repository" section of
`docs/guides/helm-chart.md` on a throwaway kind cluster (not the shared
one), word for word.

- Expected: the chart installs, and the pod's `resources.requests.memory`
  is `64Mi`.
