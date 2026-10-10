# Contract: Kubernetes Events

API `events.k8s.io/v1`. `reportingController: reaper.boop.ninja/k8s-reaper`.
`reportingInstance` is the pod name. Events are emitted on **transitions** only
(and on a change of skip reason), never every cycle.

| Reason               | Type    | Regarding                | Action field | When                                                                                       |
| -------------------- | ------- | ------------------------ | ------------ | ------------------------------------------------------------------------------------------ |
| `CrashLoopObserved`  | Normal  | workload                 | `Observe`    | none → `observed`                                                                          |
| `ReapPlanned`        | Warning | workload                 | `Plan`       | → `candidate`, effective mode `report`. Note lists the actions full mode would take         |
| `ReapSkipped`        | Warning | workload                 | `Plan`       | candidate with a skip reason, or a new skip reason. Note: `reason=<SkipReason>; would: …`   |
| `Reaped`             | Warning | workload                 | `ScaleDown`  | → `reaped`. Note: original replicas (or `suspended` for a CronJob), reason, Flux holds placed |
| `ReapFailed`         | Warning | workload                 | `ScaleDown`  | a reap step failed (API error, conflict). Retried next cycle                                |
| `Restored`           | Normal  | workload                 | `Restore`    | restore finished. Note: `requested`, `manual-scale-up` or `manual-resume`, replicas, Flux holds released     |
| `RestoreFailed`      | Warning | workload                 | `Restore`    | a restore step failed. Retried next cycle                                                   |
| `RestoreIgnored`     | Normal  | workload                 | `Restore`    | `restore` annotation on a workload the reaper holds no reap for                             |
| `CrashLoopRecovered` | Normal  | workload                 | `Recover`    | observed/candidate → none after `recoveryWindow` healthy                                    |
| `JobFailuresObserved`| Normal  | CronJob                  | `Observe`    | none → `observed`: the latest finished Job failed. Note: failing since, last failed Job     |
| `JobFailuresRecovered`| Normal | CronJob                  | `Recover`    | observed/candidate → none: a Job succeeded                                                  |
| `HelmReleaseHeld`    | Warning | HelmRelease              | `Suspend`    | first workload added to `held-for` (Note warns that the whole release is frozen)            |
| `HelmReleaseReleased`| Normal  | HelmRelease              | `Resume`     | `held-for` became empty                                                                     |
| `ConfigInvalid`      | Warning | ReaperConfig (or object) | `Validate`   | config, or a mode label value, failed validation; last valid config is kept                |
| `ConfigApplied`      | Normal  | ReaperConfig             | `Validate`   | a new config generation became active                                                      |

Note text is at most 1 KB, plain English, and always names the container,
restart count and crash-loop start for workload Events. For CronJobs it names
the failed run count, the failing-since time and the last failed Job.

Alertmanager does not consume Events directly. Alerting is metric-based (see
metrics.md). Events are for `kubectl describe`/`kubectl events` and for any
event exporter.
