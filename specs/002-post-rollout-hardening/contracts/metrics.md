# Contract delta: metrics and alerts

Applies to [001 contracts/metrics.md](../../001-crashloop-reaper/contracts/metrics.md).

## Metrics

| Metric                                | Change                                                                                       |
| ------------------------------------- | -------------------------------------------------------------------------------------------- |
| `reaper_actions_total`                | `action` gains `record`: a failed write of the reaper's own labels or annotations. Only `result="error"` is ever emitted for `record` |
| `reaper_last_cycle_timestamp_seconds` | Until the first cycle finishes it reads the **process start time** (was `0`)                 |
| `reaper_config_valid`                 | **Absent** until the first config read (was `0`). Then `1`/`0` as before                     |

The metric names and label sets are otherwise unchanged.

## Alert rules

The expressions are unchanged except for `ReaperDown`'s job, which now matches
the chart.

| Alert                    | Expr                                                         | For | Severity | Labels on the alert                                    |
| ------------------------ | ------------------------------------------------------------ | --- | -------- | ------------------------------------------------------ |
| `ReaperCrashLoopPlanned` | `reaper_workload_state{state="candidate"} == 1`              | 5m  | warning  | `namespace`, `kind`, `name`, `state`                   |
| `ReaperWorkloadReaped`   | `reaper_workload_state{state="reaped"} == 1`                 | 0m  | warning  | `namespace`, `kind`, `name`, `state`                   |
| `ReaperWorkloadSkipped`  | `reaper_workload_skipped == 1`                               | 30m | info     | `namespace`, `kind`, `name`, `reason`                  |
| `ReaperActionFailed`     | `increase(reaper_actions_total{result="error"}[15m]) > 0`    | 0m  | critical | `action`, `result`, `mode`                             |
| `ReaperStalled`          | `time() - reaper_last_cycle_timestamp_seconds > 600`         | 5m  | critical | none of its own                                        |
| `ReaperConfigInvalid`    | `reaper_config_valid == 0`                                   | 5m  | warning  | none of its own                                        |
| `ReaperDown`             | `absent(up{job="<release fullname>"} == 1)`                   | 10m | critical | `job` (from `absent`)                                  |

Notes:

- Every alert also carries `severity` and the scrape target labels (`job`,
  `instance`, `pod`, `service`). The chart's ServiceMonitor sets
  `honorLabels: true`, so a metric's `namespace` label is the workload's,
  not the reaper's.
- `<release fullname>` is `{{ include "k8s-reaper.fullname" . }}`. It is
  the Service name, which the ServiceMonitor uses as the scrape job, so it
  works for any release name.
- The chart installs `ReaperDown` only when `serviceMonitor.enabled` is
  true. Without the ServiceMonitor there is no `up` series, and the rule
  would always fire.
- `ReaperStalled` cannot fire during the first 15 minutes after a start
  (600 s plus 5 m), and `ReaperConfigInvalid` cannot fire before the first
  config read.
- The sentence "The alert labels carry `namespace`, `kind` and `name`" in
  001 is replaced by the last column of this table.
