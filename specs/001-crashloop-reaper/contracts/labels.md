# Contract: labels

## Written by the reaper

| Label                    | On                                  | Values                                         |
| ------------------------ | ----------------------------------- | ---------------------------------------------- |
| `reaper.boop.ninja/state` | Deployment, StatefulSet, DaemonSet, CronJob | `observed`, `candidate`, `reaping`, `reaped`   |

It is absent when the reaper holds no state for the object. Meant for
selection:

```bash
kubectl get deploy,sts,ds,cronjob -A -l reaper.boop.ninja/state=reaped
kubectl get deploy,sts,ds,cronjob -A -l 'reaper.boop.ninja/state in (candidate,reaping,reaped)'
```

The label sits on the workload's own metadata, never on the pod template, so
setting it never triggers a rollout.

## Read by the reaper (set by people)

| Label (or annotation)   | On                    | Values           | Effect                                                                                 |
| ----------------------- | --------------------- | ---------------- | -------------------------------------------------------------------------------------- |
| `reaper.boop.ninja/mode` | Namespace or workload | `off`            | Ignored entirely: not observed, annotated or reported. Existing reaper state is cleared once; a `reaped` workload is restored first. |
|                         |                       | `report`         | Observed and reported, never acted on (`ReportOnlyScope`)                               |
|                         |                       | anything else    | Ignored, with a `ConfigInvalid`-style Warning Event on the object                       |

Effective mode = min(global, namespace, workload) with `off < report < full`.
There is deliberately no `full` value. A label can only lower the mode.

## Read by the reaper (set by Flux)

| Labels (both required)                                                      | Meaning                                       |
| --------------------------------------------------------------------------- | --------------------------------------------- |
| `kustomize.toolkit.fluxcd.io/name`, `kustomize.toolkit.fluxcd.io/namespace` | Applied by that Flux Kustomization            |
| `helm.toolkit.fluxcd.io/name`, `helm.toolkit.fluxcd.io/namespace`           | Rendered by that Flux HelmRelease             |

If only one of a pair is present, the object is treated as not Flux-managed for
that controller, and the Event message notes it.
