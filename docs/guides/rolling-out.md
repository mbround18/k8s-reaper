# Rolling out to production

This is a playbook for taking k8s-reaper from report mode to full mode on a
production cluster. You install it in report mode, which changes nothing but
its own labels, annotations and Events, read every plan it publishes, keep out
what it must never touch, and then switch to full mode with the default limits,
which reap one workload a minute and stop at five. The worked example is a real
rollout on a small production cluster: what showed up in the first cycle, why
most workloads took 30 minutes to become candidates, and what happened in the
first minutes of full mode.

If you have not installed the reaper before, start with
[getting-started.md](getting-started.md). For the chart itself, see
[helm-chart.md](helm-chart.md).

## The example at a glance

| When                  | What happened |
| --------------------- | ------------- |
| T+0                   | Installed with the chart in report mode, all thresholds and limits at their defaults. |
| First cycle           | A CronJob whose runs had all failed for about 23 hours became a `candidate` straight away. Seven Deployments that had been crash-looping for a long time, with hundreds to thousands of restarts each, became `observed` only. Pods in `ImagePullBackOff`, `Pending` pods and failed Job pods got no label. |
| T+30m                 | The seven Deployments became candidates. |
| Report mode, overall  | The reaper used about 1m CPU and 58Mi memory. Every cycle took under 0.2 s. |
| Switch                | `reaperConfig.spec.mode: full` committed to the HelmRelease values in Git. |
| About 20 s later      | Next cycle: the CronJob was suspended. |
| Following minutes     | Deployments reaped one per cycle (one a minute), until five workloads were reaped. The rest waited with `ReapCeilingReached`. |

The rest of this guide explains each step and what to check.

## 1. Before you install

Work through this list first. Each item is cheap in report mode and expensive
to discover in full mode.

- [ ] **Decide what is out of scope.** The reaper skips `kube-system`,
  `kube-public`, `kube-node-lease`, `flux-system` and its own namespace.
  To exclude more, set `scope.excludeNamespaces` and repeat the four
  defaults, because setting the list replaces them. To start with only
  some namespaces, set `scope.includeNamespaces`. See
  [configuration.md](configuration.md#scope).
- [ ] **Label what full mode must never touch.** Use
  `reaper.boop.ninja/mode=off` to ignore something entirely, or
  `reaper.boop.ninja/mode=report` to keep reporting on it without ever
  acting. Either works on a namespace or a workload:

  ```sh
  kubectl label namespace payments reaper.boop.ninja/mode=report
  kubectl label deployment -n shop legacy-worker reaper.boop.ninja/mode=off
  ```

  For Flux-managed objects, put the label in Git, or Flux may remove it.
  Typical candidates: anything whose downtime is worse than its crash
  loop, such as an ingress controller, a payment path, or a singleton
  that recovers slowly.
- [ ] **Know your shared HelmReleases.** Reaping a workload rendered by a Flux
  HelmRelease suspends that whole release until the workload is restored.
  Every other object in the release keeps running but gets no upgrades or
  drift correction. See [flux.md](flux.md).
- [ ] **Set up alerts.** Enable the chart's ServiceMonitor and PrometheusRule
  ([helm-chart.md](helm-chart.md#servicemonitor-and-prometheusrule)), so
  `ReaperWorkloadReaped`, `ReaperActionFailed` and `ReaperStalled` reach
  someone.
- [ ] **Set up notifications.** Create the channel Secret and pick the
  reasons ([notifications.md](notifications.md)). During report mode it is
  useful to add `ReapPlanned` and `ReapSkipped` to `reasons`, so the team
  sees each new candidate. Take them out again before full mode if they
  get noisy.
- [ ] **Manage the ReaperConfig from Git.** With the chart, that means
  `reaperConfig.spec` in the HelmRelease values. A `kubectl patch` on a
  chart-managed ReaperConfig is put back at the next Helm upgrade
  ([helm-chart.md](helm-chart.md#6-the-reaperconfig)).

## 2. Install in report mode

Install with `reaperConfig.spec.mode: report`, the chart's default
([helm-chart.md](helm-chart.md#1-install-with-flux)). Then check it:

```sh
kubectl -n k8s-reaper get pods
kubectl get reaperconfig default
```

`VALID` must be `True` and `MODE` must be `report`.

### What the first cycle shows

Everything the reaper tracks carries the `reaper.boop.ninja/state` label:

```sh
kubectl get deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state
```

In the example, the first cycle showed two different clocks at work.

**CronJobs can be candidates at once.** The reaper reads a CronJob's history
from its Jobs. The failing CronJob had kept enough failed Jobs to show runs
failing for about 23 hours, well past `cronJobs.failingDuration` (6h) and
`cronJobs.minFailedRuns` (3), so it was a candidate in the first cycle. It can
only count Jobs that still exist: a CronJob that keeps one failed Job (the
Kubernetes default `failedJobsHistoryLimit: 1`) has to fail further runs
before it qualifies. See [cronjobs.md](cronjobs.md).

**Crash loops start counting when the reaper first sees them.** The reaper
keeps no history of pods from before it was running. A Deployment that had
crash-looped for days, with thousands of restarts, is recorded as `observed`
with `reaper.boop.ninja/crashloop-since` set to the first cycle. It becomes a
candidate `detection.crashLoopDuration` later (30m by default). The restart
counts already met `minRestarts` (10), so in the example all seven became
candidates 30 minutes after install.

**Some failures are never labelled**, by design:

- `ImagePullBackOff` and `ErrImagePull` are not crash loops.
- `Pending` pods never started, so they cannot crash-loop.
- Pods of Jobs are not crash loops. CronJobs are judged by their Jobs'
  results instead.

Crash-looping pods with no supported owner, such as bare pods, are counted in
the `reaper_unowned_crashlooping_pods` metric only.

### Limits apply in report mode too

The plan is the same in both modes, limits included. With
`maxReapsPerCycle: 1`, report mode shows one candidate with a plan of
`actions` and a `ReapPlanned` Event. Every other candidate gets
`ReapSkipped` with `reason=RateLimited`, and a plan of the form
`{"skip":"RateLimited","would":[…]}`.

In the example, the CronJob had the oldest failing streak, so it held the one
`ReapPlanned`. When the Deployments became candidates, all seven showed
`RateLimited`. That is expected: read their `would` lists, which are exactly
what full mode will do to them.

`ReapCeilingReached` does not appear in report mode, because nothing is
reaped.

### Cost

In the example the reaper used about 1m CPU and 58Mi memory, and each cycle
took under 0.2 s. Memory is above the chart's 32Mi request, so set
`resources.requests.memory` to what you observe. Check cycle time with the
`reaper_cycle_duration_seconds` metric.

## 3. How long to stay in report mode

Stay until both of these are true:

- **Every workload you expect has had time to become a candidate.** That is
  at least `crashLoopDuration` (30m) after install for crash loops. For
  CronJobs it is at least `minFailedRuns` runs: three days for a daily job
  with the default of 3.
- **You have read every plan and nothing surprised you.** Each candidate is
  either something you want stopped, or now carries a `mode` label.

Days are better than hours. A week covers weekly jobs and a full cycle of
deploys, so you also see how often workloads recover on their own
(`CrashLoopRecovered`).

## 4. Check before you switch

### The config

```sh
kubectl get reaperconfig default
kubectl get reaperconfig default -o jsonpath='{.status.counts}' ; echo
kubectl get events -A --field-selector reason=ConfigInvalid
```

`VALID` must be `True`, and there should be no `ConfigInvalid` Events. A
`ConfigInvalid` Event on a workload means a `mode` label or a threshold
annotation was ignored.

### Every plan

List each candidate with its plan:

```sh
kubectl get deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state=candidate \
  -o jsonpath='{range .items[*]}{.kind}/{.metadata.namespace}/{.metadata.name}{"\t"}{.metadata.annotations.reaper\.boop\.ninja/plan}{"\n"}{end}'
```

For each one, check:

- **Is it really broken, and is stopping it better than leaving it?** If
  not, label it `reaper.boop.ninja/mode=report` or `off`.
- **Does the plan hold a HelmRelease?** `holdHelmRelease` in the plan means
  the whole release is suspended while this workload is reaped:

  ```sh
  kubectl get deploy,sts,cronjob -A -l reaper.boop.ninja/state=candidate \
    -o jsonpath='{range .items[*]}{.kind}/{.metadata.namespace}/{.metadata.name}{"\t"}{.metadata.annotations.reaper\.boop\.ninja/plan}{"\n"}{end}' |
    grep holdHelmRelease
  ```

- **Why is a candidate skipped?** A skip reason other than `RateLimited`
  (`PartiallyAvailable`, `ManagedByController`, `ManagedByAutoscaler`,
  `ReportOnlyScope`, `CannotScaleToZero`) is not acted on in full mode
  either. See [how-it-works.md](how-it-works.md#5-skip-reasons).

### The Events

```sh
kubectl get events -A --field-selector reason=ReapPlanned
kubectl get events -A --field-selector reason=ReapSkipped
```

The note on each lists the container, its restarts, how long it has been
crash-looping, and what full mode would do. Events expire (one hour by
default in Kubernetes), so the plan annotations are the record to rely on.

### The order

Full mode takes candidates in a fixed order: longest crash-looping first, then
by namespace and name. Workloads first seen in the same cycle share a
`crashloop-since`, so they go in namespace and name order. With the default
limits, the first five in that order are the ones that will be reaped.

## 5. Choose the limits

| Field                     | Default | Effect at the switch |
| ------------------------- | ------- | -------------------- |
| `cycleInterval`           | `60s`   | Time between cycles, and so between reaps. |
| `limits.maxReapsPerCycle` | `1`     | New reaps per cycle. The rest wait with `RateLimited`. |
| `limits.maxReaped`        | `5`     | No new reap while this many workloads are reaped or being reaped. The rest wait with `ReapCeilingReached`. |

Suspended CronJobs count toward both limits, like Deployments and
StatefulSets.

Keep the defaults for the first switch. One reap a minute gives you time to
look at each one, and the ceiling caps the damage if a plan was wrong. If
more candidates are waiting than `maxReaped` allows, either restore some of
the reaped ones once they are fixed, or raise `maxReaped` in a later commit
when you are happy with the first batch. Raising it does not speed anything
up: the waiting workloads are still reaped one per cycle.

## 6. Switch to full mode

Change the HelmRelease values in Git:

```yaml
  values:
    reaperConfig:
      spec:
        mode: full
```

Do not use `kubectl patch` when the chart manages the ReaperConfig. The patch
works until the next Helm upgrade of the release, which puts `report` back
without warning.

After Flux applies the release, the reaper picks the change up at the start of
its next cycle, with no restart:

```sh
flux reconcile helmrelease k8s-reaper -n k8s-reaper --with-source
kubectl get reaperconfig default
kubectl get events -n default --field-selector reason=ConfigApplied
```

`MODE` shows `full` once the ReaperConfig is updated. The `ConfigApplied`
Event means the reaper is using it.

## 7. Watch the first reaps

Watch the state labels. `kubectl get -w` only watches one resource type at a
time, so use one terminal per type:

```sh
kubectl get deploy -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state -w
kubectl get cronjob -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state -w
kubectl get sts -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state -w
```

Or refresh all three together:

```sh
watch -n 10 kubectl get deploy,sts,cronjob -A -l reaper.boop.ninja/state -L reaper.boop.ninja/state
```

And the Events:

```sh
kubectl get events -A --field-selector reason=Reaped -w
kubectl get events -A --field-selector reason=ReapFailed
kubectl get events -A --field-selector reason=HelmReleaseHeld
```

### What happened in the example

The switch was committed with seven Deployments and one CronJob as
candidates:

1. **About 20 seconds later**, on the next cycle, the CronJob was suspended.
   It had the oldest failing streak, so it was first in line. Its `Reaped`
   Event reads `suspended` instead of a replica count.
2. **Then one Deployment a minute**, in namespace and name order, because all
   seven had been first seen in the same cycle. Each was scaled to 0, its
   original replica count recorded in `reaper.boop.ninja/original-replicas`,
   and any Flux holds placed.
3. **At five reaped workloads**, the CronJob plus four Deployments, the
   ceiling was reached. The other three Deployments changed from
   `RateLimited` to `ReapSkipped` with `reason=ReapCeilingReached`, and stayed
   `candidate`. They wait there until a reaped workload is restored or
   `maxReaped` is raised. Then they are taken one per cycle again.

For each reap, check:

```sh
kubectl get deployment/<name> -n <namespace> -o yaml | grep -E 'replicas:|reaper\.boop\.ninja|reconcile'
kubectl events -n <namespace> --for deployment/<name>
```

A `ReapFailed` Event means a step failed. The reaper retries on the next cycle
and finishes the reap without repeating the steps already done.

## 8. Back out

### Stop new reaps

Set `reaperConfig.spec.mode: report` in Git. From the next cycle the reaper
starts no new reaps.

**Switching back to report mode does not restore anything.** Workloads already
reaped stay at 0 or suspended, and their Flux holds stay in place. A reap that
was interrupted halfway (`reaping`) is neither finished nor undone. Restore
them as in section 9. Restoring works in report mode.

If you need the brake before Flux can apply a commit, patch the object as
well:

```sh
kubectl patch reaperconfig default --type merge -p '{"spec":{"mode":"report"}}'
```

The patch takes effect on the next cycle. Commit the same change to Git right
away, or the next Helm upgrade puts `full` back.

### Keep one workload out

```sh
kubectl label deployment -n shop api reaper.boop.ninja/mode=report
```

A `report` label stops future reaps of that workload but does not restore it
if it is already reaped. `reaper.boop.ninja/mode=off` restores a reaped
workload first and then ignores it. Put either label in Git for a
Flux-managed workload.

## 9. Restore

One annotation undoes everything the reaper did to a workload:

```sh
kubectl annotate deployment/web -n shop reaper.boop.ninja/restore=now --overwrite
```

Restores are never rate-limited and work in either mode. To restore everything
that is reaped, see the loop in
[helm-chart.md](helm-chart.md#1-restore-everything).

A restored workload that still crash-loops starts over as `observed` and is
reaped again `crashLoopDuration` later. Fix it first, or label it
`reaper.boop.ninja/mode=report` while you work on it. For Flux-managed
workloads, a fix in Git only reaches the cluster after the restore. See
[restoring-a-workload.md](restoring-a-workload.md) for every way back.

## Next steps

- [monitoring.md](monitoring.md): what each alert means and what to do.
- [troubleshooting.md](troubleshooting.md): when something does not behave as
  this guide describes.
- [configuration.md](configuration.md): every field and its range.
