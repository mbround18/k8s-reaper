# Contract: annotations

Prefix `reaper.boop.ninja/` (Question 1, answered). The reaper owns every key under
it; people may set only `restore` and `mode`. Changing a key or its format is a
breaking change.

## On Deployments, StatefulSets, DaemonSets

| Key                                  | Set by | When                                | Value                                                                                                   |
| ------------------------------------ | ------ | ----------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `reaper.boop.ninja/crashloop-since`   | reaper | state `observed` onward             | RFC 3339 UTC, first observation of the crash loop                                                       |
| `reaper.boop.ninja/healthy-since`     | reaper | observed/candidate and now healthy  | RFC 3339 UTC; removed again if it crashes before `recoveryWindow`                                       |
| `reaper.boop.ninja/plan`              | reaper | `candidate` onward (both modes)     | Compact JSON of the plan, e.g. `{"mode":"report","actions":[{"scaleTo":0},{"disableKustomizeReconcile":"Deployment/app/web"}]}` or `{"skip":"PartiallyAvailable","would":[…]}` |
| `reaper.boop.ninja/original-replicas` | reaper | reap step 1                         | Decimal `spec.replicas` before the reap                                                                 |
| `reaper.boop.ninja/reason`            | reaper | reap step 1                         | Human text: `container "api" CrashLoopBackOff, 14 restarts, crash-looping since 2026-10-10T08:00:00Z, last exit 1 (Error)` |
| `reaper.boop.ninja/previous-reconcile`| reaper | reap step 1, only if it set the Flux annotation | Previous value of `kustomize.toolkit.fluxcd.io/reconcile`, or the literal `<none>`             |
| `reaper.boop.ninja/helmrelease`       | reaper | reap step 2, only if Helm-managed   | `<namespace>/<name>` of the held HelmRelease                                                            |
| `reaper.boop.ninja/reaped-at`         | reaper | reap step 4                         | RFC 3339 UTC                                                                                            |
| `reaper.boop.ninja/restore`           | person | any time                            | Any non-empty value. On a `reaped`/`reaping` workload this triggers a restore; otherwise it is removed with `RestoreIgnored` |
| `reaper.boop.ninja/mode`              | person | any time                            | `off` or `report` (same meaning as the label, see labels.md); the label wins if both are present and differ, then the most restrictive applies |

Flux annotation written by the reaper (full mode only):

| Key                                     | Value      | On                                                                                       |
| --------------------------------------- | ---------- | ---------------------------------------------------------------------------------------- |
| `kustomize.toolkit.fluxcd.io/reconcile` | `disabled` | A reaped workload that has both `kustomize.toolkit.fluxcd.io/{name,namespace}` labels    |

## On HelmReleases (`helm.toolkit.fluxcd.io/v2`)

| Key                                     | Value                                                                             |
| --------------------------------------- | --------------------------------------------------------------------------------- |
| `reaper.boop.ninja/held-for`             | JSON array, sorted, of `Kind/namespace/name` of the workloads the hold is for      |
| `reaper.boop.ninja/suspended-by-reaper`  | `true` if the reaper set `spec.suspend: true`; `false` if it was already suspended |
| `reaper.boop.ninja/previous-reconcile`   | As above, only if the reaper disabled kustomize reconcile on the HelmRelease       |
| `kustomize.toolkit.fluxcd.io/reconcile` | `disabled`, only if the HelmRelease has both kustomize labels                      |

Field written: `spec.suspend: true` (only when `suspended-by-reaper=true`).

## Restore semantics

When a restore runs, the reaper:

1. Removes the workload from `held-for`. If the set is now empty:
   - if `suspended-by-reaper=true`, sets `spec.suspend: false`;
   - restores or deletes the HelmRelease's reconcile annotation from
     `previous-reconcile`;
   - deletes all `reaper.boop.ninja/*` keys on the HelmRelease.
2. Scales the workload to `original-replicas` (not for a manual scale-up).
3. On the workload, puts `kustomize.toolkit.fluxcd.io/reconcile` back to
   `previous-reconcile` (deleting it if `<none>`), and deletes every
   `reaper.boop.ninja/*` annotation (including `restore`) and the state label.

Annotations the reaper did not write are never changed.
