# Contract delta: ReaperConfig

Applies to [001 contracts/config.md](../../001-crashloop-reaper/contracts/config.md)
and the generated CRD. Implementation edits that file in place; this delta
records what changes.

## `spec.cronJobs.failureRate` (new)

```yaml
spec:
  cronJobs:
    enabled: true
    failingDuration: 6h      # also the minimum history age for the rate rule
    minFailedRuns: 3
    failureRate:
      enabled: true          # default true
      windowRuns: 10         # default 10, allowed 3–100
      threshold: "80%"       # default 80%, allowed 51%–100%; "80" also accepted
```

| Field        | Type    | Default | Valid           | Meaning |
| ------------ | ------- | ------- | --------------- | ------- |
| `enabled`    | bool    | `true`  | n/a             | `false` turns the rule off cluster-wide and removes every run history on the next cycle; the streak rule is unaffected |
| `windowRuns` | integer | `10`    | 3–100           | Number of recorded runs judged. The rule waits until this many runs are recorded |
| `threshold`  | string  | `80%`   | `51%`–`100%`    | Share of failed runs in the window, inclusive, that makes a candidate |

A CronJob becomes a candidate under this rule when its window is full, the
failed share is at least `threshold`, the newest run failed, and its history
began at least `failingDuration` ago.

Out-of-range values make the ReaperConfig invalid: `Valid=False`, a
`ConfigInvalid` Event, and the last valid config stays in force.

The CRD schema enforces the same ranges (`minimum`/`maximum` on `windowRuns`,
a `pattern` on `threshold`). The chart's CRD in `mbround18/helm-charts`
must be regenerated with `k8s-reaper crd`.

**Upgrade note**: the rule is on by default. In `full` mode an upgrade can
pause CronJobs that the streak rule never would. Run in `report` mode first,
or set `failureRate.enabled: false`, to see the effect before it acts.
