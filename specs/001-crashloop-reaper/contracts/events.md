# Contract: Kubernetes Events

API `events.k8s.io/v1`. `reportingController: reaper.boop.ninja/k8s-reaper`.
`reportingInstance` is the pod name. Events are emitted on **transitions** only
(and on a change of skip reason), never every cycle.

| Reason               | Type    | Regarding                | Action field | When                                                                                       |
| -------------------- | ------- | ------------------------ | ------------ | ------------------------------------------------------------------------------------------ |
| `CrashLoopObserved`  | Normal  | workload                 | `Observe`    | none → `observed`                                                                          |
| `ReapPlanned`        | Warning | workload                 | `Plan`       | → `candidate`, effective mode `report`. Note lists the actions full mode would take         |
| `ReapSkipped`        | Warning | workload                 | `Plan`       | candidate with a skip reason, or a new skip reason. Note: `reason=<SkipReason>; would: …` (see [Skip reasons](#skip-reasons)) |
| `Reaped`             | Warning | workload                 | `ScaleDown`  | → `reaped`. Note: original replicas (or `suspended` for a CronJob), reason, Flux holds placed (for a CronJob it names the rule met, see [annotations.md](./annotations.md)) |
| `ReapFailed`         | Warning | workload                 | `ScaleDown`  | a reap step failed (API error, conflict). Retried next cycle                                |
| `Restored`           | Normal  | workload                 | `Restore`    | restore finished. Note: a restore note (see [Restore notes](#restore-notes)), replicas, Flux holds released |
| `RestoreFailed`      | Warning | workload                 | `Restore`    | a restore step failed. Retried next cycle                                                   |
| `RestoreIgnored`     | Normal  | workload                 | `Restore`    | `restore` annotation on a workload the reaper holds no reap for                             |
| `RecordFailed`       | Warning | workload                 | `Record`     | writing the reaper's own state label, record or plan annotation failed (not a conflict or not-found). Retried next cycle; counted as `reaper_actions_total{action="record",result="error"}` |
| `CrashLoopRecovered` | Normal  | workload                 | `Recover`    | observed/candidate → none after `recoveryWindow` healthy                                    |
| `JobFailuresObserved`| Normal  | CronJob                  | `Observe`    | none → `observed`: the latest finished Job failed. Note: failing since, last failed Job; when a run history exists it ends with `; window: <failed> of <runs> failed` |
| `JobFailuresRecovered`| Normal | CronJob                  | `Recover`    | observed/candidate → none: a Job succeeded **and** the run history holds no failures. A success that ends the streak while the window still holds failures emits nothing |
| `HelmReleaseHeld`    | Warning | HelmRelease              | `Suspend`    | first workload added to `held-for` (Note warns that the whole release is frozen)            |
| `HelmReleaseReleased`| Normal  | HelmRelease              | `Resume`     | `held-for` became empty                                                                     |
| `ConfigInvalid`      | Warning | ReaperConfig (or object) | `Validate`   | config, or a mode label value, failed validation; last valid config is kept. On a CronJob also for an unusable `failure-window`, `failure-threshold` or `failure-rate`, or an unreadable `run-history` |
| `ConfigApplied`      | Normal  | ReaperConfig             | `Validate`   | a new config generation became active                                                      |

## Restore notes

The note of a `Restored` Event starts with one of these values. The same
string is the `RestoreKind`.

| Note              | When                                                                                   |
| ----------------- | -------------------------------------------------------------------------------------- |
| `requested`       | `reaper.boop.ninja/restore=true` on the workload                                       |
| `manual-scale-up` | someone scaled a reaped Deployment or StatefulSet above zero                           |
| `manual-resume`   | someone resumed a reaped CronJob, or the HelmRelease the reaper suspended for it       |
| `opted-out`       | `mode=off` on the workload or namespace, or its namespace left the reaper's scope      |

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

`OptedOut` and `UnsupportedOwner` were removed in 002. No release produced them:

- an opted-out workload is never a candidate;
- crash-looping pods without a supported owner are counted in
  `reaper_unowned_crashlooping_pods`.

The "Restore notes" and "Skip reasons" tables are read by
`apps/k8s-reaper/tests/contracts.rs`, which checks them in both directions
against `RestoreKind` and `SkipReason`. Keep their first column as plain
backticked strings.

## Series, note text and alerting

A repeat of the same object, reason, action, type and note within 6 minutes
continues that Event's `series`; a different note is always a new Event, so the
latest Event for a reason never carries an older note (G18).

Note text is at most 1 KB, plain English, and always names the container,
restart count and crash-loop start for workload Events. For CronJobs it names
the failed run count, the failing-since time and the last failed Job.

Alertmanager does not consume Events directly. Alerting is metric-based (see
metrics.md). Events are for `kubectl describe`/`kubectl events` and for any
event exporter.
