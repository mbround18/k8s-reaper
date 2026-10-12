# Contract delta: annotations

Applies to [001 contracts/annotations.md](../../001-crashloop-reaper/contracts/annotations.md).
Implementation edits that file in place; this delta records what changes.

## Person-set keys (added to the main table)

| Key                                    | Set by | When               | Value                                                                                      |
| -------------------------------------- | ------ | ------------------ | ------------------------------------------------------------------------------------------ |
| `reaper.boop.ninja/failure-window`     | person | any time (CronJob) | Integer 3–100. Raises `cronJobs.failureRate.windowRuns` for this CronJob; a lower value has no effect |
| `reaper.boop.ninja/failure-threshold`  | person | any time (CronJob) | Percent 51–100, written `85` or `85%`. Raises `cronJobs.failureRate.threshold` for this CronJob; a lower value has no effect |
| `reaper.boop.ninja/failure-rate`       | person | any time (CronJob) | `off`: the failure-rate rule does not apply to this CronJob, and its run history is removed. Any other value is reported |

A value that cannot be parsed or is out of range is ignored and reported with
`ConfigInvalid` on the CronJob, as for the existing threshold keys. The reaper
never writes these keys.

## CronJob-only keys (added to the CronJob table)

| Key                              | Set by | When                                              | Value |
| -------------------------------- | ------ | ------------------------------------------------- | ----- |
| `reaper.boop.ninja/run-history`  | reaper | from the first recorded failure until the window holds no failures, a restore, or the rule is turned off | JSON, see below |

```json
{"since":"2026-10-12T02:00:53Z",
 "runs":[{"job":"backup-29340000","at":"2026-10-12T02:00:53Z","ok":false},
         {"job":null,"at":"2026-10-13T02:00:40Z","ok":true}]}
```

- `since`: RFC 3339 UTC. When the history began; it is the finish time of
  its first recorded failure.
- `runs`: oldest first, at most the effective window size. `at` is the Job's
  finish time. `job` is `null` for a success that was inferred from the
  CronJob's `status.lastSuccessfulTime` because the Job was deleted before
  the reaper saw it.
- Unknown fields or invalid JSON make the value unreadable. The reaper then
  treats the CronJob as having no history, reports `ConfigInvalid` naming
  the key, and replaces the value on its next write.
- The value is written as compact JSON (no spaces); it is shown wrapped
  above.

Read the counts:

```sh
kubectl get cronjob NAME -n NS -o json \
  | jq -r '.metadata.annotations["reaper.boop.ninja/run-history"] | fromjson
           | "\([.runs[] | select(.ok|not)] | length) of \(.runs | length) failed since \(.since)"'
```

## `reason` on a CronJob (replaces the example)

| Rule met    | Example |
| ----------- | ------- |
| streak only | `7 failed runs since 2026-10-10T02:00:00Z, last job "ingest-29340000" failed (BackoffLimitExceeded)` (unchanged) |
| rate only   | `8 of last 10 runs failed, last job "ingest-29340000" failed (BackoffLimitExceeded)` |
| both        | `8 of last 10 runs failed; 7 failed runs since 2026-10-10T02:00:00Z, last job "ingest-29340000" failed (BackoffLimitExceeded)` |
