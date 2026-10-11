# Flux

This guide explains how k8s-reaper works with Flux: how it tells that Flux
manages a workload, what it changes so Flux does not undo a reap, and how a
restore puts Flux back exactly as it was. Read it if your workloads are
deployed by Flux Kustomizations or HelmReleases. The general model is in
[how it works](how-it-works.md), the `flux` settings are in
[configuration](configuration.md#flux), and the steps to fix a reaped
workload through Git are in
[restoring a workload](restoring-a-workload.md#5-fix-the-cause-with-flux).

## 1. Why the reaper holds Flux back

Flux keeps the cluster equal to Git. If the reaper only scaled a
Flux-managed Deployment to 0, the next reconcile would apply `spec.replicas`
from Git again, the pods would start and crash again, and the crash loop
would continue. So, before it scales, the reaper tells Flux to leave that
workload alone. It does this with Flux's own switches, and it records the
earlier values so it can put them back.

## 2. How it knows Flux manages a workload

Flux's controllers label the objects they apply. The reaper reads those
labels from the workload's metadata:

| Labels on the workload | Managed by |
| ---------------------- | ---------- |
| `kustomize.toolkit.fluxcd.io/name` and `kustomize.toolkit.fluxcd.io/namespace` | a Flux Kustomization |
| `helm.toolkit.fluxcd.io/name` and `helm.toolkit.fluxcd.io/namespace` | a Flux HelmRelease, `<namespace>/<name>` |

Both labels of a pair must be present. With only one of them, the reaper
treats the workload as not managed by that controller.

To see what the reaper will see:

```sh
kubectl get deployment/web -n shop --show-labels
```

The reaper does not read Kustomization objects, GitRepositories or any other
Flux source. It only reads and changes HelmReleases, and the workloads
themselves.

## 3. What it changes

When full mode reaps a Flux-managed workload, the plan contains one or both
of these steps, before the workload is scaled:

| Workload has | Step | What changes |
| ------------ | ---- | ------------ |
| the kustomize labels | `disableKustomizeReconcile` | The workload gets `kustomize.toolkit.fluxcd.io/reconcile: disabled`. kustomize-controller then skips that one object; the rest of the Kustomization keeps reconciling. |
| the helm labels | `holdHelmRelease` | The HelmRelease gets `spec.suspend: true`. helm-controller then skips the whole release. |

A plan for a workload that has both looks like this:

```json
{"actions":[{"recordReap":{"originalReplicas":2}},{"disableKustomizeReconcile":"Deployment/app/web"},{"holdHelmRelease":{"helmRelease":"helm/web","suspend":true,"disableReconcile":false}},{"scaleTo":{"replicas":0}},"markReaped"]}
```

### The Kustomization switch: an annotation on the object

The reaper sets `kustomize.toolkit.fluxcd.io/reconcile: disabled` on the
workload itself. It does not suspend the Kustomization, so every other object
that Kustomization applies keeps being reconciled.

Before it sets the annotation, it records the earlier value in
`reaper.boop.ninja/previous-reconcile` on the workload (`<none>` if there was
no annotation). If the annotation is already `disabled`, the reaper changes
nothing and records nothing, and a restore leaves it `disabled`.

### The HelmRelease switch: suspend

A HelmRelease renders the workload, so the reaper cannot hold back only one
object: it suspends the release. Every resource in that release is then
frozen, not only the reaped workload. The reaper records a `HelmReleaseHeld`
Event on the HelmRelease that says so.

On the HelmRelease it writes:

| Key | Value |
| --- | ----- |
| `spec.suspend` | `true`, only if the release was not already suspended. |
| `reaper.boop.ninja/held-for` | A sorted JSON list of the workloads holding it, e.g. `["Deployment/shop/api","Deployment/shop/worker"]`. |
| `reaper.boop.ninja/suspended-by-reaper` | `true` if the reaper suspended it, `false` if it was already suspended. Set once, by the first hold. |
| `kustomize.toolkit.fluxcd.io/reconcile` | `disabled`, only if the HelmRelease itself has both kustomize labels and the annotation is not already `disabled`. |
| `reaper.boop.ninja/previous-reconcile` | The earlier value of that annotation, when it was set. |

The reconcile annotation on the HelmRelease matters when a Kustomization
applies the HelmRelease. Without it, the next Kustomization reconcile would
apply `spec.suspend` from Git and resume the release. The plan shows this as
`"disableReconcile":true` inside `holdHelmRelease`.

The workload gets `reaper.boop.ninja/helmrelease: <namespace>/<name>` once
the hold is in place. Several workloads from one release share one hold; see
[shared HelmReleases](restoring-a-workload.md#6-shared-helmreleases).

### When a step is turned off

`flux.kustomize` and `flux.helmRelease` in the ReaperConfig turn these steps
off. A workload that needs a step that is off is never reaped: it is skipped
with `ReportOnlyScope`, because Flux would scale it straight back up.

The reconcile annotation on a HelmRelease is part of the HelmRelease hold. It
is set whenever `flux.helmRelease` is on, whatever `flux.kustomize` says.

### When the HelmRelease is missing

If the workload's helm labels name a HelmRelease that does not exist, the
hold step fails with a `ReapFailed` Event. The workload stays `reaping`, is
not scaled, and the step is retried every cycle. To stop it, restore the
workload, or fix or remove the stale labels.

## 4. How a restore undoes it

A restore undoes exactly what the reaper changed, and nothing else:

1. **Release the HelmRelease.** The workload is removed from `held-for`. If
   other workloads are still listed, the HelmRelease stays as it is. If the
   list is now empty:
   - `spec.suspend` is set to `false`, but only if the reaper suspended it
     (`suspended-by-reaper: true`) and it is still suspended. A release that
     was suspended before the reaper came stays suspended.
   - The HelmRelease's reconcile annotation goes back to its recorded value,
     or is removed if it was `<none>`.
   - The reaper's annotations are removed from the HelmRelease, and it
     records a `HelmReleaseReleased` Event.
2. **Bring the workload back**: scale it to `original-replicas`, or resume
   the CronJob. A manual scale-up has already done this, so it is skipped.
3. **Clear the workload's record.** The workload's reconcile annotation goes
   back to the value in `previous-reconcile` (removed if `<none>`). Every
   `reaper.boop.ninja/*` annotation the reaper wrote and the state label are
   removed.

Flux then picks the workload up again on its next reconcile, including any
fix you pushed while it was reaped. To apply it at once:

```sh
kubectl annotate deployment/web -n shop reaper.boop.ninja/restore=now --overwrite
flux reconcile kustomization <name> -n flux-system --with-source
```

Holds are also released without a restore when the workload they are held for
is deleted, or no longer carries the reaper's state. This happens on every
cycle, in report mode too.

## 5. Workloads not managed by Flux

A workload without a complete Flux label pair has no Flux steps. Full mode
scales it to 0 and marks it reaped.

If something else puts the replicas back, such as Argo CD, a CI job running
`kubectl apply`, or `helm upgrade` from a laptop, the reaper sees a reaped
workload with replicas above 0 and treats it as a manual scale-up: it clears
its state and records `Restored` with `manual-scale-up`. The workload then
starts from nothing; it has to crash for the full `crashLoopDuration` again
before it can be reaped. The reaper cannot hold back tools it does not know
about, so with them a reap may last only until their next sync.

## 6. A suspended parent Kustomization

The reaper never reads Kustomization objects, so a suspended Kustomization
changes nothing for it. A crash-looping workload in a suspended Kustomization
is reaped like any other, and its reconcile annotation is still set. When
someone resumes the Kustomization, kustomize-controller still skips the
reaped workload until it is restored.

The reaper never suspends or resumes a Kustomization either.

## 7. Resuming Flux by hand

The reaper notices a restore through the workload (the restore annotation,
replicas above 0, or a CronJob that is no longer suspended) and through a
HelmRelease it suspended being resumed.

- **`flux resume helmrelease`** on a release the reaper suspended
  (`suspended-by-reaper: true`) counts as a restore. On its next cycle the
  reaper restores every reaped workload held through that release, each
  with a `Restored` Event noted `manual-resume`, and then lets the release go
  with `HelmReleaseReleased`. It does not suspend or resume the release
  itself again. Resuming a release that was already suspended before the
  reaper held it (`suspended-by-reaper: false`) is not a restore.

  The reaper looks at the release once per cycle. A release resumed and then
  suspended again before the next cycle looks unchanged, so it is not seen
  as resumed, and its workloads stay reaped.
- **Removing `kustomize.toolkit.fluxcd.io/reconcile` by hand** lets
  kustomize-controller apply the workload again. If the manifest in Git sets
  `replicas`, that counts as a manual scale-up. If it does not, the workload
  stays at 0, and the reaper's later restore puts the annotation back from
  `previous-reconcile`.

Use the restore annotation instead. It releases every hold in one step. See
[restoring a workload](restoring-a-workload.md#2-restore-with-one-command).

## 8. `reconcileStrategy` and other Flux settings

The reaper does not read `reconcileStrategy`, `interval`, drift detection or
any other HelmRelease or Kustomization setting. While a HelmRelease is
suspended, helm-controller does nothing with it, whatever those settings
are; after the release, Flux behaves as it did before.

The only `reconcileStrategy` in these guides is the one in the
[getting started](getting-started.md#1-install-with-flux) install example,
where it controls how Flux upgrades the reaper's own chart.

## Next steps

- [Restoring a workload](restoring-a-workload.md): every way back, including
  shared HelmReleases.
- [CronJobs](cronjobs.md): the same Flux holds apply to paused CronJobs.
- [Configuration](configuration.md#flux): the `flux` settings.
