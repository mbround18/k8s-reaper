# Data model: Post-rollout hardening

This file covers only what changes; everything else is in
[001's data-model.md](../001-crashloop-reaper/data-model.md). Each change
points to its decision in [research.md](./research.md).

## Health (reaper-observability `http.rs`), R1–R3

| Field             | Type                   | Set by                                         | Initial                      |
| ----------------- | ---------------------- | ---------------------------------------------- | ---------------------------- |
| `cycle_started`   | instant (atomic)       | the top of each loop iteration                 | process start                |
| `cycle_finished`  | instant (existing `stale_at` basis) | the end of each cycle             | none                         |
| `config_applied`  | bool (atomic, latches) | `refresh_config` when a valid config, or the built-in defaults, is in force | `false` |
| `caches_synced`   | bool (atomic)          | after the reflectors' initial sync             | `false`                      |
| `alive`           | bool (existing)        | **removed**: never set; derived instead        | none                         |

Derived values, with `I = cycleInterval`:

```text
live(now)  = now - cycle_started  <  max(10·I, 10m)
ready(now) = caches_synced ∧ config_applied ∧ cycle_finished ≠ none
             ∧ now - cycle_finished < 3·I
readiness(now) =
    CachesNotSynced  if ¬caches_synced ∨ cycle_finished = none
    Stalled          if now - cycle_finished ≥ 3·I
    CycleCompleted   otherwise
```

Rules:

- `now` is a parameter, so tests pass an `Instant` (Constitution III).
- `config_applied` never goes back to `false` (owner decision Q1).
- Before the first cycle, `/readyz` is 503 and `Ready` is
  `False/CachesNotSynced`, even if the caches are synced. The reason means
  "nothing has completed yet".
- `10·I ≥ 3·I` always holds, so readiness always fails before liveness
  (FR-002).

## Readiness (reaper-kube `config_watch.rs`), R3

`status(...)` takes `Readiness` in place of `ready: bool`:

| `Readiness`       | `Ready` status | reason            |
| ----------------- | -------------- | ----------------- |
| `CycleCompleted`  | `True`         | `CycleCompleted`  |
| `Stalled`         | `False`        | `Stalled`         |
| `CachesNotSynced` | `False`        | `CachesNotSynced` |

Two writers:

- the cycle loop writes the whole status each cycle, as today;
- the status task writes only the `Ready` condition, and only when its
  reason changes.

## ActionKind (reaper-core `ports.rs`), R4

```text
ActionKind = Reap | Restore | Scale | KustomizeDisable | KustomizeRestore
           | HelmReleaseHold | HelmReleaseRelease | CronJobSuspend | CronJobResume
           | Record            # new: the reaper's own labels/annotations
ActionKind::ALL: [ActionKind; 10]
```

- `Record` is counted **only on failure** (`result="error"`), and only for
  `WriteError::Other`.
- `Conflict` and `NotFound` are retried next cycle without being counted, as
  for every other kind.

## Transition (reaper-core `plan.rs`), R4

`Transition` gains `RecordFailed`, a Warning Event with action field
`Record`. Its note is the error text, truncated to the 1 KB note limit. The
workload's state does not change, because the record did not land, and the
next cycle retries the same plan.

## SkipReason, R9

```text
SkipReason = ReportOnlyScope | CannotScaleToZero | ManagedByController
           | ManagedByAutoscaler | PartiallyAvailable | RateLimited
           | ReapCeilingReached
SkipReason::ALL: [SkipReason; 7]
```

`OptedOut` and `UnsupportedOwner` are removed. They were never produced.

## RestoreKind and when each applies (reaper-core `decide.rs::held_plan`), R7–R8

`RestoreKind` keeps four variants: `requested`, `manual-scale-up`,
`manual-resume` and `opted-out`. `RestoreKind::ALL` is added.

`held_plan` checks these in order for a workload with a `reaping` or `reaped`
record. The first match wins.

| # | Condition                                                                                          | Kind              | New |
| - | -------------------------------------------------------------------------------------------------- | ----------------- | --- |
| 1 | namespace out of scope (`excludeNamespaces` match, or `includeNamespaces` set and no match)          | `opted-out`       | ✔ |
| 2 | effective label mode `off` (namespace or workload)                                                  | `opted-out`       |   |
| 3 | `reaper.boop.ninja/restore=true`                                                                    | `requested`       |   |
| 4 | record `reaped`; its HelmRelease hold has `suspended_by_reaper = true`; that HelmRelease's `spec.suspend` is now `false` | `manual-resume` | ✔ |
| 5 | record `reaped`; a CronJob that is no longer suspended                                              | `manual-resume`   |   |
| 6 | record `reaped`; replicas > 0                                                                       | `manual-scale-up` |   |

Other rules:

- **Scope.** The reaper's own namespace is dropped before this table. A
  workload out of scope with no `reaping` or `reaped` record is dropped as
  today, so no Event is emitted and no label is written.
- **Global `mode: off`.** Nothing is executed (executor gate), including
  these restores.
- **Restore actions for #4.** The release is already running, so the
  restore drops this workload from `held-for` and does not resume it. When
  `held-for` becomes empty, `HelmReleaseReleased` is emitted as today.
  Every workload held through the release matches #4 in the same cycle.
- **Rate limits.** Restores are not rate-limited and do not count toward the
  reap ceiling.

## Metrics state at start-up (reaper-observability `telemetry.rs`), R6

| Series                                | Before the first cycle / config read | After                   |
| ------------------------------------- | ------------------------------------ | ----------------------- |
| `reaper_last_cycle_timestamp_seconds` | process start time (was `0`)         | last cycle finish       |
| `reaper_config_valid`                 | absent (was `0`)                     | `1` or `0`, as today    |
