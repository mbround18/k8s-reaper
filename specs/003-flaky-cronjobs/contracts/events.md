# Contract delta: Events

Applies to [001 contracts/events.md](../../001-crashloop-reaper/contracts/events.md).
Implementation edits that file in place; this delta records what changes.

No Event reason is added or removed.

| Reason                 | Change |
| ---------------------- | ------ |
| `ReapPlanned`, `ReapSkipped`, `Reaped` | The note's reason for a CronJob names the rule: `8 of last 10 runs failed, …` for the rate rule, the existing streak text for the streak rule, both joined by `; ` when both are met (see [annotations.md](./annotations.md)) |
| `JobFailuresObserved`  | Trigger unchanged (a history only begins with a failing streak, research R3). When a history exists the note ends with `; window: <failed> of <runs> failed` |
| `JobFailuresRecovered` | Emitted when the CronJob leaves `observed`/`candidate` for none: its streak is passing **and** its window holds no failures. A success that ends the streak while the window still holds failures emits nothing; the CronJob stays `observed` |
| `ConfigInvalid`        | Also emitted on a CronJob for an unparseable or out-of-range `failure-window`, `failure-threshold` or `failure-rate`, and for an unreadable `run-history` |

Events are still emitted only on change, so recording a run in a history that
already exists emits no Event.
