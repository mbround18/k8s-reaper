# Contract delta: Events, restore notes and skip reasons

Applies to [001 contracts/events.md](../../001-crashloop-reaper/contracts/events.md).
The two tables below are the ones the contracts test (`apps/k8s-reaper/tests/contracts.rs`)
reads in both directions (FR-015, SC-007). Keep their first column as plain
backticked strings.

## New reason

| Reason         | Type    | Regarding | Action field | When                                                                                         |
| -------------- | ------- | --------- | ------------ | -------------------------------------------------------------------------------------------- |
| `RecordFailed` | Warning | workload  | `Record`     | writing the reaper's own state label, record or plan annotation failed (not a conflict or not-found). Retried next cycle; counted as `reaper_actions_total{action="record",result="error"}` |

## Changed reason

The `Restored` row's note now lists all four restore notes. They are described
in the table below.

## Restore notes

The note of a `Restored` Event starts with one of these values. The same
string is the `RestoreKind`.

| Note              | When                                                                                       |
| ----------------- | ------------------------------------------------------------------------------------------ |
| `requested`       | `reaper.boop.ninja/restore=true` on the workload                                           |
| `manual-scale-up` | someone scaled a reaped Deployment or StatefulSet above zero                               |
| `manual-resume`   | someone resumed a reaped CronJob, **or the HelmRelease the reaper suspended for it** (new) |
| `opted-out`       | `mode=off` on the workload or namespace, **or its namespace left the reaper's scope** (new) |

## Skip reasons

`ReapSkipped` notes start `reason=<SkipReason>`. The same string appears in
the plan annotation's `skip` field and in the `reason` label of
`reaper_workload_skipped`.

| Skip reason           | When                                                                  |
| --------------------- | --------------------------------------------------------------------- |
| `ReportOnlyScope`     | `mode=report` on the workload or namespace, or a needed Flux step is turned off |
| `CannotScaleToZero`   | a DaemonSet                                                           |
| `ManagedByController` | a controller `ownerReference` (e.g. a database operator)              |
| `ManagedByAutoscaler` | a KEDA ScaledObject targets the workload (an HPA does not count)      |
| `PartiallyAvailable`  | at least one replica is ready                                         |
| `RateLimited`         | this cycle's reap has been used by a longer-broken workload           |
| `ReapCeilingReached`  | `maxReaped` workloads are already held                                |

`OptedOut` and `UnsupportedOwner` are removed. No release produced them:

- an opted-out workload is never a candidate;
- crash-looping pods without a supported owner are counted in
  `reaper_unowned_crashlooping_pods`.
