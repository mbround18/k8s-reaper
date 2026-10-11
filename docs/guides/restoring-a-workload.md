# Restoring a workload

In full mode, k8s-reaper stops a workload that keeps crashing. It scales a
Deployment or StatefulSet to 0, or suspends a CronJob. If Flux manages the
workload, it also pauses Flux for it, so that Flux does not start it again.
Everything it changed is written on the objects themselves, so you never have
to remember what it did.

To bring the workload back, run one command. The reaper reverses exactly what
it changed and nothing else.

Restore works in every mode, report mode included, and is never rate-limited.

## 1. Find what was reaped

The reaper labels every workload it has stopped:

```sh
kubectl get deploy,sts,cronjob -A -l reaper.boop.ninja/state=reaped
```

To see why it was stopped and what was changed, read its annotations and
Events:

```sh
kubectl get deployment/web -n shop -o yaml | grep -E 'replicas:|reaper\.boop\.ninja|reconcile'
kubectl events -n shop --for deployment/web
```

The `Reaped` Event lists the original replica count, the reason, and any Flux
holds that were placed.

## 2. Restore with one command

Set the restore annotation on the workload. Any non-empty value works:

```sh
kubectl annotate deployment/web -n shop reaper.boop.ninja/restore=now --overwrite
```

The same command works for StatefulSets and CronJobs:

```sh
kubectl annotate statefulset/db -n shop reaper.boop.ninja/restore=now --overwrite
kubectl annotate cronjob/ingest -n shop reaper.boop.ninja/restore=now --overwrite
```

The reaper notices the annotation straight away and restores the workload.
Normally that takes a few seconds, and it never takes longer than one cycle
(`cycleInterval`, 60 s by default).

What restore puts back:

| The reaper changed                                          | Restore does                                                                                              |
| ----------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Scaled the workload to 0                                    | Scales it back to the recorded original replica count                                                    |
| Suspended a CronJob (`spec.suspend: true`)                  | Sets `spec.suspend: false`                                                                                |
| Set `kustomize.toolkit.fluxcd.io/reconcile: disabled`       | Puts back the value it had before. If there was none, the annotation is removed                           |
| Suspended the owning HelmRelease                            | Resumes it, but only if the reaper suspended it and no other reaped workload still needs it (section 6)   |
| Disabled kustomize reconcile on that HelmRelease            | Puts back its previous value, when the HelmRelease is released                                            |
| Added `reaper.boop.ninja/*` annotations and the state label | Removes all of them, the `restore` annotation included                                                   |

Restore never touches anything the reaper did not change:

- **A HelmRelease you suspended yourself** before the reap stays suspended.
- **A reconcile annotation you set yourself** is put back as it was, not
  removed.
- **An HPA** on the workload resumes once the replicas are back.

When the restore is done, the reaper records a `Restored` Event on the
workload. Its note starts with `requested` and lists what was undone, for
example:

```
requested; 3 replicas; HelmRelease hold released; Kustomization reconcile restored
```

Check the result:

```sh
kubectl get deployment/web -n shop -o yaml | grep -E 'replicas:|reaper\.boop\.ninja|reconcile'
# replicas back to the original; no reaper.boop.ninja/* keys left
kubectl events -n shop --for deployment/web
```

## 3. Scale it up by hand instead

If you scale a reaped Deployment or StatefulSet above 0 yourself, the reaper
treats that as a restore. It keeps the replica count you chose:

```sh
kubectl scale deployment/web -n shop --replicas=1
```

The reaper then does everything else a restore does. It releases its Flux
holds, puts the reconcile annotation back, and removes its annotations and
label. The `Restored` Event's note starts with `manual-scale-up`.

Re-applying the workload's manifest with `kubectl apply` counts too, if the
manifest sets `replicas` above 0.

Once Flux reconciliation is back on, Flux applies what Git declares. If your
manifests set `replicas`, Flux may change the count you chose back to that
value.

> A scale-up only counts once the workload is `reaped`. If the reaper was
> interrupted halfway through a reap, the state label reads `reaping` and the
> reaper finishes the reap on its next cycle. To stop a reap in that state,
> use the restore annotation from section 2. It works in both states.

## 4. Resume a CronJob by hand

Setting `spec.suspend: false` on a CronJob the reaper suspended counts as a
restore, just like a manual scale-up:

```sh
kubectl patch cronjob/ingest -n shop -p '{"spec":{"suspend":false}}'
```

The reaper releases its Flux holds and clears its state. The `Restored` Event's
note starts with `manual-resume`.

The restore annotation from section 2 does the same thing and also sets
`spec.suspend: false` for you.

CronJobs that are already suspended can be restored even when
`cronJobs.enabled` is `false`.

## 5. Fix the cause with Flux

While a Flux-managed workload is reaped, a fix you push to Git does not reach
it:

- The workload carries `kustomize.toolkit.fluxcd.io/reconcile: disabled`, so
  kustomize-controller skips it.
- If a HelmRelease renders it, that HelmRelease is suspended, so
  helm-controller skips the whole release. If a Kustomization applies the
  HelmRelease, the HelmRelease carries the reconcile annotation too, so a
  change to the HelmRelease in Git (new values, a new chart version) is not
  applied either.

A fix therefore lands only after a restore. Either order works:

**Restore first, then fix.** Restore the workload. It starts again with the
broken version and Flux is active again. Push the fix, and Flux applies it on
its next reconcile.

**Fix first, then restore.** Push the fix. Nothing changes in the cluster yet,
because the hold is still in place. Then restore, and Flux applies the fix on
its next reconcile.

What does not work is pushing a fix and waiting. The fix stays out of the
cluster until you restore.

To apply the fix without waiting for Flux's interval, reconcile after the
restore:

```sh
kubectl annotate deployment/web -n shop reaper.boop.ninja/restore=now --overwrite
flux reconcile kustomization <name> -n flux-system --with-source
# or, for a Helm-managed workload:
flux reconcile helmrelease <name> -n <namespace> --with-source
```

The broken version may crash for a short time until the fix is applied. That is
safe: after a restore the reaper starts watching the workload from scratch, and
the full `crashLoopDuration` must pass before it can be reaped again
(section 7).

Use the restore annotation, not `flux resume`, and do not remove the reconcile
annotation by hand. The reaper's records are only cleared, and a shared
HelmRelease's hold only released, when the workload itself is restored.

## 6. Shared HelmReleases

One HelmRelease often renders several workloads. When the reaper stops one of
them, it suspends the whole HelmRelease. The other workloads keep running,
but helm-controller stops managing every resource in the release: no
upgrades, no new values, no drift correction. The reaper records a
`HelmReleaseHeld` Event on the HelmRelease that says so:

```sh
kubectl events -n shop --for helmrelease/shop
```

The reaper does not skip a workload because its HelmRelease also renders
healthy workloads. It suspends the release anyway.

The HelmRelease records which workloads it is held for:

```sh
kubectl get helmrelease/shop -n shop \
  -o jsonpath='{.metadata.annotations.reaper\.boop\.ninja/held-for}'
# ["Deployment/shop/api","Deployment/shop/worker"]
```

The hold works like a reference count:

- Each reaped workload from the release is added to `held-for`.
- Restoring one workload removes it from `held-for`. That workload's replicas
  and reconcile annotation come back, but the HelmRelease **stays suspended**
  while any other workload in the list is still reaped.
- Restoring the last workload in the list resumes the HelmRelease, if the
  reaper was the one that suspended it. It also puts the HelmRelease's
  reconcile annotation back and removes the reaper's annotations from it. The
  reaper records a `HelmReleaseReleased` Event.

So a chart fix rolls out only once every workload in `held-for` is restored.
To release the HelmRelease, restore each workload listed:

```sh
kubectl annotate deployment/api -n shop reaper.boop.ninja/restore=now --overwrite
kubectl annotate deployment/worker -n shop reaper.boop.ninja/restore=now --overwrite
```

If a workload in `held-for` is deleted, or is recreated without the reaper's
annotations, the reaper removes it from the list on its next cycle. It resumes
the HelmRelease if nothing else holds it.

## 7. If it starts crashing again

A restore clears everything the reaper recorded about the workload, apart from
a CronJob's last counted Job (below). The reaper keeps no memory of earlier reaps, so a second reap is not treated any
differently from the first.

For a Deployment or StatefulSet that crash-loops again after a restore:

1. The reaper records it as `observed`, with the crash loop starting now.
2. It becomes a candidate only after the full `crashLoopDuration` (30 minutes
   by default) and once a container reaches `minRestarts` (10 by default).
3. In full mode it is then reaped again, subject to the usual limits
   (`maxReapsPerCycle`, `maxReaped`).

The `recoveryWindow` (10 minutes by default) only affects a workload that is
being watched but has not been reaped. Its crash loop is forgotten once it has
stayed healthy for that long. A restored workload has nothing left to forget.

For a CronJob, the failing streak starts again from zero. Jobs that failed
before the restore are not counted again: the reaper keeps
`reaper.boop.ninja/last-failed-job` set to the newest of them, and counts only
Jobs that fail after it. The CronJob is suspended again only once those new
failures meet `cronJobs.failingDuration` and `cronJobs.minFailedRuns`. Fix the
cause before you restore it, or right after.

After a restore, to stop the reaper acting on a workload while you work on it,
label it:

```sh
kubectl label deployment/web -n shop reaper.boop.ninja/mode=report
```

The reaper still reports the workload but never stops it. Remove the label
when you are done. A `report` label does not restore a workload that is already
reaped. Setting `reaper.boop.ninja/mode=off` on a reaped workload
restores it first, then the reaper ignores it entirely.

## 8. When a restore does not happen

| What you see                     | Meaning                                                                                                                                                          |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `RestoreIgnored` Event           | The workload is not reaped, so there is nothing to restore. The reaper removes the annotation and changes nothing else.                                          |
| `RestoreFailed` Event            | A step failed, for example an API error or a conflicting update. The note gives the error. The workload keeps its state and the reaper retries on the next cycle.      |
| The annotation is still there    | The reaper has not run a cycle yet, or it is not running. Check its pod and logs.                                                                                |

A restore that stops halfway is safe to retry. Each step checks what is already
done, so the next cycle completes the remaining steps without repeating any.
