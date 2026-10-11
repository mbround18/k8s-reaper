# CronJobs

This guide explains how k8s-reaper handles CronJobs: when a CronJob counts as
failing, how failed runs are counted, when it is paused, and what happens
after you resume it. Read it if you run scheduled jobs and want to know when
the reaper would pause one. CronJobs go through the same states, limits and
Flux holds as Deployments, described in [how it works](how-it-works.md) and
[Flux](flux.md). The settings are in
[configuration](configuration.md#cronjobs), and resuming is covered in
[restoring a workload](restoring-a-workload.md#4-resume-a-cronjob-by-hand).

## 1. Crash loops do not apply

A CronJob's pods belong to Jobs, and the reaper ignores Job pods when it looks
for crash loops. A Job stops retrying by itself when it reaches its
`backoffLimit`. What wastes resources is a CronJob that starts a new Job on
every schedule, and every one fails. So the reaper judges a CronJob by the
outcome of its Jobs: a run of failures with no success in between, called a
failing streak.

To pause it, the reaper sets `spec.suspend: true` on the CronJob, so no new
Jobs are started. It never deletes a Job and never stops one that is
running.

## 2. Settings

| Field | Default | Allowed | Meaning |
| ----- | ------- | ------- | ------- |
| `cronJobs.enabled` | `true` | | Watch CronJobs at all. |
| `cronJobs.failingDuration` | `6h` | `10m` to `168h` | How long the streak must have lasted. |
| `cronJobs.minFailedRuns` | `3` | `1` to `1000` | How many failed runs the streak must contain. |

A CronJob becomes a candidate only when **both** are met. The two together
handle any schedule: a CronJob that runs every minute reaches 3 failed runs
quickly but must still fail for 6 hours, and a daily one reaches 6 hours
after its second failure but must fail 3 times.

Durations use `h`, `m` and `s` only: write `72h`, not `3d`.

A team can raise either threshold for one CronJob, but never lower it:

```sh
kubectl annotate cronjob/ingest -n shop \
  reaper.boop.ninja/failing-duration=24h \
  reaper.boop.ninja/min-failed-runs=5
```

See [configuration](configuration.md#thresholds-per-workload-annotations).

With `enabled: false`, the reaper stops looking at CronJobs and silently
clears the state of any CronJob it was observing. A CronJob that is already
paused can still be restored.

## 3. How the streak is found

Each cycle, the reaper looks at the Jobs the CronJob owns:

- **Running Jobs are ignored.** Only finished Jobs count.
- **A Job has failed** when it has the condition `Failed=True`. The time it
  failed is that condition's `lastTransitionTime`.
- **A Job has succeeded** when it is complete. The time is its
  `completionTime`.

Then:

1. If no Job has finished yet, nothing changes.
2. If the most recent finished Job succeeded, the CronJob is healthy (see
   [section 5](#5-what-healthy-clears)).
3. Otherwise the CronJob is failing. The last success is the later of the
   newest successful Job still listed and the CronJob's
   `status.lastSuccessfulTime`, so a success whose Job was already cleaned up
   by `successfulJobsHistoryLimit` still counts. Only failures after the
   last success belong to the streak.

The streak starts when the first Job after the last success failed. The
reaper writes that time to `reaper.boop.ninja/failing-since` and keeps it
from then on.

## 4. Counting failed runs

The reaper writes the count to `reaper.boop.ninja/failed-runs` and the name
of the newest failed Job it has counted to
`reaper.boop.ninja/last-failed-job`. That name is a watermark: on the next
cycle, only failed Jobs that finished after it are added to the count. So a
Job is never counted twice, however many cycles see it.

Kubernetes deletes old Jobs as the CronJob's `failedJobsHistoryLimit` is
reached, oldest first. The recorded count is kept, so the streak can grow
beyond the number of Jobs Kubernetes keeps. If the watermark Job itself has
been deleted, every failed Job still listed is newer than it, and all of them
are counted.

A Job that is created and deleted between two cycles is never seen, so it is
not counted. With `failedJobsHistoryLimit: 0`, failed Jobs disappear before
the reaper can see them, and the CronJob is never paused. Keep it at 1 or
more (the Kubernetes default is 1).

To see where a CronJob stands:

```sh
kubectl get cronjob/ingest -n shop -o json \
  | jq '.metadata.annotations | with_entries(select(.key | startswith("reaper.boop.ninja/")))'
```

```json
{
  "reaper.boop.ninja/failed-runs": "2",
  "reaper.boop.ninja/failing-since": "2026-10-10T03:00:12Z",
  "reaper.boop.ninja/last-failed-job": "ingest-29334180"
}
```

## 5. What "healthy" clears

As soon as the most recent finished Job succeeds, the reaper clears its state
from the CronJob: the state label and `failing-since`, `failed-runs`,
`last-failed-job` and the plan. It records a `JobFailuresRecovered` Event.

There is no recovery window for CronJobs, unlike crash loops. One success is
enough, because it shows that the job can do its work.

A CronJob that succeeds some of the time is therefore never paused. Each
success ends the streak, and the next failure starts a new one at 0. To be
paused, a CronJob must fail every run for `failingDuration` and at least
`minFailedRuns` times.

## 6. The lifecycle

| State | For a CronJob |
| ----- | ------------- |
| `observed` | Failing, below at least one threshold. Event `JobFailuresObserved`. |
| `candidate` | Both thresholds met. A plan is published; in report mode the Event is `ReapPlanned`. |
| `reaping` | Full mode is pausing it. |
| `reaped` | `spec.suspend: true`. The `Reaped` Event's note says `suspended`. |

When a CronJob becomes a candidate, the reaper writes a reason such as:

```text
3 failed runs since 2026-10-10T03:00:12Z, last job "ingest-29334180" failed (BackoffLimitExceeded)
```

The plan for a CronJob lists `recordReap`, `suspendCronJob` and `markReaped`,
plus any Flux steps. There is no `original-replicas`.

Skip reasons work as for other workloads, with two exceptions:
`PartiallyAvailable` and `ManagedByAutoscaler` never apply to a CronJob. The
limits `maxReapsPerCycle` and `maxReaped` count paused CronJobs together with
reaped Deployments and StatefulSets. See
[how it works](how-it-works.md#5-skip-reasons).

A CronJob that someone else has suspended, or one with
`reaper.boop.ninja/mode=off`, is not tracked. Any state is cleared without an
Event.

## 7. Resuming and what happens next

Any of these resumes a paused CronJob:

- the restore annotation, which also sets `spec.suspend: false`:

  ```sh
  kubectl annotate cronjob/ingest -n shop reaper.boop.ninja/restore=now --overwrite
  ```

- `spec.suspend: false` set by hand, which counts as a restore (`Restored`
  note `manual-resume`):

  ```sh
  kubectl patch cronjob/ingest -n shop -p '{"spec":{"suspend":false}}'
  ```

- `reaper.boop.ninja/mode=off` on the CronJob or its namespace.

On restore the reaper releases its Flux holds, clears its state, and leaves
one annotation behind: `reaper.boop.ninja/last-failed-job`, set to the newest
failed Job. That watermark stops the failures from before the pause from
being counted again:

- Until a new Job finishes, the newest failure is the watermark itself, so the
  CronJob stays clear.
- If the next run succeeds, the CronJob is healthy and nothing changes. The
  watermark annotation stays; it only marks which failures have been
  counted.
- If the next run fails, a new streak starts at that failure, with the count
  at 1. The CronJob must again fail for `failingDuration` and
  `minFailedRuns` times before it is paused again.

So resuming a CronJob that is still broken does not get it paused at once on
the next cycle. It gets the same full allowance as a CronJob that has never
failed.

## 8. Flux-managed CronJobs

A CronJob applied by a Flux Kustomization or rendered by a HelmRelease gets
the same holds as a Deployment: the `kustomize.toolkit.fluxcd.io/reconcile:
disabled` annotation on the CronJob, a suspended HelmRelease, or both. Without
them, the next reconcile would apply `spec.suspend` from Git (usually
`false`) and the CronJob would run again. A restore releases them, as
described in [Flux](flux.md#4-how-a-restore-undoes-it).

If `flux.kustomize` or `flux.helmRelease` is off and the CronJob needs that
step, it is skipped with `ReportOnlyScope`, like any other workload.

## Next steps

- [Restoring a workload](restoring-a-workload.md): every way back.
- [Flux](flux.md): what the Flux holds change.
- [Monitoring](monitoring.md): metrics and alerts for paused CronJobs.
