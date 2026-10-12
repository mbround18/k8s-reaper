# Contract delta: metrics

Applies to [001 contracts/metrics.md](../../001-crashloop-reaper/contracts/metrics.md).
Implementation edits that file in place; this delta records what changes.

No existing metric changes name, type or label set.

## Added

| Name                         | Type  | Labels                            | Meaning |
| ---------------------------- | ----- | --------------------------------- | ------- |
| `reaper_cronjob_window_runs` | gauge | `namespace`, `name`, `outcome`    | Recorded runs in the CronJob's window, by `outcome` ∈ `failed`, `succeeded`. Present while the CronJob has a run history; both series are removed when the history is removed |
| `reaper_cronjob_rule`        | gauge | `namespace`, `name`, `rule`       | 1 while the CronJob is `candidate` and `rule` ∈ `streak`, `failure_rate` is met this cycle. Two series when both are met; removed when it stops being a candidate. Once `reaped`, the rule is named by the `reason` annotation and the `Reaped` Event, not by this gauge |

The failure ratio is computed in PromQL rather than exported:

```promql
reaper_cronjob_window_runs{outcome="failed"}
  / ignoring(outcome) sum without(outcome) (reaper_cronjob_window_runs)
```

## Changed meaning

| Name                                | Change |
| ----------------------------------- | ------ |
| `reaper_workload_crashloop_seconds` | For a CronJob: `now -` the earlier of `failing-since` and the run history's `since`. Unchanged for a CronJob with only a streak |
