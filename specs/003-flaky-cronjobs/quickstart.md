# Quickstart: validating flaky-CronJob detection

Each scenario proves one success criterion on the kind dev cluster.
**Always pass `KUBE_CONTEXT=kind-dev`.** The `default` context is the
production cluster, and the targets refuse anything that is not `kind-*` or
`k3d-*` on loopback. Expected values are defined in
[contracts/](./contracts/) and [data-model.md](./data-model.md).

## 0. Prerequisites

```sh
export KUBE_CONTEXT=kind-dev
make test                                   # unit, contract and contracts-drift tests
make image dev-load dev-run KUBE_CONTEXT=$KUBE_CONTEXT
kubectl --context $KUBE_CONTEXT -n k8s-reaper rollout status deploy/k8s-reaper
```

The dev ReaperConfig uses `cycleInterval: 15s`.

## 1. Core tests (US2, US3, US4): SC-002, SC-003, SC-004

```sh
cargo test -p reaper-core history     # update_history: restarts, re-listing, out of order, TTL, history limit, inference
cargo test -p reaper-core rate        # rate_met and threshold resolution
cargo test -p reaper-core cron        # every existing streak test, unchanged
```

- Expected: all of them pass.
- No existing test file under `crates/reaper-core/src` changes its
  assertions for streak-only CronJobs. A diff of those assertions should be
  empty (SC-004).

## 2. A mostly-failing CronJob is paused (US1): SC-001, SC-006

```sh
make test-integration STORY=us9_flaky_cronjob KUBE_CONTEXT=$KUBE_CONTEXT
```

The story does the following:

1. **Applies the config.** It applies a ReaperConfig with:
   - `mode: report`;
   - `cronJobs.failingDuration: 10m`;
   - `cronJobs.failureRate: {windowRuns: 5, threshold: "80%"}`.
2. **Creates the CronJob.** It creates `flaky` in namespace `cron` (the dev CronJob namespace):
   - schedule `* * * * *`;
   - `failedJobsHistoryLimit: 1`, `successfulJobsHistoryLimit: 1`;
   - `ttlSecondsAfterFinished: 30`.

   The Job fails unless the scheduled minute is divisible by 5. That is a
   fixed pattern of 4 failures in 5 runs, with no randomness.
3. **Checks recording.** After the first failure, `flaky` is `observed` and
   `run-history` holds one run. After each later minute it holds one more
   run, never a duplicate. Successes deleted by the TTL show as `job: null`.
4. **Checks the rate in report mode.** At least 10 minutes after the
   history began, the window holds 5 runs, 4 of them failed, and the newest
   one failed. Then:
   - `flaky` is `candidate`;
   - `reason` reads `4 of last 5 runs failed, last job "flaky-…" failed (BackoffLimitExceeded)`;
   - a `ReapPlanned` Event carries that reason;
   - `reaper_cronjob_rule{name="flaky",rule="failure_rate"} 1` is set;
   - `spec.suspend` is still `false`.
5. **Checks full mode.** It switches to `mode: full`. `flaky` is suspended
   and the `Reaped` Event note names `4 of last 5 runs failed` (SC-006).
6. **Checks the restore** (US4). It annotates `reaper.boop.ninja/restore=1`.
   - The CronJob resumes, and `run-history`, `failing-since` and
     `failed-runs` are gone.
   - The next failure starts a one-run history, and the CronJob is not a
     candidate.

## 3. A healthy CronJob carries nothing (SC-005)

In the same story, `flaky` is patched so that every run succeeds. Within
`windowRuns` runs:

- the history holds no failures;
- `run-history` and the `state` label are removed;
- `JobFailuresRecovered` is emitted once;
- the `reaper_cronjob_window_runs` series for `flaky` are gone.

## 4. Per-CronJob overrides (US3)

| Annotation on `flaky`                 | Expected                                                                 |
| ------------------------------------- | ------------------------------------------------------------------------ |
| `reaper.boop.ninja/failure-window=8`  | Not a candidate until 8 runs are recorded                                |
| `reaper.boop.ninja/failure-window=3`  | No effect: the window stays 5 (the cluster value)                        |
| `reaper.boop.ninja/failure-window=x`  | A `ConfigInvalid` Event on the CronJob, and the window stays 5           |
| `reaper.boop.ninja/failure-rate=off`  | `run-history` is removed on the next cycle, and only the streak rule applies |

## 5. Clean up

```sh
kubectl --context $KUBE_CONTEXT -n cron delete cronjob flaky
```

The story removes its ReaperConfig changes itself.
