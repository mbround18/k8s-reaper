# Quickstart: validating 001

Contracts: [annotations](./contracts/annotations.md),
[labels](./contracts/labels.md), [events](./contracts/events.md),
[metrics](./contracts/metrics.md), [config](./contracts/config.md).

## 0. Without a cluster

```bash
make test            # fmt check, clippy -D warnings, core unit + adapter contract tests
```

Expected: every core case passes in under 10 s (SC-007). Each case asserts
report-mode plan JSON == full-mode plan JSON (SC-004).

## 1. Local cluster (k3d + Flux)

```bash
make dev-cluster     # k3d cluster, Flux installed, CRD applied, deploy/dev samples
make image dev-load  # build via Dockerfile (cargo-chef), import into k3d
make dev-run         # run the reaper in-cluster in namespace k8s-reaper, report mode
```

`deploy/dev/` contains these fixtures:

| Fixture                   | Shape                                                          | Expect                    |
| ------------------------- | -------------------------------------------------------------- | ------------------------- |
| `plain/crashloop`         | Deployment, `replicas: 2`, `command: [false]`, no Flux labels  | candidate → reaped (full) |
| `kustomize/crashloop`     | Same, applied by a Flux Kustomization                          | + reconcile disabled      |
| `helm/crashloop`          | Same, from a Flux HelmRelease of a local chart                 | + HelmRelease suspended   |
| `partial/rollout`         | 2 healthy pods, new ReplicaSet crash-looping                   | `PartiallyAvailable`      |
| `ds/crashloop`            | DaemonSet crash-looping                                        | `CannotScaleToZero`       |
| `job/fail`                | Job that fails                                                 | ignored                   |
| `optout/crashloop`        | Deployment labelled `reaper.boop.ninja/mode=off`                | untouched                 |
| `cron/failing`            | CronJob `* * * * *`, container exits 1                         | candidate → suspended (full) |

For speed, the dev ReaperConfig uses `crashLoopDuration: 5m`,
`minRestarts: 3`, `cronJobs.failingDuration: 10m` and `cronJobs.minFailedRuns:
3` (the lowest values validation allows).

## 2. Report mode (US1)

```bash
kubectl get deploy,sts,ds -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state
kubectl events -n plain --for deploy/crashloop        # CrashLoopObserved, then ReapPlanned
kubectl get deploy -n plain crashloop -o jsonpath='{.metadata.annotations.reaper\.boop\.ninja/plan}'
curl -s localhost:8080/metrics | grep reaper_workload_state
kubectl get deploy -n plain crashloop -o jsonpath='{.spec.replicas}'   # still 2
```

## 3. Full mode (US2, US4)

```bash
kubectl patch reaperconfig default --type merge -p '{"spec":{"mode":"full"}}'
# next cycle, no restart:
kubectl get deploy -n plain crashloop -o jsonpath='{.spec.replicas}'   # 0
kubectl get deploy -n kustomize crashloop \
  -o jsonpath='{.metadata.annotations.kustomize\.toolkit\.fluxcd\.io/reconcile}'  # disabled
kubectl get helmrelease -n helm crashloop -o jsonpath='{.spec.suspend}'           # true
```

Force Flux three times and confirm replicas stay 0 (SC-002):

```bash
kubectl -n flux-system annotate kustomization dev-kustomize \
  reconcile.fluxcd.io/requestedAt="$(date -Is)" --overwrite
```

With `maxReapsPerCycle: 1` and three candidates, it takes three cycles, and
`ReapSkipped reason=RateLimited` appears on those waiting.

## 4. Restore (US3)

```bash
kubectl annotate deploy -n kustomize crashloop reaper.boop.ninja/restore=true
# within a cycle:
kubectl get deploy -n kustomize crashloop -o yaml | grep -E 'replicas:|reaper\.boop\.ninja|reconcile'
# replicas: 2; no reaper.boop.ninja/* keys; no reconcile annotation
kubectl scale deploy -n plain crashloop --replicas=1   # manual scale-up = restore, keeps 1
```

## 5. Integration suite

```bash
make test-integration                   # all stories, needs Docker
make test-integration STORY=us3_restore # one story
```

## 6. On roanoke (after the chart and HelmRelease land, report mode)

```bash
kubectl get deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state=candidate
```

Expected (SC-006): the six `slam-prod` Deployments and `fathom-dev/crashloop`.
Not expected: the `slam-prod/ingest-*` Job pods or `fathom-dev/bad-image`. The
`ingest` CronJob appears only if it has failed every run for 6 hours (US5).
