# Contract: metrics

Exposed in Prometheus text format at `GET :8080/metrics`. The same instruments
are exported over OTLP when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (OTel names use
`.` in place of `_`, e.g. `reaper.actions`). Names and label sets are stable.

| Metric                                  | Type      | Labels                                   | Meaning                                                                                 |
| --------------------------------------- | --------- | ---------------------------------------- | --------------------------------------------------------------------------------------- |
| `reaper_workload_state`                 | gauge     | `namespace`, `kind`, `name`, `state`     | 1 for the workload's current state (`observed`, `candidate`, `reaping`, `reaped`); the series is removed when the state is cleared |
| `reaper_workload_skipped`               | gauge     | `namespace`, `kind`, `name`, `reason`    | 1 while a candidate is skipped for `reason`                                              |
| `reaper_workload_crashloop_seconds`     | gauge     | `namespace`, `kind`, `name`              | `now - crashloop-since` (`now - failing-since` for a CronJob)                           |
| `reaper_actions_total`                  | counter   | `action`, `result`, `mode`               | `action` ∈ `reap`, `restore`, `scale`, `kustomize_disable`, `kustomize_restore`, `helmrelease_hold`, `helmrelease_release`, `cronjob_suspend`, `cronjob_resume`, `record`; `result` ∈ `success`, `error`, `planned` (report mode). `record` is a failed write of the reaper's own labels or annotations; only `result="error"` is ever emitted for it |
| `reaper_cycles_total`                   | counter   | `result`                                 | `ok`, `error`                                                                           |
| `reaper_cycle_duration_seconds`         | histogram | none                                     | decide + execute time; unit `seconds` (`# UNIT`), 12 exponential buckets from 0.05 s (×2, up to 102.4 s) |
| `reaper_last_cycle_timestamp_seconds`   | gauge     | none                                     | Unix time the last cycle finished; the process start time until the first cycle finishes |
| `reaper_config_info`                    | gauge     | `mode`, `generation`                     | 1 for the active config                                                                 |
| `reaper_config_valid`                   | gauge     | none                                     | 1 if the latest ReaperConfig generation is valid, else 0; absent until the first config read |
| `reaper_notifications_total`            | counter   | `channel`, `provider`, `result`          | US6; see [notifications.md](./notifications.md)                                        |
| `reaper_notification_channels`          | gauge     | `channel`, `provider`, `valid`           | US6; 1 per channel read in the last cycle                                               |
| `reaper_unowned_crashlooping_pods`      | gauge     | `namespace`                              | Crash-looping pods with no supported owner (bare pods, unhandled owner kinds); the series is removed when the count drops to 0 |
| `reaper_build_info`                     | gauge     | `version`, `git_sha`                     | 1                                                                                       |

Cardinality: bounded by the number of workloads in a non-`none` state, a few
dozen on roanoke.

## Alert rules (shipped as a PrometheusRule by the chart)

| Alert                    | Expr                                                         | For | Severity | Labels on the alert                                    |
| ------------------------ | ------------------------------------------------------------ | --- | -------- | ------------------------------------------------------ |
| `ReaperCrashLoopPlanned` | `reaper_workload_state{state="candidate"} == 1`              | 5m  | warning  | `namespace`, `kind`, `name`, `state`                   |
| `ReaperWorkloadReaped`   | `reaper_workload_state{state="reaped"} == 1`                 | 0m  | warning  | `namespace`, `kind`, `name`, `state`                   |
| `ReaperWorkloadSkipped`  | `reaper_workload_skipped == 1`                               | 30m | info     | `namespace`, `kind`, `name`, `reason`                  |
| `ReaperActionFailed`     | `increase(reaper_actions_total{result="error"}[15m]) > 0`    | 0m  | critical | `action`, `result`, `mode`                             |
| `ReaperStalled`          | `time() - reaper_last_cycle_timestamp_seconds > 600`         | 5m  | critical | none of its own                                        |
| `ReaperConfigInvalid`    | `reaper_config_valid == 0`                                   | 5m  | warning  | none of its own                                        |
| `ReaperDown`             | `absent(up{job="<release fullname>"} == 1)`                  | 10m | critical | `job` (from `absent`)                                  |

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

Annotations link to `docs/guides/restoring-a-workload.md`. The existing
Alertmanager → Discord route on roanoke picks them up by severity, with no
new route.

The `action ∈ …` list above is read by `apps/k8s-reaper/tests/contracts.rs`,
which checks it in both directions against `ActionKind`. Keep it one
backticked value per entry, ending at `;`.
