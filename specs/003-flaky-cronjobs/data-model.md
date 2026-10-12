# Data Model: Flaky CronJobs

All types below live in `crates/reaper-core` and are plain data (constitution
I). Times are `Timestamp`, as elsewhere in the core.

## RunHistory (new, `record.rs`)

| Field   | Type              | Rule                                                                                |
| ------- | ----------------- | ----------------------------------------------------------------------------------- |
| `since` | `Timestamp`       | The finish time of the first recorded failure. It never moves while the history exists |
| `runs`  | `Vec<RecordedRun>` | Oldest first. `len() <= window_runs`; older runs are dropped on write              |

Invariants:

- `runs` is non-empty, and at least one run has `ok == false`. Otherwise the
  history is `None` (FR-011).
- `runs` is strictly increasing by `(at, job)`, where `job: None` compares
  equal to any name at the same `at` (research R2 and R4).

Encoding: the JSON value of `reaper.boop.ninja/run-history`. See
[contracts/annotations.md](./contracts/annotations.md).

- A decode error yields `None` plus a `DecodeError`, which becomes an
  `InvalidSetting` and so a `ConfigInvalid` Event (FR-014).
- An unknown JSON field is an error, so a hand edit is caught rather than
  half-read.

## RecordedRun (new)

| Field | Type             | Rule                                                          |
| ----- | ---------------- | ------------------------------------------------------------- |
| `job` | `Option<String>` | The Job name. `None` only for a success inferred from `lastSuccessfulTime` |
| `at`  | `Timestamp`      | The Job's finish time (`Succeeded{at}` or `Failed{at}`)        |
| `ok`  | `bool`           | `true` if the Job succeeded                                   |

## WorkloadRecord (changed)

- Gains `run_history: Option<RunHistory>`.
- `encode()` writes the key only when it is `Some`.
- `decode()` reads it on any kind; only CronJobs ever have it.
- `run-history` is added to the owned keys, so `is_owned_key` holds for it
  and the existing "remove owned keys not in desired" logic deletes it when
  the history is cleared.

## FailureRate settings (new, `config.rs`)

`CronJobs` gains `failure_rate: FailureRate`:

| Field         | Type   | Default | Range   | Raw config field                          |
| ------------- | ------ | ------- | ------- | ----------------------------------------- |
| `enabled`     | `bool` | `true`  | n/a     | `cron_jobs_failure_rate_enabled`          |
| `window_runs` | `u32`  | `10`    | 3–100   | `cron_jobs_failure_rate_window_runs`      |
| `threshold`   | `u8` (percent) | `80` | 51–100 | `cron_jobs_failure_rate_threshold` (string `"80%"` or `"80"`) |

Out-of-range values make the whole config invalid (FR-009). This is the
existing `validate()` path.

## Rate thresholds (changed, `decide.rs`)

`Thresholds` gains these fields:

- `failure_rate_enabled: bool`. It is false when the global setting is false
  or the CronJob has `failure-rate: off`.
- `window_runs: u32`, resolved as `max(global, failure-window)`.
- `threshold: u8`, resolved as `max(global, failure-threshold)`.

Out-of-range or unparseable values become `InvalidSetting`s, as today.

## History update (new pure function, `detect.rs`)

```text
update_history(prev: Option<RunHistory>, cron: &CronJobView, streak: &JobStreak,
               window_runs: u32) -> Option<RunHistory>
```

1. **No previous history.** If `prev` is `None`:
   - When `streak` is `Failing`, seed the history from the streak's failed
     Jobs that are still listed, oldest first, plus any later finished Jobs.
     `since` is the first one's finish time. (In practice there are none
     later, since a streak ends on a success.) This is research R3.
   - Otherwise return `None`.
2. **Previous history.** Append every finished listed Job whose `(at, name)`
   sorts after the last run, in sorted order. Running Jobs are skipped.
3. **Inferred success.** If `cron.last_successful_time` sorts after the last
   run and no appended run is a success at that time, append
   `{job: None, at: lst, ok: true}` (research R4).
4. **Trim.** Keep only the newest `window_runs` runs.
5. **Clear.** If no run has `ok == false`, return `None`.

The function is total and deterministic. Unit tests drive it with sequences
of `CronJobView`s for US2.

## Rate evaluation (new, `detect.rs` or `decide.rs`)

```text
rate_met(h: &RunHistory, t: &Thresholds, now) -> RateEvidence
  failed = count(!ok); window = runs.len()
  met = enabled
        && window == t.window_runs
        && failed * 100 >= t.threshold * window
        && now - h.since >= t.failing_duration
        && !runs.last().ok
```

`RateEvidence { failed: u32, window: u32, met: bool }` is attached to
`Evidence::JobFailures { …, rate: Option<RateEvidence> }`.

## CronJob state transitions (unchanged states, wider triggers)

| From                    | Trigger                                              | To                   | Plan / Event                       |
| ----------------------- | ---------------------------------------------------- | -------------------- | ---------------------------------- |
| none                    | the first streak failure is recorded                  | `observed`           | `Observe` / `JobFailuresObserved`  |
| `observed`              | the streak rule **or** the rate rule is met           | `candidate`          | the existing candidate flow         |
| `observed`/`candidate`  | the streak recovers, and the window still holds a failure | `observed`       | `Observe` (no Event; the state is unchanged or drops a level) |
| `observed`/`candidate`  | the streak is passing and the history is cleared      | none                 | `Recover` / `JobFailuresRecovered` |
| `reaped`                | any restore                                           | none                 | the history is cleared (FR-012)     |
| any tracked             | mode off, CronJobs disabled, suspended by someone else, or the rule off with no streak | none | `Forget` (FR-013) |

A `candidate` whose newest run then succeeds is no longer met, so it returns
to `observed` (Edge Cases). The existing candidate → observed path already
handles this for limits and skips.
