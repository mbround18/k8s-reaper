# Contract: metrics

Exposed in Prometheus text format at `GET :8080/metrics`. The same instruments
are exported over OTLP when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (OTel names use
`.` in place of `_`, e.g. `reaper.actions`). Names and label sets are stable.

| Metric                                  | Type      | Labels                                   | Meaning                                                                                 |
| --------------------------------------- | --------- | ---------------------------------------- | --------------------------------------------------------------------------------------- |
| `reaper_workload_state`                 | gauge     | `namespace`, `kind`, `name`, `state`     | 1 for the workload's current state (`observed`, `candidate`, `reaping`, `reaped`); the series is removed when the state is cleared |
| `reaper_workload_skipped`               | gauge     | `namespace`, `kind`, `name`, `reason`    | 1 while a candidate is skipped for `reason`                                              |
| `reaper_workload_crashloop_seconds`     | gauge     | `namespace`, `kind`, `name`              | `now - crashloop-since` (`now - failing-since` for a CronJob)                           |
| `reaper_actions_total`                  | counter   | `action`, `result`, `mode`               | `action` ∈ `reap`, `restore`, `scale`, `kustomize_disable`, `kustomize_restore`, `helmrelease_hold`, `helmrelease_release`, `cronjob_suspend`, `cronjob_resume`; `result` ∈ `success`, `error`, `planned` (report mode) |
| `reaper_cycles_total`                   | counter   | `result`                                 | `ok`, `error`                                                                           |
| `reaper_cycle_duration_seconds`         | histogram | none                                     | decide + execute time                                                                   |
| `reaper_last_cycle_timestamp_seconds`   | gauge     | none                                     | Unix time the last cycle finished                                                       |
| `reaper_config_info`                    | gauge     | `mode`, `generation`                     | 1 for the active config                                                                 |
| `reaper_config_valid`                   | gauge     | none                                     | 1 if the latest ReaperConfig generation is valid, else 0                                |
| `reaper_build_info`                     | gauge     | `version`, `git_sha`                     | 1                                                                                       |

Cardinality: bounded by the number of workloads in a non-`none` state, a few
dozen on roanoke.

## Alert rules (shipped as a PrometheusRule by the chart)

| Alert                   | Expr                                                                   | For  | Severity |
| ----------------------- | ---------------------------------------------------------------------- | ---- | -------- |
| `ReaperCrashLoopPlanned`| `reaper_workload_state{state="candidate"} == 1`                        | 5m   | warning  |
| `ReaperWorkloadReaped`  | `reaper_workload_state{state="reaped"} == 1`                           | 0m   | warning  |
| `ReaperWorkloadSkipped` | `reaper_workload_skipped == 1`                                         | 30m  | info     |
| `ReaperActionFailed`    | `increase(reaper_actions_total{result="error"}[15m]) > 0`              | 0m   | critical |
| `ReaperStalled`         | `time() - reaper_last_cycle_timestamp_seconds > 600`                   | 5m   | critical |
| `ReaperConfigInvalid`   | `reaper_config_valid == 0`                                             | 5m   | warning  |
| `ReaperDown`            | `absent(up{job="k8s-reaper"} == 1)`                                    | 10m  | critical |

The alert labels carry `namespace`, `kind` and `name`. Annotations link to
`docs/guides/restoring-a-workload.md`. The existing Alertmanager → Discord
route on roanoke picks them up by severity, with no new route.
